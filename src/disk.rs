use std::path::Path;

use eyre::{Context, bail};
use tokio::process::Command;
use tracing::{info, warn};

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

    // qemu-img resolves -b relative to the overlay's directory,
    // so canonicalize the base path to avoid double-nesting.
    let base = tokio::fs::canonicalize(base)
        .await
        .wrap_err_with(|| format!("failed to resolve base image path: {}", base.display()))?;

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

/// Result of `create_clone`: whether APFS clonefile succeeded or a full copy was used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneKind {
    /// APFS clonefile(2) succeeded — instant copy-on-write, ~0 disk usage.
    Cloned,
    /// Fallback: full byte-for-byte copy via `tokio::fs::copy`.
    Copied,
}

/// Clone a file into `dest` using APFS clonefile(2) on macOS, with a
/// transparent fallback to a regular copy when the underlying filesystem
/// doesn't support cloning (e.g. non-APFS volumes).
///
/// The destination is overwritten if it exists.
#[cfg(target_os = "macos")]
pub async fn create_clone(base: &Path, dest: &Path) -> eyre::Result<CloneKind> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    info!(
        base = %base.display(),
        dest = %dest.display(),
        "cloning disk image",
    );

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // clonefile fails if dest exists.
    tokio::fs::remove_file(dest).await.ok();

    let src_c =
        CString::new(base.as_os_str().as_bytes()).wrap_err("base path contains a NUL byte")?;
    let dst_c =
        CString::new(dest.as_os_str().as_bytes()).wrap_err("dest path contains a NUL byte")?;

    // SAFETY: both pointers are valid for the duration of the call and reference
    // null-terminated C strings; flags=0 means default behavior.
    let rc = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if rc == 0 {
        return Ok(CloneKind::Cloned);
    }

    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOTSUP) | Some(libc::EXDEV) => {
            warn!(
                ?err,
                "clonefile unsupported on this filesystem, copying instead"
            );
            tokio::fs::copy(base, dest).await.wrap_err_with(|| {
                format!("failed to copy {} to {}", base.display(), dest.display(),)
            })?;
            Ok(CloneKind::Copied)
        }
        _ => bail!("clonefile failed: {err}"),
    }
}

#[cfg(not(target_os = "macos"))]
pub async fn create_clone(_base: &Path, _dest: &Path) -> eyre::Result<CloneKind> {
    bail!("create_clone is only supported on macOS (uses APFS clonefile)")
}

#[cfg(all(test, target_os = "macos"))]
mod clone_tests {
    use super::*;

    #[tokio::test]
    async fn clone_smaller_temp_file() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("source.bin");
        let dst = dir.path().join("clone.bin");

        let payload: Vec<u8> = (0u8..=255).cycle().take(64 * 1024).collect();
        tokio::fs::write(&src, &payload).await?;

        let kind = create_clone(&src, &dst).await?;
        // The repo lives on APFS; we expect Cloned.
        assert_eq!(kind, CloneKind::Cloned);

        let read_back = tokio::fs::read(&dst).await?;
        assert_eq!(read_back, payload);

        Ok(())
    }
}
