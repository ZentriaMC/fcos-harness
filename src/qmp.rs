use std::path::PathBuf;

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
