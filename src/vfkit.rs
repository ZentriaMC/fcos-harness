use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::{Context, bail};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tracing::{info, warn};

use crate::backend::Backend;
use crate::ssh::{SshConfig, SshSession};

/// Path where macOS's vmnet DHCP server writes its lease file.
/// Records have shape `{ name=...; ip_address=...; hw_address=1,XX:XX:..; ... }`.
const DHCPD_LEASES_PATH: &str = "/var/db/dhcpd_leases";

/// Builder for configuring and launching a vfkit VM.
pub struct VmBuilder {
    work_dir: PathBuf,
    disk: PathBuf,
    ignition: Option<PathBuf>,
    ssh_key: PathBuf,
    hostname: String,
    cpus: u32,
    memory_mib: u32,
    serial_log: PathBuf,
    rest_socket: PathBuf,
    efi_vars: PathBuf,
    pid_file: PathBuf,
    nested: bool,
    extra_args: Vec<String>,
}

impl VmBuilder {
    pub fn new(work_dir: impl Into<PathBuf>) -> Self {
        let work_dir = work_dir.into();
        Self {
            disk: PathBuf::new(),
            ignition: None,
            ssh_key: PathBuf::new(),
            hostname: "fcos-test".into(),
            cpus: 4,
            memory_mib: 4096,
            serial_log: work_dir.join("serial.log"),
            rest_socket: work_dir.join("vfkit.sock"),
            efi_vars: work_dir.join("efi-vars.fd"),
            pid_file: work_dir.join("vfkit.pid"),
            nested: false,
            extra_args: Vec::new(),
            work_dir,
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

    pub fn memory_mib(mut self, mib: u32) -> Self {
        self.memory_mib = mib;
        self
    }

    pub fn serial_log(mut self, path: impl Into<PathBuf>) -> Self {
        self.serial_log = path.into();
        self
    }

    pub fn rest_socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.rest_socket = path.into();
        self
    }

    pub fn efi_vars(mut self, path: impl Into<PathBuf>) -> Self {
        self.efi_vars = path.into();
        self
    }

    pub fn pid_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.pid_file = path.into();
        self
    }

    pub fn nested(mut self, enabled: bool) -> Self {
        self.nested = enabled;
        self
    }

    pub fn extra_arg(mut self, arg: impl Into<String>) -> Self {
        self.extra_args.push(arg.into());
        self
    }

    /// Build the full vfkit argument list (for debugging / dry-run).
    pub fn build_args(&self, mac: &str) -> eyre::Result<Vec<String>> {
        // vfkit's --restful-uri is a URI; for unix sockets it requires an absolute path
        // (the form is unix:///abs/path — three slashes). Resolve relative paths here.
        let rest_socket_abs = absolute_path(&self.rest_socket)?;

        let mut args = vec![
            "--bootloader".into(),
            format!("efi,variable-store={},create", self.efi_vars.display(),),
            "--cpus".into(),
            self.cpus.to_string(),
            "--memory".into(),
            self.memory_mib.to_string(),
            "--device".into(),
            format!("virtio-blk,path={}", self.disk.display()),
            "--device".into(),
            format!("virtio-net,nat,mac={mac}"),
            "--device".into(),
            "virtio-rng".into(),
            "--device".into(),
            format!("virtio-serial,logFilePath={}", self.serial_log.display()),
            "--restful-uri".into(),
            format!("unix://{}", rest_socket_abs.display()),
            "--pidfile".into(),
            self.pid_file.display().to_string(),
        ];

        if let Some(ref ign) = self.ignition {
            args.push("--ignition".into());
            args.push(ign.display().to_string());
        }

        if self.nested {
            args.push("--nested".into());
        }

        args.extend(self.extra_args.iter().cloned());
        Ok(args)
    }

    /// Launch vfkit as a background process and wait for the guest to acquire a DHCP lease.
    pub async fn launch(self) -> eyre::Result<Vm> {
        if !is_supported_platform() {
            bail!(
                "vfkit backend requires aarch64-darwin (current: {}/{})",
                std::env::consts::OS,
                std::env::consts::ARCH,
            );
        }

        // Ensure work dir and serial log parent exist.
        tokio::fs::create_dir_all(&self.work_dir).await?;
        if let Some(parent) = self.serial_log.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Clean up stale REST socket from a prior run.
        tokio::fs::remove_file(&self.rest_socket).await.ok();

        let mac = deterministic_mac(&self.work_dir, &self.hostname);
        let args = self.build_args(&mac)?;

        info!(
            mac,
            cpus = self.cpus,
            memory_mib = self.memory_mib,
            nested = self.nested,
            "launching vfkit VM"
        );

        let mut child = Command::new("vfkit")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .wrap_err("failed to spawn vfkit")?;

        // Brief pause to let vfkit initialize. If it exited immediately, surface that.
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Ok(Some(status)) = child.try_wait() {
            // Read whatever vfkit wrote to stderr to surface the underlying error.
            let stderr = if let Some(mut pipe) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let mut buf = String::new();
                pipe.read_to_string(&mut buf).await.ok();
                buf
            } else {
                String::new()
            };
            bail!(
                "vfkit exited immediately with status {status}\n--- vfkit stderr ---\n{}\n---",
                stderr.trim(),
            );
        }

