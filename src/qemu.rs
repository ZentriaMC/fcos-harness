use std::path::{Path, PathBuf};

use eyre::{Context, bail};
use tokio::process::Command;
use tracing::{info, warn};

use crate::arch::Platform;
use crate::ssh::{SshConfig, SshSession};

/// Builder for configuring and launching a QEMU VM.
pub struct VmBuilder {
    platform: Platform,
    firmware: PathBuf,
    disk: PathBuf,
    ignition: Option<PathBuf>,
    ssh_port: u16,
    ssh_key: PathBuf,
    hostname: String,
    cpus: u32,
    memory: String,
    serial_log: PathBuf,
    snapshot_mode: bool,
    qmp_socket: Option<PathBuf>,
    loadvm: Option<String>,
    block_size: Option<u32>,
    interactive: bool,
    forwards: Vec<(u16, u16)>,
    extra_args: Vec<String>,
}

impl VmBuilder {
    pub fn new(platform: Platform, firmware: impl Into<PathBuf>) -> Self {
        Self {
            platform,
            firmware: firmware.into(),
            disk: PathBuf::new(),
            ignition: None,
            ssh_port: 2223,
            ssh_key: PathBuf::new(),
            hostname: "fcos-test".into(),
            cpus: 4,
            memory: "4G".into(),
            serial_log: PathBuf::from("/tmp/vm/serial-test.log"),
            snapshot_mode: false,
            qmp_socket: None,
            loadvm: None,
            block_size: None,
            interactive: false,
            forwards: Vec::new(),
            extra_args: Vec::new(),
        }
    }

    pub fn disk(mut self, path: impl Into<PathBuf>) -> Self {
        self.disk = path.into();
        self
    }

    pub fn ignition(mut self, path: impl Into<PathBuf>) -> Self {
        self.ignition = Some(path.into());
        self
    }

    pub fn ssh_port(mut self, port: u16) -> Self {
        self.ssh_port = port;
        self
    }

    pub fn ssh_key(mut self, path: impl Into<PathBuf>) -> Self {
        self.ssh_key = path.into();
        self
    }

    pub fn hostname(mut self, name: impl Into<String>) -> Self {
        self.hostname = name.into();
        self
    }

    pub fn cpus(mut self, n: u32) -> Self {
        self.cpus = n;
        self
    }

    pub fn memory(mut self, mem: impl Into<String>) -> Self {
        self.memory = mem.into();
        self
    }

    pub fn serial_log(mut self, path: impl Into<PathBuf>) -> Self {
        self.serial_log = path.into();
        self
    }

    pub fn snapshot_mode(mut self, enabled: bool) -> Self {
        self.snapshot_mode = enabled;
        self
    }

