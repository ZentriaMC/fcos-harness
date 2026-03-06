pub mod boot;
pub mod ignition;
pub mod ssh;
pub mod up;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::fcos::ImageVariant;

fn default_cache_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg).join("fcos-harness")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache/fcos-harness")
    } else {
        PathBuf::from(".cache/fcos-harness")
    }
}

#[derive(Parser)]
#[command(name = "fcos-harness", about = "FCOS + QEMU integration test harness")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Working directory for VM artifacts (overlays, logs, PID files).
    #[arg(
        long,
        env = "FCOS_HARNESS_WORK_DIR",
        default_value = "tmp/vm",
        global = true
    )]
    pub work_dir: PathBuf,

    /// Cache directory for downloaded images and tools.
    #[arg(
        long,
        env = "FCOS_HARNESS_CACHE_DIR",
        default_value_os_t = default_cache_dir(),
        global = true
    )]
    pub cache_dir: PathBuf,

    /// UEFI firmware path (required for boot/start).
    #[arg(long, env = "QEMU_EFI_FW", global = true)]
    pub firmware: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Download and cache a FCOS image.
    Image {
        /// FCOS stream (stable, testing, next).
        #[arg(long, default_value = "next")]
        stream: String,

        /// Image variant (qemu or metal4k).
        #[arg(long, value_enum, default_value_t = ImageVariant::Qemu)]
        variant: ImageVariant,
    },

    /// Compile Butane files into a merged Ignition config.
    Ignition {
        /// Butane source files to compile and merge.
        sources: Vec<PathBuf>,

        /// Pre-compiled .ign file to use as the merge base.
        #[arg(long)]
        base: Option<PathBuf>,

        /// Overlay .bu files to compile and merge on top.
        #[arg(long)]
        overlay: Vec<PathBuf>,

        /// Template variable in KEY=VALUE format.
        #[arg(long, short = 'v')]
        var: Vec<String>,

        /// --files-dir for butane.
        #[arg(long)]
        files_dir: Option<PathBuf>,

        /// Path to butane binary.
        #[arg(long, env = "BUTANE", default_value = "butane")]
        butane: PathBuf,

        /// Output path for the .ign file.
        #[arg(long, short)]
        output: PathBuf,
    },

    /// Boot a FCOS VM interactively (blocks until shutdown).
    Boot(boot::BootArgs),

    /// Start a QEMU VM (background by default, or interactive with --interactive).
    Start {
        /// Path to the disk image.
        #[arg(long)]
        disk: PathBuf,

        /// Ignition config file.
        #[arg(long)]
        ignition: Option<PathBuf>,

        /// SSH port forward.
        #[arg(long, env = "TEST_SSH_PORT", default_value = "2223")]
        ssh_port: u16,

        /// VM hostname.
        #[arg(long, default_value = "fcos-test")]
        hostname: String,

        /// Serial log path (ignored in interactive mode).
        #[arg(long)]
        serial_log: Option<PathBuf>,

        /// QMP unix socket path (enables QMP).
        #[arg(long)]
        qmp: Option<PathBuf>,

        /// Load from a named QEMU snapshot instead of cold boot.
        #[arg(long)]
        loadvm: Option<String>,

        /// Block size for the SCSI disk device (e.g. 4096 for 4K).
        #[arg(long)]
        block_size: Option<u32>,

        /// Run QEMU in foreground with serial console on stdio.
        #[arg(long)]
        interactive: bool,

        /// Additional port forward (host:guest, repeatable).
        #[arg(long)]
        forward: Vec<String>,

        /// Extra QEMU argument (repeatable).
        #[arg(long)]
        qemu_arg: Vec<String>,

        /// File to write the QEMU PID to (required in background mode).
        #[arg(long)]
        pid_file: Option<PathBuf>,
    },

    /// Stop a background QEMU VM by PID file.
    Stop {
        /// PID file written by the start command.
        #[arg(long)]
        pid_file: PathBuf,
    },

    /// Create a qcow2 copy-on-write disk overlay.
    Disk {
        /// Base disk image.
        #[arg(long)]
        base: PathBuf,

        /// Output overlay path.
        #[arg(long)]
        overlay: PathBuf,

        /// Disk size.
        #[arg(long, default_value = "32G")]
        size: String,

        /// Backing image format (qcow2 or raw).
        #[arg(long, default_value = "qcow2")]
        backing_format: String,
    },

    /// Send a QMP command to a running VM.
    Qmp {
        /// Path to QMP unix socket.
        #[arg(long)]
        socket: PathBuf,

        #[command(subcommand)]
        command: QmpCommand,
    },

    /// Run goss validation against a running VM.
    Goss {
        /// Path to the goss.yaml file.
        gossfile: PathBuf,

        /// SSH port of the running VM.
        #[arg(long, env = "TEST_SSH_PORT", default_value = "2223")]
        ssh_port: u16,

        /// SSH private key.
        #[arg(long)]
        ssh_key: PathBuf,

        /// Retry timeout for goss validation (seconds).
        #[arg(long, default_value = "60")]
        retry_timeout_secs: u64,

        /// Run goss with sudo.
        #[arg(long)]
        sudo: bool,
    },

    /// Execute a command on the VM via SSH.
    Ssh(ssh::SshArgs),

    /// Bring up a VM: image → disk → start → wait SSH (with optional snapshot caching).
    Up(up::UpArgs),

    /// Tear down a running VM started by `up`.
    Down,
}

#[derive(Subcommand)]
pub enum QmpCommand {
    /// Save a VM snapshot.
    Savevm {
        /// Snapshot name.
        name: String,
    },
    /// Quit QEMU cleanly.
    Quit,
}
