use std::path::{Path, PathBuf};

use clap::ValueEnum;
use eyre::{Context, bail};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::info;

use crate::arch::Arch;

const FCOS_BUILDS_URL: &str = "https://builds.coreos.fedoraproject.org/streams";

/// Which FCOS image variant to download and use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ImageVariant {
    /// Standard qemu artifact, qcow2.xz format.
    #[default]
    Qemu,
    /// Metal artifact, 4k.raw.xz format (4K block size disk).
    #[value(name = "metal4k")]
    Metal4k,
}

impl ImageVariant {
    /// The artifact key in the FCOS stream JSON.
    pub fn artifact(&self) -> &'static str {
        match self {
            Self::Qemu => "qemu",
            Self::Metal4k => "metal",
        }
    }

    /// The format key in the FCOS stream JSON.
    pub fn format_key(&self) -> &'static str {
        match self {
            Self::Qemu => "qcow2.xz",
            Self::Metal4k => "4k.raw.xz",
        }
    }

    /// Filename for the cached decompressed image.
    pub fn cached_filename(&self) -> &'static str {
        match self {
            Self::Qemu => "fcos.qcow2",
            Self::Metal4k => "fcos-4k.raw",
        }
    }

    /// The qemu-img backing format string (`-F` flag).
    pub fn backing_format(&self) -> &'static str {
        match self {
            Self::Qemu => "qcow2",
            Self::Metal4k => "raw",
        }
    }
}

/// FCOS stream name (e.g. "stable", "testing", "next").
#[derive(Debug, Clone)]
pub struct FcosStream(pub String);

impl Default for FcosStream {
    fn default() -> Self {
        Self("next".into())
    }
}

/// Resolved metadata from a FCOS stream.
struct StreamArtifact {
    version: String,
    url: String,
    sha256: String,
}

/// Manages downloading, verifying, and caching FCOS images.
///
/// Two modes depending on whether `cache_dir` is set:
/// - **With cache**: versioned storage in `{cache_dir}/images/{stream}/{arch}/{version}/`,
///   symlink in `work_dir` pointing to the active version. Repeated calls skip network
///   if the symlink is valid.
/// - **Without cache** (legacy): flat storage directly in `work_dir` (`work_dir/fcos.qcow2`).
///   If the file exists, no network call is made.
pub struct FcosImage {
    work_dir: PathBuf,
    cache_dir: Option<PathBuf>,
    stream: FcosStream,
    arch: Arch,
    variant: ImageVariant,
}

impl FcosImage {
    pub fn new(work_dir: impl Into<PathBuf>, arch: Arch) -> Self {
        Self {
            work_dir: work_dir.into(),
            cache_dir: None,
            stream: FcosStream::default(),
            arch,
            variant: ImageVariant::default(),
        }
    }

