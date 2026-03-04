use std::path::PathBuf;

use clap::Args;
use eyre::Context;
use tracing::info;

use crate::arch::Platform;
use crate::fcos::ImageVariant;
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

    /// Image variant (qemu or metal4k).
    #[arg(long, value_enum, default_value_t = ImageVariant::Qemu)]
    pub variant: ImageVariant,

    /// Run QEMU in foreground with serial console on stdio.
    #[arg(long)]
    pub interactive: bool,

    /// Additional port forward (host:guest, repeatable).
    #[arg(long)]
    pub forward: Vec<String>,

    /// Extra QEMU argument (repeatable).
    #[arg(long)]
    pub qemu_arg: Vec<String>,
}

pub async fn run(
    args: BootArgs,
    work_dir: &std::path::Path,
    firmware: &std::path::Path,
) -> eyre::Result<()> {
    let platform = Platform::detect()?;

    // Ensure FCOS image
    let base_disk = crate::fcos::FcosImage::new(work_dir, platform.arch)
        .variant(args.variant)
        .ensure()
        .await?;

    // Create overlay disk (segregate by variant)
    let overlay_name = match args.variant {
        ImageVariant::Qemu => "diff-boot.qcow2",
        ImageVariant::Metal4k => "diff-boot-4k.qcow2",
    };
    let diff_disk = work_dir.join(overlay_name);
    if !diff_disk.exists() {
        crate::disk::create_overlay(&base_disk, &diff_disk, "32G", args.variant.backing_format())
            .await?;
    }

    let mut builder = VmBuilder::new(platform, firmware)
        .disk(&diff_disk)
        .ignition(&args.ignition)
        .ssh_port(args.ssh_port)
        .ssh_key(&args.ssh_key)
        .hostname(&args.hostname)
        .serial_log(work_dir.join("serial-boot.log"))
        .interactive(args.interactive);

    if args.variant == ImageVariant::Metal4k {
        builder = builder.block_size(4096);
    }

    if args.qmp {
        builder = builder.qmp_socket(work_dir.join("qemu-monitor.sock"));
    }

    if let Some(ref name) = args.loadvm {
        builder = builder.loadvm(name);
    }

    for fwd in &args.forward {
        let (host, guest) = fwd
            .split_once(':')
            .ok_or_else(|| eyre::eyre!("invalid forward format '{fwd}', expected host:guest"))?;
        let host: u16 = host
            .parse()
            .wrap_err_with(|| format!("invalid host port in '{fwd}'"))?;
        let guest: u16 = guest
            .parse()
            .wrap_err_with(|| format!("invalid guest port in '{fwd}'"))?;
        builder = builder.forward(host, guest);
    }

    for arg in &args.qemu_arg {
        builder = builder.extra_arg(arg);
    }

    if args.interactive {
        return builder.spawn_interactive();
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
