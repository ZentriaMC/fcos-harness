use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use eyre::bail;
use tracing::info;

use crate::ssh::{SshConfig, SshSession};
use crate::state::VmState;

#[derive(Args)]
pub struct SshArgs {
    /// Command to run on the VM (omit when using --emit-opts).
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,

    /// SSH host. Defaults to the state file written by `fh up`, else 127.0.0.1.
    #[arg(long, env = "FCOS_HARNESS_SSH_HOST")]
    pub host: Option<String>,

    /// SSH port. Defaults to the state file written by `fh up`, else 2223.
    #[arg(long, env = "FCOS_HARNESS_SSH_PORT")]
    pub ssh_port: Option<u16>,

    /// SSH private key. Defaults to the state file written by `fh up`.
    #[arg(long, env = "FCOS_HARNESS_SSH_KEY")]
    pub ssh_key: Option<PathBuf>,

    /// SSH user. Defaults to the state file written by `fh up`, else "core".
    #[arg(long)]
    pub user: Option<String>,

    /// Wait for SSH to become reachable (timeout in seconds, 0 = no wait).
    #[arg(long, default_value = "0")]
    pub wait: u64,

    /// Print ssh/scp `-o` options for the current VM (one per line) and exit,
    /// instead of running a command. One-per-line so fish `(cmd)` and bash
    /// `$(cmd)` both auto-split correctly. Useful for embedding into scp/rsync,
    /// e.g. `scp (fh ssh --emit-opts) file dest:/path`.
    #[arg(long, conflicts_with = "command")]
    pub emit_opts: bool,
}

pub async fn run(args: SshArgs, work_dir: &Path) -> eyre::Result<()> {
    let state = VmState::read(work_dir).await?;

    let host = args
        .host
        .or_else(|| state.as_ref().map(|s| s.host.clone()))
        .unwrap_or_else(|| "127.0.0.1".into());
    let port = args
        .ssh_port
        .or_else(|| state.as_ref().map(|s| s.port))
        .unwrap_or(2223);
    let user = args
        .user
        .or_else(|| state.as_ref().map(|s| s.user.clone()))
        .unwrap_or_else(|| "core".into());
    let ssh_key = args
        .ssh_key
        .or_else(|| state.as_ref().and_then(|s| s.identity_file.clone()))
        .ok_or_else(|| {
            eyre::eyre!(
                "no SSH key available: pass --ssh-key, set FCOS_HARNESS_SSH_KEY, or run `fh up` first"
            )
        })?;

    if args.emit_opts {
        let opts = [
            format!("-oHostname={host}"),
            format!("-oPort={port}"),
            format!("-oUser={user}"),
            format!("-oIdentityFile={}", ssh_key.display()),
            "-oStrictHostKeyChecking=no".into(),
            "-oUserKnownHostsFile=/dev/null".into(),
            "-oLogLevel=ERROR".into(),
            "-oConnectTimeout=5".into(),
        ];
        for opt in &opts {
            println!("{opt}");
        }
        return Ok(());
    }

    if args.command.is_empty() {
        bail!("a command is required (or pass --emit-opts to print ssh options)");
    }

    let session = SshSession::new(SshConfig {
        host,
        port,
        user,
        identity_file: ssh_key,
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
