use std::path::{Path, PathBuf};
use std::time::Duration;

use eyre::{Context, bail};
use tracing::info;

use crate::arch::Arch;
use crate::ssh::SshSession;

const DEFAULT_GOSS_VERSION: &str = "v0.4.9";

/// Goss binary manager and test runner.
pub struct Goss {
    version: String,
    cache_dir: PathBuf,
    arch: Arch,
}

impl Goss {
    pub fn new(cache_dir: impl Into<PathBuf>, arch: Arch) -> Self {
        Self {
            version: DEFAULT_GOSS_VERSION.into(),
            cache_dir: cache_dir.into(),
            arch,
        }
    }

    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    fn binary_name(&self) -> String {
        format!("goss-linux-{}", self.arch.goss_arch())
    }

    fn download_url(&self) -> String {
        format!(
            "https://github.com/goss-org/goss/releases/download/{}/{}",
            self.version,
            self.binary_name()
        )
    }

    /// Ensure the goss binary is downloaded and cached locally.
    pub async fn ensure_binary(&self) -> eyre::Result<PathBuf> {
        let bin_path = self.cache_dir.join(self.binary_name());
        if bin_path.exists() {
            return Ok(bin_path);
        }

        tokio::fs::create_dir_all(&self.cache_dir).await?;

        let url = self.download_url();
        info!(url, "downloading goss");

        let client = reqwest::Client::new();
        let bytes = client
            .get(&url)
            .send()
            .await
            .wrap_err("failed to download goss")?
            .bytes()
            .await
            .wrap_err("failed to read goss binary")?;

        tokio::fs::write(&bin_path, &bytes).await?;

        // Make executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&bin_path, perms)?;
        }

        Ok(bin_path)
    }

    /// Deploy goss to the VM and run the given gossfile.
    pub async fn validate(
        &self,
        ssh: &SshSession,
        gossfile: &Path,
        retry_timeout: Duration,
        sleep_interval: Duration,
    ) -> eyre::Result<()> {
        let bin_path = self.ensure_binary().await?;

        info!("deploying goss to VM");
        ssh.upload(&bin_path, "/tmp/goss").await?;
        ssh.upload(gossfile, "/tmp/goss.yaml").await?;

        let cmd = format!(
            "/tmp/goss --gossfile /tmp/goss.yaml validate --retry-timeout {}s --sleep {}s",
            retry_timeout.as_secs(),
            sleep_interval.as_secs(),
        );

        info!("running goss validation");
        let output = ssh.exec(&cmd).await?;
        if output.exit_code != 0 {
            bail!(
                "goss validation failed (exit {})\nstdout: {}\nstderr: {}",
                output.exit_code,
                output.stdout,
                output.stderr,
            );
        }

        info!("goss validation passed");
        Ok(())
    }
}
