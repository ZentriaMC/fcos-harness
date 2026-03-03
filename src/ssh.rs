use std::path::{Path, PathBuf};
use std::time::Duration;

use eyre::{Context, bail};
use tokio::process::Command;
use tracing::{debug, info};

/// SSH connection configuration.
#[derive(Debug, Clone)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub identity_file: PathBuf,
    pub connect_timeout: Duration,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 2223,
            user: "core".into(),
            identity_file: PathBuf::new(),
            connect_timeout: Duration::from_secs(5),
        }
    }
}

/// Output from an SSH command execution.
#[derive(Debug)]
pub struct SshOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// SSH session to a running VM.
pub struct SshSession {
    config: SshConfig,
}

impl SshSession {
    pub fn new(config: SshConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &SshConfig {
        &self.config
    }

    /// Common SSH args shared between ssh and scp.
    fn common_args(&self) -> Vec<String> {
        vec![
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            "UserKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.config.connect_timeout.as_secs()),
            "-i".into(),
            self.config.identity_file.display().to_string(),
        ]
    }

    fn ssh_args(&self) -> Vec<String> {
        let mut args = self.common_args();
        args.push("-p".into());
        args.push(self.config.port.to_string());
        args
    }

    fn scp_args(&self) -> Vec<String> {
        let mut args = self.common_args();
        args.push("-P".into());
        args.push(self.config.port.to_string());
        args
    }

    fn remote_dest(&self) -> String {
        format!("{}@{}", self.config.user, self.config.host)
    }

    /// Poll until SSH is reachable, with timeout and interval.
    /// Returns the approximate elapsed time in seconds.
    pub async fn wait_ready(&self, timeout: Duration, interval: Duration) -> eyre::Result<u64> {
        let start = tokio::time::Instant::now();
        let deadline = start + timeout;

        loop {
            match self.exec("true").await {
                Ok(output) if output.exit_code == 0 => {
                    let elapsed = start.elapsed().as_secs();
                    info!(elapsed_secs = elapsed, "SSH is ready");
                    return Ok(elapsed);
                }
                _ => {}
            }

            if tokio::time::Instant::now() + interval > deadline {
                bail!("SSH not reachable after {}s", timeout.as_secs());
            }

            tokio::time::sleep(interval).await;
        }
    }

    /// Execute a command on the remote host.
    pub async fn exec(&self, command: &str) -> eyre::Result<SshOutput> {
        let args = self.ssh_args();
        let dest = self.remote_dest();

        debug!(command, "ssh exec");

        let output = Command::new("ssh")
            .args(&args)
            .arg(&dest)
            .arg(command)
            .output()
            .await
            .wrap_err("failed to execute ssh")?;

        Ok(SshOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Execute a command, returning an error if exit code is non-zero.
    pub async fn exec_ok(&self, command: &str) -> eyre::Result<SshOutput> {
        let output = self.exec(command).await?;
        if output.exit_code != 0 {
            bail!(
                "command failed (exit {}): {}\nstdout: {}\nstderr: {}",
                output.exit_code,
                command,
                output.stdout.trim(),
                output.stderr.trim(),
            );
        }
        Ok(output)
    }

    /// SCP a local file to the remote host.
    pub async fn upload(&self, local: &Path, remote: &str) -> eyre::Result<()> {
        let args = self.scp_args();
        let dest = format!("{}:{remote}", self.remote_dest());

        debug!(
            local = %local.display(),
            remote,
            "scp upload"
        );

        let output = Command::new("scp")
            .args(&args)
            .arg(local)
            .arg(&dest)
            .output()
            .await
            .wrap_err("failed to execute scp")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("scp upload failed: {stderr}");
        }

        Ok(())
    }

    /// SCP a remote file to the local host.
    pub async fn download(&self, remote: &str, local: &Path) -> eyre::Result<()> {
        let args = self.scp_args();
        let src = format!("{}:{remote}", self.remote_dest());

        debug!(
            remote,
            local = %local.display(),
            "scp download"
        );

        let output = Command::new("scp")
            .args(&args)
            .arg(&src)
            .arg(local)
            .output()
            .await
            .wrap_err("failed to execute scp")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("scp download failed: {stderr}");
        }

        Ok(())
    }
}
