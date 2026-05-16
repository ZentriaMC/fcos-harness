use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use eyre::{Context, bail};
use tracing::info;

use crate::arch::Platform;
use crate::backend::{Backend, BackendKind};
use crate::disk::create_clone;
use crate::fcos::ImageVariant;
use crate::qemu::VmBuilder;
use crate::qmp::QmpClient;
use crate::snapshot::SnapshotCache;
use crate::state::VmState;
use crate::vfkit;

#[derive(Args)]
pub struct UpArgs {
    /// Ignition config file.
    #[arg(long)]
    pub ignition: PathBuf,

    /// SSH private key.
    #[arg(long, env = "FCOS_HARNESS_SSH_KEY")]
    pub ssh_key: PathBuf,

    /// SSH port forward (QEMU backend only; vfkit uses NAT + port 22).
    #[arg(long, env = "FCOS_HARNESS_SSH_PORT", default_value = "2223")]
    pub ssh_port: u16,

    /// VM hostname.
    #[arg(long, default_value = "fcos-test")]
    pub hostname: String,

    /// Hypervisor backend. If omitted: vfkit when `--nested` is set on macOS aarch64,
    /// else qemu.
    #[arg(long, value_enum)]
    pub backend: Option<BackendKind>,

    /// Image variant. Defaults to qemu (QEMU backend) or applehv (vfkit backend).
    #[arg(long, value_enum)]
    pub variant: Option<ImageVariant>,

    /// FCOS stream (stable, testing, next).
    #[arg(long, default_value = "next")]
    pub stream: String,

    /// Named snapshot for fast restarts.
    /// QEMU: hashed memory snapshot via QMP savevm/loadvm.
    /// vfkit: warmed-disk cache (cold boot from a post-first-boot disk clone).
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

    /// Additional port forward host:guest (QEMU backend only, repeatable).
    #[arg(long)]
    pub forward: Vec<String>,

    /// Extra QEMU argument (QEMU backend only, repeatable).
    #[arg(long)]
    pub qemu_arg: Vec<String>,

    /// Extra vfkit argument (vfkit backend only, repeatable).
    #[arg(long)]
    pub vfkit_arg: Vec<String>,

    /// Enable nested virtualization.
    /// vfkit: requires M3+ on macOS 15+ (passes --nested to vfkit).
    /// QEMU on Linux: no-op — KVM+`-cpu host` already exposes nested if the
    /// host kernel has nested enabled.
    /// QEMU on macOS: not supported (HVF doesn't expose nested); use --backend vfkit.
    #[arg(long)]
    pub nested: bool,

    /// Disk size for the overlay (QEMU backend only).
    #[arg(long, default_value = "32G")]
    pub disk_size: String,

    /// SSH wait timeout in seconds.
    #[arg(long, default_value = "180")]
    pub ssh_timeout: u64,
}

impl UpArgs {
    /// Resolve the effective backend, applying the macOS-aarch64 auto-switch
    /// to vfkit when `--nested` is set without an explicit `--backend`.
    pub fn resolved_backend(&self) -> BackendKind {
        if let Some(b) = self.backend {
            return b;
        }
        if self.nested && cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return BackendKind::Vfkit;
        }
        BackendKind::Qemu
    }
}

/// Well-known paths inside work_dir, used by both `up` and `down`.
pub struct WorkPaths {
    pub qemu_pid_file: PathBuf,
    pub vfkit_pid_file: PathBuf,
    pub qmp_socket: PathBuf,
    pub rest_socket: PathBuf,
    pub serial_log: PathBuf,
    pub efi_vars: PathBuf,
}

impl WorkPaths {
    pub fn new(work_dir: &Path) -> Self {
        Self {
            qemu_pid_file: work_dir.join("qemu.pid"),
            vfkit_pid_file: work_dir.join("vfkit.pid"),
            qmp_socket: work_dir.join("qemu-monitor.sock"),
            rest_socket: work_dir.join("vfkit.sock"),
            serial_log: work_dir.join("serial.log"),
            efi_vars: work_dir.join("efi-vars.fd"),
        }
    }

    pub fn pid_file(&self, backend: BackendKind) -> &Path {
        match backend {
            BackendKind::Qemu => &self.qemu_pid_file,
            BackendKind::Vfkit => &self.vfkit_pid_file,
        }
    }
}

fn overlay_name(variant: ImageVariant) -> &'static str {
    match variant {
        ImageVariant::Qemu => "disk.qcow2",
        ImageVariant::Metal4k => "disk-4k.qcow2",
        ImageVariant::AppleHv => "disk-applehv.raw",
    }
}

