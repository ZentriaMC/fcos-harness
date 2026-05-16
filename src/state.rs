use std::path::{Path, PathBuf};

use eyre::Context;
use serde::{Deserialize, Serialize};

const STATE_FILE: &str = "vm-state.json";

/// Connection metadata for the most recently launched VM in a work dir.
///
/// Written by `fh up` and consumed by `fh ssh` so the right SSH endpoint is
/// used without forcing consumers to track host/port themselves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmState {
    pub backend: String,
    pub host: String,
    pub port: u16,
    pub user: String,
}

impl VmState {
    pub fn path(work_dir: &Path) -> PathBuf {
        work_dir.join(STATE_FILE)
    }

    pub async fn read(work_dir: &Path) -> eyre::Result<Option<Self>> {
        let path = Self::path(work_dir);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let state = serde_json::from_str(&content)
                    .wrap_err_with(|| format!("invalid VM state at {}", path.display()))?;
                Ok(Some(state))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).wrap_err_with(|| format!("failed to read {}", path.display())),
        }
    }

    pub async fn write(&self, work_dir: &Path) -> eyre::Result<()> {
        let path = Self::path(work_dir);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let content =
            serde_json::to_string_pretty(self).wrap_err("failed to serialize VM state")?;
        tokio::fs::write(&path, content)
            .await
            .wrap_err_with(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    pub async fn remove(work_dir: &Path) -> eyre::Result<()> {
        tokio::fs::remove_file(Self::path(work_dir)).await.ok();
        Ok(())
    }
}
