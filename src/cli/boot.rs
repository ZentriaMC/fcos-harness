use std::path::PathBuf;

use clap::Args;
use tracing::info;

use crate::arch::Platform;
use crate::qemu::VmBuilder;

#[derive(Args)]
pub struct BootArgs {
    /// Ignition config file.
    #[arg(long)]
    pub ignition: PathBuf,

    /// SSH port forward.
    #[arg(long, env = "TEST_SSH_PORT", default_value = "2223")]
    pub ssh_port: u16,

    /// SSH private key.
    #[arg(long)]
    pub ssh_key: PathBuf,

    /// VM hostname.
    #[arg(long, default_value = "fcos-test")]
    pub hostname: String,

    /// Keep VM running interactively after boot.
    #[arg(long)]
    pub keep: bool,

    /// Enable QMP socket for snapshot operations.
    #[arg(long)]
    pub qmp: bool,

    /// Load from a named QEMU snapshot instead of cold boot.
    #[arg(long)]
    pub loadvm: Option<String>,
}

pub async fn run(
    args: BootArgs,
    work_dir: &std::path::Path,
    firmware: &std::path::Path,
) -> eyre::Result<()> {
    let platform = Platform::detect()?;

    // Ensure FCOS image
    let base_disk = crate::fcos::FcosImage::new(work_dir, platform.arch)
        .ensure()
        .await?;

    // Create overlay disk
    let diff_disk = work_dir.join("diff-boot.qcow2");
    if !diff_disk.exists() {
        crate::disk::create_overlay(&base_disk, &diff_disk, "32G").await?;
    }

    let mut builder = VmBuilder::new(platform, firmware)
        .disk(&diff_disk)
        .ignition(&args.ignition)
        .ssh_port(args.ssh_port)
        .ssh_key(&args.ssh_key)
        .hostname(&args.hostname)
        .serial_log(work_dir.join("serial-boot.log"));

    if args.qmp {
        builder = builder.qmp_socket(work_dir.join("qemu-monitor.sock"));
    }

    if let Some(ref name) = args.loadvm {
        builder = builder.loadvm(name);
    }

    let mut vm = builder.launch().await?;

    let timeout = if args.loadvm.is_some() {
        std::time::Duration::from_secs(30)
    } else {
        std::time::Duration::from_secs(180)
    };

    let ssh = vm.ssh();
    match ssh
        .wait_ready(timeout, std::time::Duration::from_secs(5))
        .await
    {
        Ok(elapsed) => info!(elapsed, "SSH is ready"),
        Err(err) => {
            if let Ok(tail) = vm.serial_tail(30).await {
                eprintln!("--- serial log tail ---\n{tail}\n---");
            }
            return Err(err);
        }
    }

    if args.keep {
        eprintln!(
            "VM is running (ssh -p {} core@127.0.0.1). Press Ctrl-C to stop...",
            args.ssh_port
        );
        tokio::signal::ctrl_c().await?;
    }

    vm.shutdown().await?;
    Ok(())
}