        // Discover the guest IP from the vmnet DHCP lease file.
        let ip = match discover_guest_ip(&mac, Duration::from_secs(60)).await {
            Ok(ip) => ip,
            Err(err) => {
                let _ = child.start_kill();
                return Err(err);
            }
        };
        info!(mac, ip = %ip, "vfkit guest IP discovered");

        let ssh_config = SshConfig {
            host: ip.to_string(),
            port: 22,
            user: "core".into(),
            identity_file: self.ssh_key.clone(),
            ..SshConfig::default()
        };

        Ok(Vm {
            child,
            ssh_config,
            serial_log: self.serial_log,
            rest_socket: self.rest_socket,
            pid_file: self.pid_file,
            mac,
        })
    }
}

/// A running vfkit VM with lifecycle management.
pub struct Vm {
    child: tokio::process::Child,
    ssh_config: SshConfig,
    serial_log: PathBuf,
    rest_socket: PathBuf,
    pid_file: PathBuf,
    mac: String,
}

impl Vm {
    pub fn ssh(&self) -> SshSession {
        SshSession::new(self.ssh_config.clone())
    }

    pub fn ssh_config(&self) -> &SshConfig {
        &self.ssh_config
    }

    pub fn rest_socket(&self) -> &Path {
        &self.rest_socket
    }

    pub fn pid_file(&self) -> &Path {
        &self.pid_file
    }

    pub fn mac(&self) -> &str {
        &self.mac
    }

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

    pub fn is_running(&self) -> bool {
        self.child.id().is_some()
    }

    pub async fn wait(&mut self) -> eyre::Result<ExitStatus> {
        self.child.wait().await.wrap_err("failed to wait on vfkit")
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn detach(self) {
        std::mem::forget(self);
    }

    /// Gracefully stop the VM with SIGTERM, escalating to SIGKILL after a short grace.
    /// vfkit's signal handler releases the vmnet DHCP lease, so SIGTERM first is preferred.
    pub async fn shutdown(&mut self) -> eyre::Result<()> {
        info!("shutting down vfkit VM");
        if let Some(pid) = self.child.id() {
            // SIGTERM
            // SAFETY: pid is a valid process id from a tokio::process::Child we still own.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }

            // Grace period
            let grace = Duration::from_secs(5);
            match tokio::time::timeout(grace, self.child.wait()).await {
                Ok(_) => {}
                Err(_) => {
                    warn!("vfkit did not exit within {grace:?}, sending SIGKILL");
                    self.child.kill().await.ok();
                    self.child.wait().await.ok();
                }
            }
        }
        // Clean up the REST socket.
        tokio::fs::remove_file(&self.rest_socket).await.ok();
        Ok(())
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.child.id().is_some() {
            warn!("Vm dropped without explicit shutdown, terminating vfkit");
            if let Some(pid) = self.child.id() {
                // SAFETY: pid is a valid process id; we ignore the result.
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
            }
            // Best-effort fallback; we can't await in Drop.
            let _ = self.child.start_kill();
        }
    }
}

#[async_trait]
impl Backend for Vm {
    fn ssh(&self) -> SshSession {
        Vm::ssh(self)
    }

    fn ssh_config(&self) -> &SshConfig {
        Vm::ssh_config(self)
    }

    fn pid(&self) -> Option<u32> {
        Vm::pid(self)
    }

    async fn serial_tail(&self, lines: usize) -> eyre::Result<String> {
        Vm::serial_tail(self, lines).await
    }

    async fn shutdown(&mut self) -> eyre::Result<()> {
        Vm::shutdown(self).await
    }

    async fn wait(&mut self) -> eyre::Result<ExitStatus> {
        Vm::wait(self).await
    }

    fn detach(self: Box<Self>) {
        Vm::detach(*self);
    }
}

/// True if vfkit can run on this host (aarch64-darwin).
pub fn is_supported_platform() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

/// Resolve a path to an absolute one, without requiring it to exist.
/// Falls back to joining with the current directory for relative paths.
fn absolute_path(p: &Path) -> eyre::Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .wrap_err("failed to read current directory")?
            .join(p))
    }
}

/// Generate a deterministic locally-administered MAC address from a seed.
///
/// The first octet has the LAA bit (0x02) set and the multicast bit (0x01) cleared,
/// which avoids collisions with real hardware while remaining a valid unicast MAC.
fn deterministic_mac(work_dir: &Path, hostname: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(work_dir.display().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(hostname.as_bytes());
    let digest = hasher.finalize();

    let mut mac = [0u8; 6];
    mac.copy_from_slice(&digest[..6]);
    // Set LAA (bit 1), clear multicast (bit 0).
    mac[0] = (mac[0] & 0xFC) | 0x02;

    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
    )
}