    pub fn qmp_socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.qmp_socket = Some(path.into());
        self
    }

    pub fn loadvm(mut self, name: impl Into<String>) -> Self {
        self.loadvm = Some(name.into());
        self
    }

    pub fn block_size(mut self, size: u32) -> Self {
        self.block_size = Some(size);
        self
    }

    pub fn interactive(mut self, enabled: bool) -> Self {
        self.interactive = enabled;
        self
    }

    pub fn forward(mut self, host_port: u16, guest_port: u16) -> Self {
        self.forwards.push((host_port, guest_port));
        self
    }

    pub fn extra_arg(mut self, arg: impl Into<String>) -> Self {
        self.extra_args.push(arg.into());
        self
    }

    /// Build the full QEMU argument list (for debugging / dry-run).
    pub fn build_args(&self) -> Vec<String> {
        let mut args = Vec::new();

        // Base args
        args.extend([
            "-nodefaults".into(),
            "-no-user-config".into(),
            "-display".into(),
            "none".into(),
            "-cpu".into(),
            "host".into(),
            "-smp".into(),
            self.cpus.to_string(),
            "-m".into(),
            self.memory.clone(),
            "-serial".into(),
            if self.interactive {
                "mon:stdio".into()
            } else {
                format!("file:{}", self.serial_log.display())
            },
            "-rtc".into(),
            "base=utc".into(),
        ]);

        // Networking
        let mut netdev = format!(
            "user,id=user.0,hostfwd=tcp::{}-:22,hostname={}",
            self.ssh_port, self.hostname
        );
        for (host, guest) in &self.forwards {
            netdev.push_str(&format!(",hostfwd=tcp::{host}-:{guest}"));
        }
        args.extend(["-netdev".into(), netdev]);

        // Devices
        args.extend([
            "-device".into(),
            "virtio-rng-pci".into(),
            "-device".into(),
            "virtio-scsi-pci,id=scsi0,num_queues=4".into(),
            "-device".into(),
            "virtio-serial-pci".into(),
            "-device".into(),
            "virtio-net-pci,netdev=user.0".into(),
            "-device".into(),
            "qemu-xhci".into(),
            "-usb".into(),
        ]);

        // Disk
        let mut scsi_device = "scsi-hd,bus=scsi0.0,drive=root-disk0".to_string();
        if let Some(bs) = self.block_size {
            scsi_device.push_str(&format!(
                ",physical_block_size={bs},logical_block_size={bs}"
            ));
        }
        args.extend([
            "-drive".into(),
            format!(
                "if=none,id=root-disk0,file={},format=qcow2",
                self.disk.display()
            ),
            "-device".into(),
            scsi_device,
        ]);

        // Snapshot mode
        if self.snapshot_mode {
            args.push("-snapshot".into());
        }

        // Platform-specific: machine type + firmware
        args.extend(self.platform.machine_args());
        args.extend(self.platform.firmware_args(&self.firmware));

        // Ignition config
        if let Some(ref ign) = self.ignition {
            args.extend([
                "-fw_cfg".into(),
                format!("name=opt/com.coreos/config,file={}", ign.display()),
            ]);
        }

        // QMP socket
        if let Some(ref qmp) = self.qmp_socket {
            args.extend([
                "-qmp".into(),
                format!("unix:{},server,nowait", qmp.display()),
            ]);
        }

        // Load VM snapshot
        if let Some(ref name) = self.loadvm {
            args.extend(["-loadvm".into(), name.clone()]);
        }

        // Extra args
        args.extend(self.extra_args.clone());

        args
    }

    /// Spawn QEMU with inherited stdio (interactive/foreground mode).
    /// Blocks until QEMU exits.
    pub fn spawn_interactive(self) -> eyre::Result<()> {
        let args = self.build_args();
        info!(
            binary = self.platform.qemu_binary,
            ssh_port = self.ssh_port,
            "spawning QEMU VM (interactive)"
        );

        let status = std::process::Command::new(self.platform.qemu_binary)
            .args(&args)
            .status()
            .wrap_err_with(|| format!("failed to spawn {}", self.platform.qemu_binary))?;

        if !status.success() {
            bail!("QEMU exited with {status}");
        }

        Ok(())
    }

    /// Launch the VM as a background process.
    pub async fn launch(self) -> eyre::Result<Vm> {
        // Ensure serial log directory exists
        if let Some(parent) = self.serial_log.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Clean up stale QMP socket
        if let Some(ref qmp) = self.qmp_socket {
            tokio::fs::remove_file(qmp).await.ok();
        }

        let args = self.build_args();

        info!(
            binary = self.platform.qemu_binary,
            ssh_port = self.ssh_port,
            "launching QEMU VM"
        );

        let child = Command::new(self.platform.qemu_binary)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .wrap_err_with(|| format!("failed to spawn {}", self.platform.qemu_binary))?;

        let ssh_config = SshConfig {
            host: "127.0.0.1".into(),
            port: self.ssh_port,
            user: "core".into(),
            identity_file: self.ssh_key.clone(),
            ..SshConfig::default()
        };

        // Brief pause to let QEMU initialize
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let vm = Vm {
            child,
            ssh_config,
            serial_log: self.serial_log,
            qmp_socket: self.qmp_socket,
        };

        // Check that QEMU is still running
        if !vm.is_running() {
            bail!("QEMU failed to start (exited immediately)");
        }

        Ok(vm)
    }
}

/// A running QEMU VM with lifecycle management.
pub struct Vm {
    child: tokio::process::Child,
    ssh_config: SshConfig,
    serial_log: PathBuf,
    qmp_socket: Option<PathBuf>,
}

impl Vm {
    /// Get an SSH session to this VM.
    pub fn ssh(&self) -> SshSession {
        SshSession::new(self.ssh_config.clone())
    }

    /// Get the SSH config for this VM.
    pub fn ssh_config(&self) -> &SshConfig {
        &self.ssh_config
    }

    /// Get the QMP socket path, if configured.
    pub fn qmp_socket(&self) -> Option<&Path> {
        self.qmp_socket.as_deref()
    }

    /// Read the tail of the serial log (useful for debugging failures).
    pub async fn serial_tail(&self, lines: usize) -> eyre::Result<String> {
        let content = tokio::fs::read_to_string(&self.serial_log)
            .await
            .wrap_err_with(|| {
                format!("failed to read serial log: {}", self.serial_log.display())
            })?;

        let tail: Vec<&str> = content.lines().rev().take(lines).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        Ok(tail.join("\n"))
    }

    /// Check if the QEMU process is still running.
    pub fn is_running(&self) -> bool {
        // try_wait returns Ok(None) if the process is still running
        self.child.id().is_some()
    }

    /// Wait for the process to exit.
    pub async fn wait(&mut self) -> eyre::Result<std::process::ExitStatus> {
        let status = self.child.wait().await.wrap_err("failed to wait on QEMU")?;
        Ok(status)
    }

    /// Get the QEMU process PID.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Detach from the QEMU process so `Drop` won't kill it.
    /// Used by `up` to keep the VM running after the CLI exits.
    pub fn detach(self) {
        std::mem::forget(self);
    }

    /// Send SIGTERM and wait for exit.
    pub async fn shutdown(&mut self) -> eyre::Result<()> {
        info!("shutting down QEMU VM");
        if self.child.id().is_some() {
            self.child.kill().await.ok();
            self.child.wait().await.ok();
        }
        Ok(())
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.child.id().is_some() {
            warn!("Vm dropped without explicit shutdown, killing QEMU");
            // Best-effort kill; we can't await in Drop
            let _ = self.child.start_kill();
        }
    }
}
