use std::path::{Path, PathBuf};

use eyre::{Context, bail};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::{debug, info};

/// QMP (QEMU Machine Protocol) client over a Unix socket.
pub struct QmpClient {
    socket_path: PathBuf,
}

impl QmpClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    /// Connect, negotiate capabilities, and send a command.
    async fn send_command(&self, command: serde_json::Value) -> eyre::Result<serde_json::Value> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to connect to QMP socket: {}",
                    self.socket_path.display()
                )
            })?;

        let mut buf = vec![0u8; 65536];

        // Read QMP greeting
        let n = stream.read(&mut buf).await?;
        debug!(
            greeting = %String::from_utf8_lossy(&buf[..n]),
            "QMP greeting"
        );

        // Negotiate capabilities
        let caps = json!({"execute": "qmp_capabilities"});
        stream.write_all(format!("{caps}\n").as_bytes()).await?;
        let n = stream.read(&mut buf).await?;
        debug!(
            response = %String::from_utf8_lossy(&buf[..n]),
            "QMP capabilities response"
        );

        // Send the actual command
        let cmd_str = format!("{command}\n");
        debug!(command = %cmd_str.trim(), "QMP send");
        stream.write_all(cmd_str.as_bytes()).await?;

        // Read response (may fail for quit command, which is expected)
        match tokio::time::timeout(std::time::Duration::from_secs(120), stream.read(&mut buf)).await
        {
            Ok(Ok(n)) if n > 0 => {
                let response: serde_json::Value =
                    serde_json::from_slice(&buf[..n]).unwrap_or(json!({"return": {}}));

                if let Some(err) = response.get("error") {
                    bail!("QMP error: {err}");
                }

                Ok(response)
            }
            _ => {
                // Expected for quit command — connection drops
                Ok(json!({"return": {}}))
            }
        }
    }

    /// Save a VM snapshot with the given name using the human-monitor-command.
    pub async fn savevm(&self, name: &str) -> eyre::Result<()> {
        info!(name, "saving VM snapshot");
        let cmd = json!({
            "execute": "human-monitor-command",
            "arguments": {"command-line": format!("savevm {name}")}
        });
        self.send_command(cmd).await?;
        Ok(())
    }

    /// Quit QEMU cleanly.
    pub async fn quit(&self) -> eyre::Result<()> {
        info!("sending QMP quit");
        let cmd = json!({"execute": "quit"});
        // quit may drop the connection, so we ignore errors
        let _ = self.send_command(cmd).await;
        Ok(())
    }
}

/// Manages snapshot caching with hash-based invalidation.
///
/// Pattern: hash the ignition config, save a VM snapshot after boot + goss,
/// and reuse it until the hash changes.
pub struct SnapshotCache {
    disk_path: PathBuf,
    hash_file: PathBuf,
    snapshot_name: String,
}

impl SnapshotCache {
    pub fn new(
        disk_path: impl Into<PathBuf>,
        hash_file: impl Into<PathBuf>,
        snapshot_name: impl Into<String>,
    ) -> Self {
        Self {
            disk_path: disk_path.into(),
            hash_file: hash_file.into(),
            snapshot_name: snapshot_name.into(),
        }
    }

    pub fn disk_path(&self) -> &Path {
        &self.disk_path
    }

    pub fn snapshot_name(&self) -> &str {
        &self.snapshot_name
    }

    /// Check if a valid snapshot exists for the given content hash.
    pub async fn is_valid(&self, current_hash: &str) -> eyre::Result<bool> {
        // Check that the disk file exists
        if !self.disk_path.exists() {
            return Ok(false);
        }

        // Check that the hash file matches
        let stored_hash = match tokio::fs::read_to_string(&self.hash_file).await {
            Ok(h) => h,
            Err(_) => return Ok(false),
        };

        if stored_hash.trim() != current_hash {
            return Ok(false);
        }

        // Check that the snapshot actually exists in the disk image
        crate::disk::snapshot_exists(&self.disk_path, &self.snapshot_name).await
    }

    /// Record the hash after saving a snapshot.
    pub async fn record(&self, hash: &str) -> eyre::Result<()> {
        tokio::fs::write(&self.hash_file, hash)
            .await
            .wrap_err("failed to write snapshot hash file")?;
        Ok(())
    }

    /// Clean up stale snapshot files for a fresh start.
    pub async fn invalidate(&self) -> eyre::Result<()> {
        tokio::fs::remove_file(&self.disk_path).await.ok();
        tokio::fs::remove_file(&self.hash_file).await.ok();
        Ok(())
    }
}
