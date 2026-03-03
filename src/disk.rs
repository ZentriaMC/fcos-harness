use std::path::Path;

use eyre::{Context, bail};
use tokio::process::Command;
use tracing::info;

/// Create a qcow2 copy-on-write overlay backed by `base`.
pub async fn create_overlay(
    base: &Path,
    overlay: &Path,
    size: &str,
    backing_format: &str,
) -> eyre::Result<()> {
    info!(
        base = %base.display(),
        overlay = %overlay.display(),
        size,
        backing_format,
        "creating qcow2 overlay"
    );

    if let Some(parent) = overlay.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let output = Command::new("qemu-img")
        .args([
            "create",
            "-f",
            "qcow2",
            "-b",
            &base.display().to_string(),
            "-F",
            backing_format,
            &overlay.display().to_string(),
            size,
        ])
        .output()
        .await
        .wrap_err("failed to execute qemu-img")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("qemu-img create failed: {stderr}");
    }

    Ok(())
}

/// Check if a QEMU snapshot with the given name exists in the disk image.
pub async fn snapshot_exists(disk: &Path, name: &str) -> eyre::Result<bool> {
    let output = Command::new("qemu-img")
        .args(["snapshot", "-l", &disk.display().to_string()])
        .output()
        .await
        .wrap_err("failed to execute qemu-img snapshot -l")?;

    if !output.status.success() {
        return Ok(false);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.contains(name))
}
