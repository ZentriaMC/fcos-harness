use std::time::Duration;

use clap::Parser;
use eyre::{Context, bail};
use tracing::info;

use fcos_harness::arch::Platform;
use fcos_harness::cli::{Cli, Commands, QmpCommand};
use fcos_harness::fcos::FcosImage;
use fcos_harness::goss::Goss;
use fcos_harness::qmp::QmpClient;
use fcos_harness::ssh::{SshConfig, SshSession};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Image { stream, variant } => {
            let platform = Platform::detect()?;
            let image = FcosImage::new(&cli.work_dir, platform.arch)
                .stream(stream)
                .variant(variant);
            let path = image.ensure().await?;
            println!("{}", path.display());
        }

        Commands::Ignition {
            sources,
            base,
            overlay,
            var,
            files_dir,
            butane,
            output,
        } => {
            fcos_harness::cli::ignition::run(
                sources,
                base,
                overlay,
                var,
                files_dir,
                butane,
                output,
                &cli.work_dir,
            )
            .await?;
        }

        Commands::Boot(args) => {
            let firmware = match cli.firmware {
                Some(ref fw) => fw.clone(),
                None => Platform::detect()?.discover_firmware()?,
            };
            fcos_harness::cli::boot::run(args, &cli.work_dir, &firmware).await?;
        }

        Commands::Start {
            disk,
            ignition,
            ssh_port,
            hostname,
            serial_log,
            qmp,
            loadvm,
            block_size,
            pid_file,
        } => {
            let platform = Platform::detect()?;
            let qemu_binary = platform.qemu_binary;
            let serial = serial_log.unwrap_or_else(|| cli.work_dir.join("serial.log"));

            // Clean up stale QMP socket
            if let Some(ref sock) = qmp {
                tokio::fs::remove_file(sock).await.ok();
            }

            // Build QEMU args using VmBuilder, then spawn detached
            let firmware = match cli.firmware {
                Some(ref fw) => fw.clone(),
                None => platform.discover_firmware()?,
            };
            let mut builder = fcos_harness::qemu::VmBuilder::new(platform, &firmware)
                .disk(&disk)
                .ssh_port(ssh_port)
                .hostname(&hostname)
                .serial_log(&serial);

            if let Some(ref ign) = ignition {
                builder = builder.ignition(ign);
            }
            if let Some(ref sock) = qmp {
                builder = builder.qmp_socket(sock);
            }
            if let Some(ref name) = loadvm {
                builder = builder.loadvm(name);
            }
            if let Some(bs) = block_size {
                builder = builder.block_size(bs);
            }

            let args = builder.build_args();

            // Spawn QEMU as a detached process (survives parent exit)
            let child = std::process::Command::new(qemu_binary)
                .args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .wrap_err_with(|| format!("failed to spawn {qemu_binary}"))?;

            let pid = child.id();

            // Write PID file
            if let Some(parent) = pid_file.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&pid_file, pid.to_string()).await?;

            // Brief pause to let QEMU initialize
            tokio::time::sleep(Duration::from_secs(2)).await;

            // Verify it's still running
            unsafe {
                if libc::kill(pid as i32, 0) != 0 {
                    bail!("QEMU failed to start (exited immediately)");
                }
            }

            info!(pid, pid_file = %pid_file.display(), "QEMU started");
            println!("{pid}");
        }

        Commands::Stop { pid_file } => {
            let pid_str = tokio::fs::read_to_string(&pid_file)
                .await
                .wrap_err_with(|| format!("failed to read PID file: {}", pid_file.display()))?;
            let pid: i32 = pid_str.trim().parse().wrap_err("invalid PID in file")?;

            info!(pid, "stopping QEMU");
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }

            // Wait briefly for process to exit
            for _ in 0..20 {
                unsafe {
                    if libc::kill(pid, 0) != 0 {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }

            tokio::fs::remove_file(&pid_file).await.ok();
        }

        Commands::Disk {
            base,
            overlay,
            size,
            backing_format,
        } => {
            fcos_harness::disk::create_overlay(&base, &overlay, &size, &backing_format).await?;
        }

        Commands::Qmp { socket, command } => {
            let client = QmpClient::new(&socket);
            match command {
                QmpCommand::Savevm { name } => client.savevm(&name).await?,
                QmpCommand::Quit => client.quit().await?,
            }
        }

        Commands::Goss {
            gossfile,
            ssh_port,
            ssh_key,
            retry_timeout_secs,
            sudo,
        } => {
            let platform = Platform::detect()?;
            let session = SshSession::new(SshConfig {
                port: ssh_port,
                identity_file: ssh_key,
                ..SshConfig::default()
            });

            Goss::new(&cli.work_dir, platform.arch)
                .sudo(sudo)
                .validate(
                    &session,
                    &gossfile,
                    Duration::from_secs(retry_timeout_secs),
                    Duration::from_secs(5),
                )
                .await?;
        }

        Commands::Ssh(args) => {
            fcos_harness::cli::ssh::run(args).await?;
        }
    }

    Ok(())
}
