use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use tracing::info;

use crate::ssh::{SshConfig, SshSession};

#[derive(Args)]
pub struct SshArgs {
    /// Command to run on the VM.
    #[arg(trailing_var_arg = true, required = true)]
    pub command: Vec<String>,

    /// SSH port.
    #[arg(long, env = "TEST_SSH_PORT", default_value = "2223")]
    pub ssh_port: u16,

    /// SSH private key.
    #[arg(long)]
    pub ssh_key: PathBuf,

    /// SSH user.
    #[arg(long, default_value = "core")]
    pub user: String,

    /// Wait for SSH to become reachable (timeout in seconds, 0 = no wait).
    #[arg(long, default_value = "0")]
    pub wait: u64,
}

pub async fn run(args: SshArgs) -> eyre::Result<()> {
    let session = SshSession::new(SshConfig {
        port: args.ssh_port,
        user: args.user,
        identity_file: args.ssh_key,
        ..SshConfig::default()
    });

    if args.wait > 0 {
        let elapsed = session
            .wait_ready(Duration::from_secs(args.wait), Duration::from_secs(5))
            .await?;
        info!(elapsed, "SSH ready");
    }

    let cmd = args.command.join(" ");
    let output = session.exec_ok(&cmd).await?;

    if !output.stdout.is_empty() {
        print!("{}", output.stdout);
    }
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
    }

    Ok(())
}
