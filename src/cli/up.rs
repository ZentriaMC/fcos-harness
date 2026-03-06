use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use eyre::{Context, bail};
use tracing::info;

use crate::arch::Platform;
use crate::fcos::ImageVariant;
use crate::qemu::VmBuilder;
use crate::qmp::{QmpClient, SnapshotCache};

#[derive(Args)]
pub struct UpArgs {
    /// Ignition config file.
    #[arg(long)]
    pub ignition: PathBuf,

    /// SSH private key.
    #[arg(long, env = "FCOS_HARNESS_SSH_KEY")]
    pub ssh_key: PathBuf,

    /// SSH port forward.
    #[arg(long, env = "TEST_SSH_PORT", default_value = "2223")]
    pub ssh_port: u16,

    /// VM hostname.
    #[arg(long, default_value = "fcos-test")]
    pub hostname: String,

    /// Image variant (qemu or metal4k).
    #[arg(long, value_enum, default_value_t = ImageVariant::Qemu)]
    pub variant: ImageVariant,

    /// FCOS stream (stable, testing, next).
    #[arg(long, default_value = "next")]
    pub stream: String,

    /// Named snapshot for fast restarts.
    /// Hashes the ignition config; if the snapshot exists and the hash matches,
    /// the VM is restored instantly. Otherwise a fresh boot + save is performed.
    #[arg(long)]
    pub snapshot: Option<String>,

    /// Goss file to validate before saving a snapshot.
    #[arg(long)]
    pub snapshot_goss: Option<PathBuf>,

    /// Run snapshot goss validation with sudo.
    #[arg(long)]
    pub snapshot_goss_sudo: bool,

    /// Force snapshot recreation even if hash matches.
    #[arg(long)]
    pub rebuild_snapshot: bool,

    /// Additional port forward (host:guest, repeatable).
    #[arg(long)]
    pub forward: Vec<String>,

    /// Extra QEMU argument (repeatable).
    #[arg(long)]
    pub qemu_arg: Vec<String>,

    /// Disk size for the overlay.
    #[arg(long, default_value = "32G")]
    pub disk_size: String,

    /// SSH wait timeout in seconds.
    #[arg(long, default_value = "180")]
    pub ssh_timeout: u64,
}

/// Well-known paths inside work_dir, used by both `up` and `down`.
pub struct WorkPaths {
    pub pid_file: PathBuf,
    pub qmp_socket: PathBuf,
    pub serial_log: PathBuf,
}

impl WorkPaths {
    pub fn new(work_dir: &std::path::Path) -> Self {
        Self {
            pid_file: work_dir.join("qemu.pid"),
            qmp_socket: work_dir.join("qemu-monitor.sock"),
            serial_log: work_dir.join("serial.log"),
        }
    }
}

fn overlay_name(variant: ImageVariant) -> &'static str {
    match variant {
        ImageVariant::Qemu => "disk.qcow2",
        ImageVariant::Metal4k => "disk-4k.qcow2",
    }
}

fn snapshot_overlay_name(variant: ImageVariant) -> &'static str {
    match variant {
        ImageVariant::Qemu => "snapshot.qcow2",
        ImageVariant::Metal4k => "snapshot-4k.qcow2",
    }
}

fn parse_forwards(forward: &[String]) -> eyre::Result<Vec<(u16, u16)>> {
    forward
        .iter()
        .map(|fwd| {
            let (host, guest) = fwd.split_once(':').ok_or_else(|| {
                eyre::eyre!("invalid forward format '{fwd}', expected host:guest")
            })?;
            let host: u16 = host
                .parse()
                .wrap_err_with(|| format!("invalid host port in '{fwd}'"))?;
            let guest: u16 = guest
                .parse()
                .wrap_err_with(|| format!("invalid guest port in '{fwd}'"))?;
            Ok((host, guest))
        })
        .collect()
}

