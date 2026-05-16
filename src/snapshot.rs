use std::path::{Path, PathBuf};

use eyre::Context;

/// What the snapshot artifact is, and how to verify its existence.
#[derive(Debug, Clone)]
pub enum SnapshotKind {
    /// Named snapshot stored inside a qcow2 image (QEMU's savevm/loadvm).
    /// Verified via `qemu-img snapshot -l`.
    QcowInternal { name: String },
    /// External warmed-disk file (vfkit's cold-boot acceleration).
    /// Verified via file existence.
    ExternalDisk { warmed_path: PathBuf },
}

/// Manages snapshot caching with hash-based invalidation.
///
/// Pattern: hash the ignition config, save a snapshot artifact after first
/// boot (+ optional validation), and reuse it until the hash changes. The
/// shape of the artifact is backend-specific (see [`SnapshotKind`]).
pub struct SnapshotCache {
    disk_path: PathBuf,
    hash_file: PathBuf,
    kind: SnapshotKind,
}

impl SnapshotCache {
    pub fn new(
        disk_path: impl Into<PathBuf>,
        hash_file: impl Into<PathBuf>,
        kind: SnapshotKind,
    ) -> Self {
        Self {
            disk_path: disk_path.into(),
            hash_file: hash_file.into(),
            kind,
        }
    }

    /// Convenience constructor for qcow2 internal snapshots.
    pub fn qcow_internal(
        disk_path: impl Into<PathBuf>,
        hash_file: impl Into<PathBuf>,
        snapshot_name: impl Into<String>,
    ) -> Self {
        Self::new(
            disk_path,
            hash_file,
            SnapshotKind::QcowInternal {
                name: snapshot_name.into(),
            },
        )
    }

    /// Convenience constructor for external warmed-disk snapshots.
    pub fn external_disk(
        disk_path: impl Into<PathBuf>,
        hash_file: impl Into<PathBuf>,
        warmed_path: impl Into<PathBuf>,
    ) -> Self {
        Self::new(
            disk_path,
            hash_file,
            SnapshotKind::ExternalDisk {
                warmed_path: warmed_path.into(),
            },
        )
    }

    /// The live/active disk path (overlay or clone).
    pub fn disk_path(&self) -> &Path {
        &self.disk_path
    }

    /// The snapshot name (only meaningful for qcow2 internal snapshots).
    pub fn snapshot_name(&self) -> Option<&str> {
        match &self.kind {
            SnapshotKind::QcowInternal { name } => Some(name),
            SnapshotKind::ExternalDisk { .. } => None,
        }
    }

    /// The warmed-disk path (only meaningful for external snapshots).
    pub fn warmed_path(&self) -> Option<&Path> {
        match &self.kind {
            SnapshotKind::QcowInternal { .. } => None,
            SnapshotKind::ExternalDisk { warmed_path } => Some(warmed_path),
        }
    }

    pub fn kind(&self) -> &SnapshotKind {
        &self.kind
    }

    /// Check if a valid snapshot exists for the given content hash.
    pub async fn is_valid(&self, current_hash: &str) -> eyre::Result<bool> {
        // Check that the hash file matches.
        let stored_hash = match tokio::fs::read_to_string(&self.hash_file).await {
            Ok(h) => h,
            Err(_) => return Ok(false),
        };
        if stored_hash.trim() != current_hash {
            return Ok(false);
        }

        match &self.kind {
            SnapshotKind::QcowInternal { name } => {
                if !self.disk_path.exists() {
                    return Ok(false);
                }
                crate::disk::snapshot_exists(&self.disk_path, name).await
            }
            SnapshotKind::ExternalDisk { warmed_path } => Ok(warmed_path.exists()),
        }
    }

    /// Record the hash after saving a snapshot.
    pub async fn record(&self, hash: &str) -> eyre::Result<()> {
        tokio::fs::write(&self.hash_file, hash)
            .await
            .wrap_err("failed to write snapshot hash file")?;
        Ok(())
    }

    /// Clean up stale snapshot files for a fresh start.
    ///
    /// For qcow2 internal snapshots, removes the disk overlay (which contains
    /// the stale snapshot) and the hash file.
    /// For external snapshots, removes the warmed-disk file and the hash file
    /// — the live disk is freshly cloned each run, so we leave it.
    pub async fn invalidate(&self) -> eyre::Result<()> {
        tokio::fs::remove_file(&self.hash_file).await.ok();
        match &self.kind {
            SnapshotKind::QcowInternal { .. } => {
                tokio::fs::remove_file(&self.disk_path).await.ok();
            }
            SnapshotKind::ExternalDisk { warmed_path } => {
                tokio::fs::remove_file(warmed_path).await.ok();
            }
        }
        Ok(())
    }
}
