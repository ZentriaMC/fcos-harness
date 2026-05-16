use std::process::ExitStatus;

use async_trait::async_trait;
use clap::ValueEnum;

use crate::ssh::{SshConfig, SshSession};

/// Selects which hypervisor backend to use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum BackendKind {
    /// QEMU (cross-platform, default).
    #[default]
    Qemu,
    /// vfkit (macOS Virtualization.framework; aarch64-darwin only).
    Vfkit,
}

impl BackendKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Qemu => "qemu",
            Self::Vfkit => "vfkit",
        }
    }
}

/// A running VM, agnostic to the underlying hypervisor.
///
/// Implementations cover lifecycle (shutdown/wait/detach), serial log access,
/// and the SSH connection. Snapshot semantics differ enough between backends
/// (QMP `savevm` vs warmed-disk clone) that snapshot handling lives in the
/// orchestration layer (`cli::up`) rather than on the trait.
#[async_trait]
pub trait Backend: Send {
    /// Build an SSH session for this VM.
    fn ssh(&self) -> SshSession;

    /// Get the SSH config (host/port/user/identity).
    fn ssh_config(&self) -> &SshConfig;

    /// Get the underlying VM process ID, if available.
    fn pid(&self) -> Option<u32>;

    /// Read the tail of the serial log (useful for debugging boot failures).
    async fn serial_tail(&self, lines: usize) -> eyre::Result<String>;

    /// Gracefully terminate the VM and wait for the process to exit.
    async fn shutdown(&mut self) -> eyre::Result<()>;

    /// Wait for the underlying process to exit.
    async fn wait(&mut self) -> eyre::Result<ExitStatus>;

    /// Detach from the VM process so `Drop` won't kill it.
    /// Used by `up` to keep the VM running after the CLI exits.
    fn detach(self: Box<Self>);
}
