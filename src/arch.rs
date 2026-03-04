use std::path::{Path, PathBuf};

use eyre::{Context, bail};
use tracing::debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    /// Rust target triple for musl cross-compilation.
    pub fn musl_target(&self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-unknown-linux-musl",
            Self::Aarch64 => "aarch64-unknown-linux-musl",
        }
    }

    /// Architecture string used by goss releases (amd64/arm64).
    pub fn goss_arch(&self) -> &'static str {
        match self {
            Self::X86_64 => "amd64",
            Self::Aarch64 => "arm64",
        }
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareStyle {
    /// x86_64: `-drive if=pflash,file=FW,format=raw,unit=0,readonly=on`
    Pflash,
    /// aarch64: `-bios FW`
    Bios,
}

/// Detected host platform with QEMU configuration.
#[derive(Debug, Clone)]
pub struct Platform {
    pub arch: Arch,
    pub qemu_binary: &'static str,
    pub machine_type: &'static str,
    pub accel: &'static str,
    pub firmware_style: FirmwareStyle,
}

impl Platform {
    /// Auto-detect from the current host OS and architecture.
    pub fn detect() -> eyre::Result<Self> {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;

        match (os, arch) {
            ("linux", "x86_64") => Ok(Self {
                arch: Arch::X86_64,
                qemu_binary: "qemu-system-x86_64",
                machine_type: "q35",
                accel: "kvm",
                firmware_style: FirmwareStyle::Pflash,
            }),
            ("macos", "aarch64") => Ok(Self {
                arch: Arch::Aarch64,
                qemu_binary: "qemu-system-aarch64",
                machine_type: "virt",
                accel: "hvf",
                firmware_style: FirmwareStyle::Bios,
            }),
            ("linux", "aarch64") => Ok(Self {
                arch: Arch::Aarch64,
                qemu_binary: "qemu-system-aarch64",
                machine_type: "virt",
                accel: "kvm",
                firmware_style: FirmwareStyle::Bios,
            }),
            _ => bail!("unsupported platform: {os}/{arch}"),
        }
    }

    /// Build the firmware-related QEMU arguments.
    pub fn firmware_args(&self, fw_path: &Path) -> Vec<String> {
        let fw = fw_path.display().to_string();
        match self.firmware_style {
            FirmwareStyle::Pflash => vec![
                "-drive".into(),
                format!("if=pflash,file={fw},format=raw,unit=0,readonly=on"),
            ],
            FirmwareStyle::Bios => vec!["-bios".into(), fw],
        }
    }

    /// Auto-discover UEFI firmware by scanning QEMU's bundled firmware descriptors.
    ///
    /// Locates the QEMU binary via PATH, resolves symlinks (important for Nix),
    /// then parses `../share/qemu/firmware/*.json` to find a non-Secure-Boot UEFI
    /// firmware matching this platform's architecture and machine type.
    pub fn discover_firmware(&self) -> eyre::Result<PathBuf> {
        let output = std::process::Command::new("which")
            .arg(self.qemu_binary)
            .output()
            .wrap_err_with(|| format!("failed to locate {}", self.qemu_binary))?;

        if !output.status.success() {
            bail!("{} not found in PATH", self.qemu_binary);
        }

        let bin_path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        let bin_path = std::fs::canonicalize(&bin_path)
            .wrap_err_with(|| format!("failed to resolve {}", bin_path.display()))?;

        let firmware_dir = bin_path
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("share/qemu/firmware"))
            .ok_or_else(|| eyre::eyre!("unexpected QEMU binary path structure"))?;

        if !firmware_dir.is_dir() {
            bail!(
                "firmware descriptor directory not found: {}",
                firmware_dir.display()
            );
        }

        let arch_str = self.arch.as_str();

        for entry in std::fs::read_dir(&firmware_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            let content = std::fs::read_to_string(&path)
                .wrap_err_with(|| format!("failed to read {}", path.display()))?;
            let desc: serde_json::Value = serde_json::from_str(&content)
                .wrap_err_with(|| format!("failed to parse {}", path.display()))?;

            // Must be UEFI
            let interfaces = desc["interface-types"].as_array();
            if !interfaces.is_some_and(|a| a.iter().any(|v| v.as_str() == Some("uefi"))) {
                continue;
            }

            // Must not require SMM (Secure Boot)
            let features = desc["features"].as_array();
            if features.is_some_and(|a| a.iter().any(|v| v.as_str() == Some("requires-smm"))) {
                continue;
            }

            // Must match our architecture and machine type
            let targets = desc["targets"].as_array();
            let matches = targets.is_some_and(|arr| {
                arr.iter().any(|t| {
                    let arch_ok = t["architecture"].as_str() == Some(arch_str);
                    let machine_ok = t["machines"].as_array().is_some_and(|machines| {
                        machines.iter().any(|m| {
                            // Glob patterns like "pc-q35-*" or "virt-*" — check if
                            // the base (minus trailing glob) contains our machine type
                            m.as_str().is_some_and(|pattern| {
                                pattern.trim_end_matches('*').trim_end_matches('-').contains(self.machine_type)
                            })
                        })
                    });
                    arch_ok && machine_ok
                })
            });
            if !matches {
                continue;
            }

            if let Some(filename) = desc["mapping"]["executable"]["filename"].as_str() {
                let fw_path = PathBuf::from(filename);
                if fw_path.exists() {
                    debug!(path = %fw_path.display(), "auto-discovered UEFI firmware");
                    return Ok(fw_path);
                }
            }
        }

        bail!(
            "no suitable UEFI firmware found for {} in {}",
            arch_str,
            firmware_dir.display()
        )
    }

    /// Build the machine-related QEMU arguments.
    pub fn machine_args(&self) -> Vec<String> {
        vec![
            "-machine".into(),
            format!("{},accel={}", self.machine_type, self.accel),
        ]
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn detect_returns_valid_platform() {
        let platform = Platform::detect();
        // Should succeed on any supported CI/dev machine
        assert!(platform.is_ok(), "detect() failed: {platform:?}");
        let p = platform.unwrap();
        assert!(!p.qemu_binary.is_empty());
    }

    #[test]
    fn firmware_args_pflash() {
        let platform = Platform {
            arch: Arch::X86_64,
            qemu_binary: "qemu-system-x86_64",
            machine_type: "q35",
            accel: "kvm",
            firmware_style: FirmwareStyle::Pflash,
        };
        let args = platform.firmware_args(Path::new("/path/to/edk2.fd"));
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "-drive");
        assert!(args[1].contains("pflash"));
    }

    #[test]
    fn firmware_args_bios() {
        let platform = Platform {
            arch: Arch::Aarch64,
            qemu_binary: "qemu-system-aarch64",
            machine_type: "virt",
            accel: "hvf",
            firmware_style: FirmwareStyle::Bios,
        };
        let args = platform.firmware_args(Path::new("/path/to/edk2.fd"));
        assert_eq!(args, vec!["-bios", "/path/to/edk2.fd"]);
    }

    #[test]
    fn arch_goss_arch() {
        assert_eq!(Arch::X86_64.goss_arch(), "amd64");
        assert_eq!(Arch::Aarch64.goss_arch(), "arm64");
    }

    #[test]
    fn arch_musl_target() {
        assert_eq!(Arch::X86_64.musl_target(), "x86_64-unknown-linux-musl");
        assert_eq!(Arch::Aarch64.musl_target(), "aarch64-unknown-linux-musl");
    }
}