fn snapshot_overlay_name(variant: ImageVariant) -> &'static str {
    match variant {
        ImageVariant::Qemu => "snapshot.qcow2",
        ImageVariant::Metal4k => "snapshot-4k.qcow2",
        ImageVariant::AppleHv => "snapshot-applehv.raw",
    }
}

/// Choose and validate the effective image variant for a backend.
///
/// Defaults: `qemu` for QEMU backend, `applehv` for vfkit backend.
/// Errors on incompatible combinations (e.g. applehv + qemu backend).
fn resolve_variant(
    backend: BackendKind,
    variant: Option<ImageVariant>,
) -> eyre::Result<ImageVariant> {
    let resolved = variant.unwrap_or(match backend {
        BackendKind::Qemu => ImageVariant::Qemu,
        BackendKind::Vfkit => ImageVariant::AppleHv,
    });
    match (backend, resolved) {
        (BackendKind::Qemu, ImageVariant::AppleHv) => bail!(
            "--variant applehv is built with platform=applehv and won't receive ignition under qemu; use --backend vfkit"
        ),
        (BackendKind::Vfkit, ImageVariant::Qemu | ImageVariant::Metal4k) => {
            bail!("--backend vfkit requires --variant applehv (or omit --variant to default)")
        }
        _ => Ok(resolved),
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

fn validate_backend_flags(args: &UpArgs, backend: BackendKind) -> eyre::Result<()> {
    match backend {
        BackendKind::Qemu => {
            if !args.vfkit_arg.is_empty() {
                bail!("--vfkit-arg is only valid with --backend vfkit");
            }
            // --nested + QEMU on macOS uses HVF, which doesn't expose nested virt.
            // On Linux this is a no-op (KVM + `-cpu host` already passes through).
            if args.nested && cfg!(target_os = "macos") {
                bail!(
                    "QEMU on macOS uses HVF, which cannot do nested virtualization. \
                     Use --backend vfkit (requires M3+ on macOS 15+), or omit --backend \
                     so it auto-switches to vfkit."
                );
            }
        }
        BackendKind::Vfkit => {
            if !args.forward.is_empty() {
                bail!("--forward is not supported with --backend vfkit (NAT mode)");
            }
            if !args.qemu_arg.is_empty() {
                bail!("--qemu-arg is only valid with --backend qemu");
            }
        }
    }
    Ok(())
}

pub async fn run(
    args: UpArgs,
    work_dir: &Path,
    cache_dir: Option<&Path>,
    firmware: Option<&Path>,
) -> eyre::Result<()> {
    let platform = Platform::detect()?;
    let backend = args.resolved_backend();
    let variant = resolve_variant(backend, args.variant)?;
    let paths = WorkPaths::new(work_dir);

    validate_backend_flags(&args, backend)?;
    let forwards = parse_forwards(&args.forward)?;

    if args.backend.is_none() && backend == BackendKind::Vfkit {
        info!("--nested on macOS aarch64: auto-selected vfkit backend");
    }

    // 1. Ensure FCOS image
    let mut image = crate::fcos::FcosImage::new(work_dir, platform.arch)
        .stream(&args.stream)
        .variant(variant);
    if let Some(dir) = cache_dir {
        image = image.cache_dir(dir);
    }
    let base_disk = image.ensure().await?;

    // 2 & 3. Prepare the live disk and (for snapshot mode) determine validity.
    let (overlay_disk, snapshot_valid) = match backend {
        BackendKind::Qemu => qemu_prepare_disk(&args, work_dir, &base_disk, variant).await?,
        BackendKind::Vfkit => vfkit_prepare_disk(&args, work_dir, &base_disk, variant).await?,
    };

    // 4. Build and launch VM.
    let mut vm: Box<dyn Backend> = match backend {
        BackendKind::Qemu => {
            let firmware = firmware.ok_or_else(|| {
                eyre::eyre!("QEMU backend requires firmware (--firmware or QEMU_EFI_FW)")
            })?;
            Box::new(
                build_qemu(
                    platform.clone(),
                    firmware,
                    &args,
                    &paths,
                    &overlay_disk,
                    variant,
                    &forwards,
                    snapshot_valid,
                )
                .launch()
                .await?,
            )
        }
        BackendKind::Vfkit => Box::new(
            build_vfkit(work_dir, &args, &paths, &overlay_disk)
                .launch()
                .await?,
        ),
    };

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

    // 5. If snapshot mode and no valid snapshot, create one.
    // QEMU: QMP savevm + restart with --loadvm.
    // vfkit: guest poweroff + clone live→warmed + relaunch.
    if let Some(ref snapshot_name) = args.snapshot
        && !snapshot_valid
        && backend == BackendKind::Vfkit
    {
        // Optional goss validation before warming the disk.
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

        // Shut down the guest cleanly so the disk is in a consistent state.
        // We trigger via SSH (systemctl poweroff) and wait for vfkit to exit
        // naturally; if the guest is unresponsive, fall back to SIGTERM.
        info!("powering off guest to warm the disk snapshot");
        let _ = ssh.exec("sudo systemctl poweroff").await;
        match tokio::time::timeout(Duration::from_secs(30), vm.wait()).await {
            Ok(_) => info!("vfkit exited after guest poweroff"),
            Err(_) => {
                tracing::warn!("vfkit did not exit within 30s after poweroff; sending SIGTERM");
                vm.shutdown().await?;
            }
        }

        // Clone the now-clean live disk into the warmed cache.
        let warmed = work_dir.join(snapshot_overlay_name(variant));
        let kind = create_clone(&overlay_disk, &warmed).await?;
        info!(?kind, warmed = %warmed.display(), "warmed disk saved");

        // Record the ignition hash for future runs.
        let hash = crate::sha256_file(&args.ignition).await?;
        let hash_file = work_dir.join("snapshot.hash");
        tokio::fs::write(&hash_file, &hash).await?;

        // Relaunch vfkit so this run leaves a running VM as `fh up` always does.
        vm = Box::new(
            build_vfkit(work_dir, &args, &paths, &overlay_disk)
                .launch()
                .await?,
        );

        let post_ssh = vm.ssh();
        post_ssh
            .wait_ready(Duration::from_secs(60), Duration::from_secs(2))
            .await?;

        info!(snapshot = snapshot_name, "vfkit snapshot warmed");
    }

    if let Some(ref snapshot_name) = args.snapshot
        && !snapshot_valid
        && backend == BackendKind::Qemu
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

        // Save snapshot via QMP, quit, restart from snapshot.
        let qmp = QmpClient::new(&paths.qmp_socket);
        qmp.savevm(snapshot_name).await?;
        info!("restarting from snapshot");
        qmp.quit().await?;
        vm.wait().await.ok();

        let firmware = firmware.expect("QEMU branch already verified firmware");
        vm = Box::new(
            build_qemu(
                platform.clone(),
                firmware,
                &args,
                &paths,
                &overlay_disk,
                variant,
                &forwards,
                true,
            )
            .launch()
            .await?,
        );

        let post_ssh = vm.ssh();
        post_ssh
            .wait_ready(Duration::from_secs(30), Duration::from_secs(2))
            .await?;

        let hash = crate::sha256_file(&args.ignition).await?;
        let hash_file = work_dir.join("snapshot.hash");
        tokio::fs::write(&hash_file, &hash).await?;

        info!(snapshot = snapshot_name, "snapshot created and restored");
    }

    // 6. Write PID and SSH-endpoint state, then detach.
    let pid_file = paths.pid_file(backend);
    let pid = vm.pid().ok_or_else(|| eyre::eyre!("VM has no PID"))?;
    if let Some(parent) = pid_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(pid_file, pid.to_string()).await?;

    let cfg = vm.ssh_config();
    VmState {
        backend: backend.as_str().to_string(),
        host: cfg.host.clone(),
        port: cfg.port,
        user: cfg.user.clone(),
    }
    .write(work_dir)
    .await?;

    vm.detach();
    info!(pid, backend = backend.as_str(), "VM is up");
    println!("{pid}");
    Ok(())
}

/// Determine the vfkit live disk path and warmed-snapshot validity for a given run.
///
/// Layout:
/// - `live_disk` is what vfkit attaches as virtio-blk (`disk-applehv.raw`).
/// - `warmed_disk` is the cached post-first-boot clone (`snapshot-applehv.raw`).
///
/// If the snapshot hash matches and the warmed disk exists, the live disk is
/// cloned from the warmed disk for a fast cold boot. Otherwise, live is cloned
/// from the base image and the warmed disk will be (re)created after first boot.
async fn vfkit_prepare_disk(
    args: &UpArgs,
    work_dir: &Path,
    base_disk: &Path,
    variant: ImageVariant,
) -> eyre::Result<(PathBuf, bool)> {
    let live = work_dir.join(overlay_name(variant));
    let warmed = work_dir.join(snapshot_overlay_name(variant));
    let hash_file = work_dir.join("snapshot.hash");

    let (source, snapshot_valid) = if let Some(_snapshot_name) = args.snapshot.as_ref() {
        let cache = SnapshotCache::external_disk(&live, &hash_file, &warmed);
        let current_hash = crate::sha256_file(&args.ignition).await?;
        let valid = if args.rebuild_snapshot {
            false
        } else {
            cache.is_valid(&current_hash).await.unwrap_or(false)
        };
        if !valid {
            cache.invalidate().await?;
            (base_disk.to_path_buf(), false)
        } else {
            info!(warmed = %warmed.display(), "vfkit: warmed disk hit");
            (warmed.clone(), true)
        }
    } else {
        (base_disk.to_path_buf(), false)
    };

    let kind = create_clone(&source, &live).await?;
    info!(
        ?kind,
        source = %source.display(),
        live = %live.display(),
        "vfkit disk ready",
    );
    Ok((live, snapshot_valid))
}

/// Determine the QEMU overlay path and snapshot validity for a given run.
async fn qemu_prepare_disk(
    args: &UpArgs,
    work_dir: &Path,
    base_disk: &Path,
    variant: ImageVariant,
) -> eyre::Result<(PathBuf, bool)> {
    let (overlay_disk, snapshot_valid) = if let Some(ref snapshot_name) = args.snapshot {
        let disk = work_dir.join(snapshot_overlay_name(variant));
        let hash_file = work_dir.join("snapshot.hash");

        let cache = SnapshotCache::qcow_internal(&disk, &hash_file, snapshot_name);
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
        (work_dir.join(overlay_name(variant)), false)
    };

    if !overlay_disk.exists() {
        crate::disk::create_overlay(
            base_disk,
            &overlay_disk,
            &args.disk_size,
            variant.backing_format(),
        )
        .await?;
    }

    Ok((overlay_disk, snapshot_valid))
}

#[allow(clippy::too_many_arguments)]
fn build_qemu(
    platform: Platform,
    firmware: &Path,
    args: &UpArgs,
    paths: &WorkPaths,
    overlay_disk: &Path,
    variant: ImageVariant,
    forwards: &[(u16, u16)],
    loadvm: bool,
) -> VmBuilder {
    let mut builder = VmBuilder::new(platform, firmware)
        .disk(overlay_disk)
        .ignition(&args.ignition)
        .ssh_port(args.ssh_port)
        .ssh_key(&args.ssh_key)
        .hostname(&args.hostname)
        .serial_log(&paths.serial_log)
        .qmp_socket(&paths.qmp_socket);

    if variant == ImageVariant::Metal4k {
        builder = builder.block_size(4096);
    }
    for (host, guest) in forwards {
        builder = builder.forward(*host, *guest);
    }
    for arg in &args.qemu_arg {
        builder = builder.extra_arg(arg);
    }
    if loadvm {
        builder = builder.loadvm(args.snapshot.as_ref().expect("loadvm requires --snapshot"));
    }
    builder
}

fn build_vfkit(
    work_dir: &Path,
    args: &UpArgs,
    paths: &WorkPaths,
    overlay_disk: &Path,
) -> vfkit::VmBuilder {
    let mut builder = vfkit::VmBuilder::new(work_dir.to_path_buf())
        .disk(overlay_disk)
        .ignition(&args.ignition)
        .ssh_key(&args.ssh_key)
        .hostname(&args.hostname)
        .serial_log(&paths.serial_log)
        .rest_socket(&paths.rest_socket)
        .efi_vars(&paths.efi_vars)
        .pid_file(&paths.vfkit_pid_file)
        .nested(args.nested);
    for arg in &args.vfkit_arg {
        builder = builder.extra_arg(arg);
    }
    builder
}

pub async fn down(work_dir: &Path) -> eyre::Result<()> {
    let paths = WorkPaths::new(work_dir);
    let candidates: &[&Path] = &[&paths.qemu_pid_file, &paths.vfkit_pid_file];

    let mut stopped_any = false;
    for pid_file in candidates {
        if pid_file.exists() {
            stop_by_pid_file(pid_file).await?;
            stopped_any = true;
        }
    }

    if !stopped_any {
        bail!("no VM running (no PID file in {})", work_dir.display(),);
    }

    // Best-effort socket + state cleanup.
    tokio::fs::remove_file(&paths.qmp_socket).await.ok();
    tokio::fs::remove_file(&paths.rest_socket).await.ok();
    VmState::remove(work_dir).await.ok();

    info!("VM stopped");
    Ok(())
}

async fn stop_by_pid_file(pid_file: &Path) -> eyre::Result<()> {
    let pid_str = tokio::fs::read_to_string(pid_file)
        .await
        .wrap_err_with(|| format!("failed to read PID file: {}", pid_file.display()))?;
    let pid: i32 = pid_str.trim().parse().wrap_err("invalid PID in file")?;

    info!(pid, "stopping VM");
    // SAFETY: SIGTERM to an arbitrary PID is safe; we tolerate ESRCH if the
    // process already exited.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    for _ in 0..20 {
        // SAFETY: signal 0 is a probe (no effect, just returns ESRCH if dead).
        unsafe {
            if libc::kill(pid, 0) != 0 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    tokio::fs::remove_file(pid_file).await.ok();
    Ok(())
}
