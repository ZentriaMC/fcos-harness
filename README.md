# fcos-harness

FCOS + QEMU integration test harness. Library and CLI for managing Fedora CoreOS VM lifecycle in end-to-end tests.

## Install

```
cargo install --locked --git https://github.com/ZentriaMC/fcos-harness
```

Requires `qemu` and `butane` at runtime — provided by the Nix dev shell (`nix develop`).

## CLI

```
fcos-harness image                          # download/cache FCOS qcow2
fcos-harness disk --base IMG --overlay OUT  # create qcow2 CoW overlay
fcos-harness ignition src.bu -o out.ign     # compile butane → ignition
fcos-harness start --disk IMG --pid-file PID --ssh-port 2223  # boot VM (background)
fcos-harness stop --pid-file PID            # kill VM
fcos-harness ssh --ssh-key KEY -- CMD       # exec over SSH
fcos-harness goss goss.yaml --ssh-key KEY   # deploy + run goss validation
fcos-harness qmp --socket SOCK savevm NAME  # save QEMU snapshot
fcos-harness qmp --socket SOCK quit         # quit QEMU
```

`QEMU_EFI_FW` is read from the environment (set by the Nix dev shell).

## Usage in a test script

```bash
#!/usr/bin/env bash
set -euo pipefail

work_dir="tmp/vm"
ssh_key="dev/dev_ed25519"

fh() { fcos-harness --work-dir "${work_dir}" "$@"; }

# Prepare
fh image > /dev/null
fh disk --base "${work_dir}/fcos.qcow2" --overlay "${work_dir}/diff.qcow2"

# Boot
fh start --disk "${work_dir}/diff.qcow2" --ignition config.ign \
    --pid-file "${work_dir}/qemu.pid"
trap 'fh stop --pid-file "${work_dir}/qemu.pid"' EXIT

# Wait + validate
fh ssh --ssh-key "${ssh_key}" --wait 180 -- true
fh goss goss.yaml --ssh-key "${ssh_key}"

# Run project-specific tests...
```

### Snapshot caching

For faster iteration, save a VM snapshot after initial boot + validation and restore it on subsequent runs:

```bash
fh start --disk snapshot.qcow2 --ignition config.ign \
    --qmp "${work_dir}/qmp.sock" --pid-file "${work_dir}/qemu.pid"

fh ssh --ssh-key "${ssh_key}" --wait 180 -- true
fh goss goss.yaml --ssh-key "${ssh_key}"

fh qmp --socket "${work_dir}/qmp.sock" savevm ssh-ready
fh qmp --socket "${work_dir}/qmp.sock" quit

# Next run: instant restore
fh start --disk snapshot.qcow2 --loadvm ssh-ready --pid-file "${work_dir}/qemu.pid"
```

## Library

```rust
use std::time::Duration;
use fcos_harness::{Platform, FcosImage, VmBuilder, Goss};

#[tokio::test]
async fn e2e() -> eyre::Result<()> {
    let platform = Platform::detect()?;
    let work_dir = "tmp/vm";
    let fw = std::env::var("QEMU_EFI_FW")?;

    let base = FcosImage::new(work_dir, platform.arch).ensure().await?;
    fcos_harness::disk::create_overlay(&base, "tmp/vm/diff.qcow2".as_ref(), "32G").await?;

    let mut vm = VmBuilder::new(platform.clone(), &fw)
        .disk("tmp/vm/diff.qcow2")
        .ignition("config.ign")
        .ssh_key("dev/dev_ed25519")
        .snapshot_mode(true)
        .launch()
        .await?;

    let ssh = vm.ssh();
    ssh.wait_ready(Duration::from_secs(180), Duration::from_secs(5)).await?;

    Goss::new(work_dir, platform.arch)
        .validate(&ssh, "goss.yaml".as_ref(), Duration::from_secs(60), Duration::from_secs(5))
        .await?;

    vm.shutdown().await
}
```

## Modules

| Module | Purpose |
|--------|---------|
| `arch` | Platform detection → QEMU binary, machine type, accel, firmware |
| `fcos` | FCOS image download with progress bar, SHA256 verify, XZ decompress |
| `disk` | qcow2 CoW overlay creation, snapshot existence check |
| `ignition` | Butane compilation with minijinja templating and Ignition merge |
| `qemu` | `VmBuilder` → `Vm` with full QEMU arg construction and lifecycle |
| `ssh` | `SshSession` with exec, upload, download, readiness polling |
| `goss` | Goss binary download, deploy to VM, run validation |
| `qmp` | QMP client (savevm/quit) + `SnapshotCache` for hash-based invalidation |

## Platform support

| Host | QEMU | Accel |
|------|------|-------|
| Linux x86_64 | `qemu-system-x86_64`, q35 | KVM |
| Linux aarch64 | `qemu-system-aarch64`, virt | KVM |
| macOS aarch64 | `qemu-system-aarch64`, virt | HVF |
