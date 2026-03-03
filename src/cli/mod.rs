pub mod boot;
pub mod ignition;
pub mod ssh;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fcos-harness", about = "FCOS + QEMU integration test harness")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Working directory for VM artifacts (images, overlays, logs).
    #[arg(
        long,
        env = "FCOS_HARNESS_WORK_DIR",
        default_value = "tmp/vm",
        global = true
    )]
    pub work_dir: PathBuf,

    /// UEFI firmware path.
    #[arg(long, env = "QEMU_EFI_FW", global = true)]
    pub firmware: PathBuf,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Download and cache a FCOS qcow2 image.
    Image {
        /// FCOS stream (stable, testing, next).
        #[arg(long, default_value = "next")]
        stream: String,
    },

    /// Compile Butane files into a merged Ignition config.
    Ignition {
        /// Butane source files to compile and merge.
        #[arg(required = true)]
        sources: Vec<PathBuf>,

        /// Overlay .bu files to merge on top.
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

    /// Start a QEMU VM in the background and return immediately.
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

        /// Serial log path.
        #[arg(long)]
        serial_log: Option<PathBuf>,

        /// QMP unix socket path (enables QMP).
        #[arg(long)]
        qmp: Option<PathBuf>,

        /// Load from a named QEMU snapshot instead of cold boot.
        #[arg(long)]
        loadvm: Option<String>,

        /// File to write the QEMU PID to.
        #[arg(long)]
        pid_file: PathBuf,
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