pub async fn run(
    args: UpArgs,
    work_dir: &std::path::Path,
    cache_dir: Option<&std::path::Path>,
    firmware: &std::path::Path,
) -> eyre::Result<()> {
    let platform = Platform::detect()?;
    let paths = WorkPaths::new(work_dir);
    let forwards = parse_forwards(&args.forward)?;

    // 1. Ensure FCOS image
    let mut image = crate::fcos::FcosImage::new(work_dir, platform.arch)
        .stream(&args.stream)
        .variant(args.variant);
    if let Some(dir) = cache_dir {
        image = image.cache_dir(dir);
    }
    let base_disk = image.ensure().await?;

    // 2. Determine overlay disk and snapshot state
    let (overlay_disk, snapshot_valid) = if let Some(ref snapshot_name) = args.snapshot {
        let disk = work_dir.join(snapshot_overlay_name(args.variant));
        let hash_file = work_dir.join("snapshot.hash");

        let cache = SnapshotCache::new(&disk, &hash_file, snapshot_name);
        let current_hash = crate::sha256_file(&args.ignition).await?;

        let valid = if args.rebuild_snapshot {
            false
        } else {
            cache.is_valid(&current_hash).await.unwrap_or(false)
        };

        if !valid {
            cache.invalidate().await?;
        }

        (disk, valid)
    } else {
        (work_dir.join(overlay_name(args.variant)), false)
    };

    // 3. Create overlay if needed
    if !overlay_disk.exists() {
        crate::disk::create_overlay(
            &base_disk,
            &overlay_disk,
            &args.disk_size,
            args.variant.backing_format(),
        )
        .await?;
    }

    // 4. Build and launch VM
    let mut builder = VmBuilder::new(platform.clone(), firmware)
        .disk(&overlay_disk)
        .ignition(&args.ignition)
        .ssh_port(args.ssh_port)
        .ssh_key(&args.ssh_key)
        .hostname(&args.hostname)
        .serial_log(&paths.serial_log)
        .qmp_socket(&paths.qmp_socket);

    if args.variant == ImageVariant::Metal4k {
        builder = builder.block_size(4096);
    }
    for (host, guest) in &forwards {
        builder = builder.forward(*host, *guest);
    }
    for arg in &args.qemu_arg {
        builder = builder.extra_arg(arg);
    }
    if snapshot_valid {
        builder = builder.loadvm(args.snapshot.as_ref().unwrap());
    }

    let mut vm = builder.launch().await?;

    let ssh_timeout = if snapshot_valid {
        Duration::from_secs(30)
    } else {
        Duration::from_secs(args.ssh_timeout)
    };

    let ssh = vm.ssh();
    match ssh.wait_ready(ssh_timeout, Duration::from_secs(5)).await {
        Ok(elapsed) => info!(elapsed, "SSH is ready"),
        Err(err) => {
            if let Ok(tail) = vm.serial_tail(30).await {
                eprintln!("--- serial log tail ---\n{tail}\n---");
            }
            vm.shutdown().await?;
            return Err(err);
        }
    }

    // 5. If snapshot mode and no valid snapshot, create one
    if let Some(ref snapshot_name) = args.snapshot
        && !snapshot_valid
    {
        // Optional goss validation before snapshot
        if let Some(ref gossfile) = args.snapshot_goss {
            info!("running goss validation before snapshot");
            let goss_cache = cache_dir.unwrap_or(work_dir);
            crate::goss::Goss::new(goss_cache, platform.arch)
                .sudo(args.snapshot_goss_sudo)
                .validate(
                    &ssh,
                    gossfile,
                    Duration::from_secs(60),
                    Duration::from_secs(5),
                )
                .await?;
        }

        // Save snapshot
        let qmp = QmpClient::new(&paths.qmp_socket);
        qmp.savevm(snapshot_name).await?;

        // Quit, restart from snapshot
        info!("restarting from snapshot");
        qmp.quit().await?;
        vm.wait().await.ok();

        // Rebuild VM with --loadvm
        let mut builder = VmBuilder::new(platform, firmware)
            .disk(&overlay_disk)
            .ignition(&args.ignition)
            .ssh_port(args.ssh_port)
            .ssh_key(&args.ssh_key)
            .hostname(&args.hostname)
            .serial_log(&paths.serial_log)
            .qmp_socket(&paths.qmp_socket)
            .loadvm(snapshot_name);

        if args.variant == ImageVariant::Metal4k {
            builder = builder.block_size(4096);
        }
        for (host, guest) in &forwards {
            builder = builder.forward(*host, *guest);
        }
        for arg in &args.qemu_arg {
            builder = builder.extra_arg(arg);
        }

        vm = builder.launch().await?;

        let ssh = vm.ssh();
        ssh.wait_ready(Duration::from_secs(30), Duration::from_secs(2))
            .await?;

        // Record hash
        let hash = crate::sha256_file(&args.ignition).await?;
        let hash_file = work_dir.join("snapshot.hash");
        tokio::fs::write(&hash_file, &hash).await?;

        info!(snapshot = snapshot_name, "snapshot created and restored");
    }

    // 6. Write PID file and detach
    let pid = vm.pid().ok_or_else(|| eyre::eyre!("QEMU has no PID"))?;
    tokio::fs::write(&paths.pid_file, pid.to_string()).await?;

    // Forget the Vm so Drop doesn't kill QEMU
    vm.detach();

    info!(pid, "VM is up");
    println!("{pid}");
    Ok(())
}

pub async fn down(work_dir: &std::path::Path) -> eyre::Result<()> {
    let paths = WorkPaths::new(work_dir);

    if !paths.pid_file.exists() {
        bail!(
            "no VM running (PID file not found: {})",
            paths.pid_file.display()
        );
    }

    let pid_str = tokio::fs::read_to_string(&paths.pid_file)
        .await
        .wrap_err_with(|| format!("failed to read PID file: {}", paths.pid_file.display()))?;
    let pid: i32 = pid_str.trim().parse().wrap_err("invalid PID in file")?;

    info!(pid, "stopping VM");
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    // Wait for process to exit
    for _ in 0..20 {
        unsafe {
            if libc::kill(pid, 0) != 0 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    tokio::fs::remove_file(&paths.pid_file).await.ok();
    tokio::fs::remove_file(&paths.qmp_socket).await.ok();

    info!("VM stopped");
    Ok(())
}