    pub fn cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(dir.into());
        self
    }

    pub fn stream(mut self, stream: impl Into<String>) -> Self {
        self.stream = FcosStream(stream.into());
        self
    }

    pub fn variant(mut self, variant: ImageVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn image_variant(&self) -> ImageVariant {
        self.variant
    }

    /// Versioned cache path: `{cache_dir}/images/{stream}/{arch}/{version}/{filename}`
    fn versioned_path(&self, cache_dir: &Path, version: &str) -> PathBuf {
        cache_dir
            .join("images")
            .join(&self.stream.0)
            .join(self.arch.as_str())
            .join(version)
            .join(self.variant.cached_filename())
    }

    /// Local image path in work_dir (e.g. `work_dir/fcos.qcow2`).
    fn local_path(&self) -> PathBuf {
        self.work_dir.join(self.variant.cached_filename())
    }

    /// Ensure the base image exists, downloading if necessary.
    pub async fn ensure(&self) -> eyre::Result<PathBuf> {
        match &self.cache_dir {
            Some(cache_dir) => self.ensure_cached(cache_dir).await,
            None => self.ensure_local().await,
        }
    }

    /// Force a fresh download of the latest version.
    pub async fn refresh(&self) -> eyre::Result<PathBuf> {
        match &self.cache_dir {
            Some(cache_dir) => self.refresh_cached(cache_dir).await,
            None => self.refresh_local().await,
        }
    }

    // --- Legacy (flat) mode: image stored directly in work_dir ---

    async fn ensure_local(&self) -> eyre::Result<PathBuf> {
        let dest = self.local_path();
        if dest.exists() {
            info!(path = %dest.display(), "FCOS image already present");
            return Ok(dest);
        }
        let artifact = self.fetch_metadata().await?;
        self.download(&artifact, &dest).await?;
        Ok(dest)
    }

    async fn refresh_local(&self) -> eyre::Result<PathBuf> {
        let dest = self.local_path();
        if dest.exists() {
            tokio::fs::remove_file(&dest).await?;
        }
        let artifact = self.fetch_metadata().await?;
        self.download(&artifact, &dest).await?;
        Ok(dest)
    }

    // --- Cached (versioned) mode: image in cache_dir, symlink in work_dir ---

    async fn ensure_cached(&self, cache_dir: &Path) -> eyre::Result<PathBuf> {
        let link = self.local_path();
        if link.exists() {
            // Regular file = per-project override or pre-existing image; use directly.
            let meta = tokio::fs::symlink_metadata(&link).await?;
            if !meta.is_symlink() {
                info!(path = %link.display(), "using local image (not a symlink)");
                return Ok(link);
            }
            // Valid symlink → no network
            let resolved = tokio::fs::canonicalize(&link)
                .await
                .wrap_err("failed to resolve image symlink")?;
            info!(path = %resolved.display(), "FCOS image already linked");
            return Ok(resolved);
        }

        let artifact = self.fetch_metadata().await?;
        let cached_image = self.versioned_path(cache_dir, &artifact.version);

        if cached_image.exists() {
            info!(
                path = %cached_image.display(),
                version = artifact.version,
                "FCOS image already cached",
            );
        } else {
            self.download(&artifact, &cached_image).await?;
        }

        self.update_symlink(&cached_image).await?;
        Ok(cached_image)
    }

    async fn refresh_cached(&self, cache_dir: &Path) -> eyre::Result<PathBuf> {
        let artifact = self.fetch_metadata().await?;
        let cached_image = self.versioned_path(cache_dir, &artifact.version);

        if cached_image.exists() {
            tokio::fs::remove_file(&cached_image).await?;
        }

        self.download(&artifact, &cached_image).await?;
        self.update_symlink(&cached_image).await?;
        Ok(cached_image)
    }

    async fn update_symlink(&self, target: &Path) -> eyre::Result<()> {
        let link = self.local_path();
        if let Some(parent) = link.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Remove stale symlink (or regular file from old layout)
        tokio::fs::remove_file(&link).await.ok();
        tokio::fs::symlink(target, &link).await.wrap_err_with(|| {
            format!(
                "failed to symlink {} -> {}",
                link.display(),
                target.display(),
            )
        })?;
        info!(
            link = %link.display(),
            target = %target.display(),
            "symlinked image",
        );
        Ok(())
    }

    // --- Shared helpers ---

    async fn fetch_metadata(&self) -> eyre::Result<StreamArtifact> {
        let stream_url = format!("{}/{}.json", FCOS_BUILDS_URL, self.stream.0);
        info!(url = stream_url, arch = %self.arch, "fetching FCOS stream metadata");

        let client = reqwest::Client::new();
        let metadata: serde_json::Value = client
            .get(&stream_url)
            .send()
            .await
            .wrap_err("failed to fetch FCOS stream metadata")?
            .json()
            .await
            .wrap_err("failed to parse FCOS stream JSON")?;

        let arch_str = self.arch.as_str();
        let artifact_key = self.variant.artifact();
        let format_key = self.variant.format_key();
        let artifact = &metadata["architectures"][arch_str]["artifacts"][artifact_key];

        let version = artifact["release"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("missing release version for {artifact_key}/{arch_str}"))?
            .to_string();

        let url = artifact["formats"][format_key]["disk"]["location"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("missing {format_key} location for {arch_str}"))?
            .to_string();

        let sha256 = artifact["formats"][format_key]["disk"]["sha256"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("missing {format_key} sha256 for {arch_str}"))?
            .to_string();

        info!(version, "resolved FCOS stream version");
        Ok(StreamArtifact {
            version,
            url,
            sha256,
        })
    }

    async fn download(&self, artifact: &StreamArtifact, dest: &Path) -> eyre::Result<()> {
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let compressed_path = dest.with_extension(format!(
            "{}.xz",
            dest.extension().unwrap_or_default().to_string_lossy()
        ));

        let client = reqwest::Client::new();
        let response = client
            .get(&artifact.url)
            .send()
            .await
            .wrap_err("failed to download FCOS image")?;

        let total = response.content_length().unwrap_or(0);

        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{msg} [{bar:40}] {bytes}/{total_bytes} ({eta})")
                .expect("valid template")
                .progress_chars("=> "),
        );
        pb.set_message("downloading");

        let mut file = tokio::fs::File::create(&compressed_path)
            .await
            .wrap_err("failed to create compressed image file")?;

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.wrap_err("error reading download stream")?;
            file.write_all(&chunk).await?;
            pb.inc(chunk.len() as u64);
        }

        pb.finish_with_message("downloaded");
        file.flush().await?;
        drop(file);

        // Verify SHA256
        info!("verifying SHA256 checksum");
        let actual_sha256 = sha256_of_file(&compressed_path).await?;
        if actual_sha256 != artifact.sha256 {
            bail!(
                "SHA256 mismatch: expected {}, got {actual_sha256}",
                artifact.sha256,
            );
        }

        // Decompress XZ
        info!("decompressing XZ image");
        decompress_xz(&compressed_path, dest).await?;

        // Clean up compressed file
        tokio::fs::remove_file(&compressed_path).await.ok();

        info!(path = %dest.display(), "FCOS image ready");
        Ok(())
    }
}

/// Stream SHA256 without buffering the entire file in memory.
async fn sha256_of_file(path: &Path) -> eyre::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::{BufReader, Read};

        let file = std::fs::File::open(&path)
            .wrap_err_with(|| format!("failed to open {}", path.display()))?;
        let mut reader = BufReader::new(file);
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await
    .wrap_err("sha256 task panicked")?
}

async fn decompress_xz(src: &Path, dest: &Path) -> eyre::Result<()> {
    let src = src.to_path_buf();
    let dest = dest.to_path_buf();

    tokio::task::spawn_blocking(move || {
        use std::io::{BufReader, BufWriter, Read, Write};

        let input = std::fs::File::open(&src)
            .wrap_err_with(|| format!("failed to open {}", src.display()))?;
        let reader = BufReader::new(input);
        let mut decoder = xz2::read::XzDecoder::new(reader);

        let output = std::fs::File::create(&dest)
            .wrap_err_with(|| format!("failed to create {}", dest.display()))?;
        let mut writer = BufWriter::new(output);

        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = decoder.read(&mut buf).wrap_err("xz decompression failed")?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
        }
        writer.flush()?;

        Ok::<(), eyre::Report>(())
    })
    .await
    .wrap_err("decompression task panicked")??;

    Ok(())
}
