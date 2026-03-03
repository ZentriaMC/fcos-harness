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

    /// Filename for the compressed download.
    fn compressed_filename(&self) -> &'static str {
        match self {
            Self::Qemu => "fcos.qcow2.xz",
            Self::Metal4k => "fcos-4k.raw.xz",
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

/// Manages downloading, verifying, and caching FCOS images.
pub struct FcosImage {
    cache_dir: PathBuf,
    stream: FcosStream,
    arch: Arch,
    variant: ImageVariant,
}

impl FcosImage {
    pub fn new(cache_dir: impl Into<PathBuf>, arch: Arch) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            stream: FcosStream::default(),
            arch,
            variant: ImageVariant::default(),
        }
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

    /// Ensure the base image exists, downloading if necessary.
    /// Returns the path to the uncompressed image file.
    pub async fn ensure(&self) -> eyre::Result<PathBuf> {
        let base_disk = self.cache_dir.join(self.variant.cached_filename());
        if base_disk.exists() {
            info!(path = %base_disk.display(), "FCOS image already cached");
            return Ok(base_disk);
        }
        self.download(&base_disk).await
    }

    /// Force a fresh download even if cached.
    pub async fn refresh(&self) -> eyre::Result<PathBuf> {
        let base_disk = self.cache_dir.join(self.variant.cached_filename());
        if base_disk.exists() {
            tokio::fs::remove_file(&base_disk).await?;
        }
        self.download(&base_disk).await
    }

    async fn download(&self, dest: &Path) -> eyre::Result<PathBuf> {
        tokio::fs::create_dir_all(&self.cache_dir).await?;

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

        let url = artifact["formats"][format_key]["disk"]["location"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("missing {format_key} location for {arch_str}"))?;

        let expected_sha256 = artifact["formats"][format_key]["disk"]["sha256"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("missing {format_key} sha256 for {arch_str}"))?;

        let compressed_path = self.cache_dir.join(self.variant.compressed_filename());

        // Stream download with progress bar
        let response = client
            .get(url)
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

        // Verify SHA256 (streaming, not buffered)
        info!("verifying SHA256 checksum");
        let actual_sha256 = sha256_of_file(&compressed_path).await?;
        if actual_sha256 != expected_sha256 {
            bail!("SHA256 mismatch: expected {expected_sha256}, got {actual_sha256}");
        }

        // Decompress XZ → qcow2
        info!("decompressing XZ image");
        decompress_xz(&compressed_path, dest).await?;

        // Clean up compressed file
        tokio::fs::remove_file(&compressed_path).await.ok();

        info!(path = %dest.display(), "FCOS image ready");
        Ok(dest.to_path_buf())
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