/// Poll `/var/db/dhcpd_leases` until a lease for `mac` appears or the timeout expires.
async fn discover_guest_ip(mac: &str, timeout: Duration) -> eyre::Result<IpAddr> {
    let want = normalize_mac(mac);
    let start = Instant::now();
    let interval = Duration::from_millis(500);

    loop {
        if let Ok(content) = tokio::fs::read_to_string(DHCPD_LEASES_PATH).await
            && let Some(ip) = find_lease(&content, &want)
        {
            return Ok(ip);
        }

        if start.elapsed() >= timeout {
            bail!(
                "no DHCP lease found in {DHCPD_LEASES_PATH} for MAC {mac} after {}s",
                timeout.as_secs(),
            );
        }

        tokio::time::sleep(interval).await;
    }
}

/// Parse `/var/db/dhcpd_leases` looking for `mac`'s lease.
///
/// Format:
/// ```text
/// {
///     name=foo
///     ip_address=192.168.64.10
///     hw_address=1,e2:85:1d:67:90:32
///     identifier=...
///     lease=0x...
/// }
/// ```
fn find_lease(content: &str, want_mac: &str) -> Option<IpAddr> {
    let mut ip: Option<IpAddr> = None;
    let mut mac: Option<String> = None;

    for raw in content.lines() {
        let line = raw.trim();
        match line {
            "{" => {
                ip = None;
                mac = None;
            }
            "}" => {
                if let (Some(parsed_ip), Some(parsed_mac)) = (ip.take(), mac.take())
                    && parsed_mac == want_mac
                {
                    return Some(parsed_ip);
                }
            }
            _ => {
                if let Some(val) = line.strip_prefix("ip_address=") {
                    ip = val.trim().parse().ok();
                } else if let Some(val) = line.strip_prefix("hw_address=") {
                    // "1,e2:85:1d:67:90:32"
                    if let Some((_, m)) = val.split_once(',') {
                        mac = Some(normalize_mac(m.trim()));
                    }
                }
            }
        }
    }
    None
}

/// Lowercase and zero-pad each octet of a MAC address.
///
/// `/var/db/dhcpd_leases` sometimes writes octets without leading zeros (e.g. `e:85:...`),
/// so we normalize both the lease line and our generated MAC before comparing.
fn normalize_mac(mac: &str) -> String {
    mac.split(':')
        .map(|octet| {
            let cleaned = octet.trim();
            u8::from_str_radix(cleaned, 16)
                .map(|b| format!("{b:02x}"))
                .unwrap_or_else(|_| cleaned.to_lowercase())
        })
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn mac_has_laa_bit_set_and_multicast_clear() {
        let mac = deterministic_mac(Path::new("/tmp/vm"), "fcos-test");
        let first_octet = u8::from_str_radix(&mac[..2], 16).unwrap();
        assert_eq!(first_octet & 0x03, 0x02, "LAA set, multicast clear");
    }

    #[test]
    fn mac_is_deterministic() {
        let a = deterministic_mac(Path::new("/tmp/vm"), "host-a");
        let b = deterministic_mac(Path::new("/tmp/vm"), "host-a");
        assert_eq!(a, b);
    }

    #[test]
    fn different_inputs_produce_different_macs() {
        let a = deterministic_mac(Path::new("/tmp/vm"), "host-a");
        let b = deterministic_mac(Path::new("/tmp/vm"), "host-b");
        assert_ne!(a, b);
    }

    #[test]
    fn normalize_mac_pads_octets() {
        assert_eq!(normalize_mac("e:85:1d:67:90:32"), "0e:85:1d:67:90:32");
        assert_eq!(normalize_mac("E:85:1D:67:90:32"), "0e:85:1d:67:90:32");
        assert_eq!(normalize_mac("0e:85:1d:67:90:32"), "0e:85:1d:67:90:32");
    }

    #[test]
    fn parse_lease_file() {
        let content = r#"
{
    name=other
    ip_address=192.168.64.5
    hw_address=1,aa:bb:cc:dd:ee:ff
    identifier=...
    lease=0x12345
}
{
    name=mine
    ip_address=192.168.64.10
    hw_address=1,e:85:1d:67:90:32
    identifier=...
    lease=0x67890
}
"#;
        let ip = find_lease(content, "0e:85:1d:67:90:32").expect("lease found");
        assert_eq!(ip.to_string(), "192.168.64.10");
    }

    #[test]
    fn parse_lease_file_no_match() {
        let content = "{\n  ip_address=10.0.0.1\n  hw_address=1,aa:bb:cc:dd:ee:ff\n}\n";
        assert!(find_lease(content, "01:02:03:04:05:06").is_none());
    }
}
