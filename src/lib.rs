pub mod arch;
pub mod backend;
pub mod cli;
pub mod disk;
pub mod fcos;
pub mod goss;
pub mod ignition;
pub mod qemu;
pub mod qmp;
pub mod snapshot;
pub mod ssh;
pub mod state;
pub mod vfkit;

pub use arch::{Arch, Platform};
pub use backend::{Backend, BackendKind};
pub use fcos::{FcosImage, ImageVariant};
pub use goss::Goss;
pub use ignition::{ButaneSource, IgnitionBuilder};
pub use qemu::{Vm, VmBuilder};
pub use qmp::QmpClient;
pub use snapshot::{SnapshotCache, SnapshotKind};
pub use ssh::{SshConfig, SshOutput, SshSession};
pub use state::VmState;

use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

/// Compute the SHA256 hex digest of a file.
pub async fn sha256_file(path: &Path) -> eyre::Result<String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|err| eyre::eyre!("failed to open {}: {err}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
