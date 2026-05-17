# fcos-harness

FCOS integration test harness. Library and CLI for managing Fedora CoreOS VM
lifecycle in end-to-end tests, with **two hypervisor backends**:

- **QEMU** (default, cross-platform) — `qemu-system-x86_64`/`-aarch64` with
  KVM on Linux, HVF on macOS
- **vfkit** (aarch64-darwin only) — Apple's Virtualization.framework, the only
  way to run **nested virtualization** on Apple Silicon (M3+ on macOS 15+)

## Install

```
cargo install --locked --git https://github.com/ZentriaMC/fcos-harness
```

Runtime dependencies (provided by the Nix dev shell, `nix develop`):
- `butane` (always)
- `qemu` (for the QEMU backend)
- `vfkit` (for the vfkit backend; aarch64-darwin only)

## CLI

```
fcos-harness ignition src.bu -o out.ign        # compile butane → ignition
fcos-harness up --ignition out.ign --ssh-key K # image + disk + boot + wait SSH
fcos-harness ssh -- cmd                        # exec over SSH (auto-discovers endpoint)
fcos-harness goss goss.yaml --ssh-key K        # deploy + run goss validation
fcos-harness down                              # tear down the VM
```

`fh up` is the high-level lifecycle command — downloads the image, creates
the disk, launches the VM, waits for SSH. It writes `<work_dir>/vm-state.json`
(host/port/user) so subsequent `fh ssh` invocations work without arguments.

### Backend selection

```
fh up                              # QEMU (default)
fh up --backend vfkit              # vfkit on macOS aarch64
fh up --nested                     # nested virt — auto-picks vfkit on macOS aarch64, qemu elsewhere
fh up --backend vfkit --nested     # explicit vfkit + nested
```

QEMU on Linux exposes nested virt automatically via `-cpu host` (no extra
flag); on macOS, HVF doesn't support nested virt at all — use vfkit.

### Snapshot caching

For faster iteration, `--snapshot <name>` hashes the ignition config and
caches state across runs. Hash mismatch (e.g. config edited) invalidates.

```
fh up --ignition c.ign --ssh-key K --snapshot ready --snapshot-goss g.yaml
```

| Backend | Mechanism                                              | First run         | Cached run                              |
|---------|--------------------------------------------------------|-------------------|-----------------------------------------|
| QEMU    | QMP `savevm` / `loadvm` (memory snapshot)              | full boot         | sub-second restore                      |
| vfkit   | Warmed-disk: guest poweroff + APFS clone + relaunch    | full boot + warm  | cold boot from warmed disk (~50% faster) |

### Embedding ssh options for scp/rsync

```
scp $(fh ssh --emit-opts) ./binary _:/usr/local/bin/foo
ssh $(fh ssh --emit-opts) _ -- 'systemctl status foo'
```

`--emit-opts` prints `-oHostname=… -oPort=… -oUser=… -oIdentityFile=… …`.
The `_` placeholder is overridden by `-oHostname=`.

## Test script template

```bash
#!/usr/bin/env bash
set -euo pipefail

export FCOS_HARNESS_WORK_DIR="tmp/vm"
export FCOS_HARNESS_SSH_KEY="dev/dev_ed25519"

fh() { fcos-harness "$@"; }

fh ignition config.bu -o "${FCOS_HARNESS_WORK_DIR}/config.ign"
fh up --ignition "${FCOS_HARNESS_WORK_DIR}/config.ign" \
      --snapshot ready --snapshot-goss goss.yaml
trap 'fh down' EXIT

fh ssh -- 'sudo systemctl is-active my-service'
# project-specific tests...
```

See `SKILL.md` for the full project-setup guide (Butane configs, Makefile,
gitignore, etc.) and `CLAUDE.md` for architecture / design notes.

## Library

Most consumers should use the `up`/`down` CLI, but the library API is
backend-agnostic via the `Backend` trait:

```rust
use fcos_harness::backend::Backend;

let vm: Box<dyn Backend> = /* qemu::VmBuilder::new(…).launch().await? */;
let ssh = vm.ssh();
ssh.wait_ready(
    std::time::Duration::from_secs(180),
    std::time::Duration::from_secs(5),
).await?;
```

See `src/cli/up.rs` for the canonical wiring of either backend.

## Modules

| Module      | Purpose |
|-------------|---------|
| `arch`      | Platform detection → QEMU binary, machine type, accel, firmware |
| `backend`   | `Backend` trait + `BackendKind` enum (`Qemu` / `Vfkit`) |
| `fcos`      | FCOS image download (xz/gz), SHA256 verify, versioned cache |
| `disk`      | qcow2 overlay creation (qemu-img) + APFS `clonefile` helper |
| `ignition`  | Butane compilation with minijinja templating + Ignition merge |
| `qemu`      | `VmBuilder` → `Vm` for QEMU + `Backend` impl |
| `vfkit`     | `VmBuilder` → `Vm` for vfkit + `Backend` impl (macOS aarch64) |
| `qmp`       | QMP client (savevm/quit) — QEMU only |
| `snapshot`  | `SnapshotCache` with hash-based invalidation; `QcowInternal` or `ExternalDisk` |
| `state`     | `vm-state.json` — SSH endpoint written by `up`, read by `ssh` |
| `ssh`       | `SshSession` with exec, upload, download, readiness polling |
| `goss`      | Goss binary download, deploy to VM, run validation |

## Platform support

| Host          | QEMU backend                         | vfkit backend       |
|---------------|--------------------------------------|---------------------|
| Linux x86_64  | `qemu-system-x86_64`, q35, KVM       | n/a                 |
| Linux aarch64 | `qemu-system-aarch64`, virt, KVM     | n/a                 |
| macOS aarch64 | `qemu-system-aarch64`, virt, HVF     | M3+ on macOS 15+    |

Nested virtualization:
- **Linux/KVM**: automatic via `-cpu host` (provided host kernel has `nested=1`)
- **macOS/HVF (QEMU)**: not supported by Apple's hypervisor framework
- **macOS/vfkit on M3+**: pass `--nested`
