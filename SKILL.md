# SKILL.md — Setting up fcos-harness for a new project

When asked to "set up fcos-harness for project X", follow this guide to create the full E2E test infrastructure. The result is a project that can boot a FCOS VM, validate it with goss, and run project-specific tests against it.

## What you're creating

```
project/
├── flake.nix               # Nix devShell with fcos-harness input
├── hack/
│   ├── dev/
│   │   ├── dev_ed25519     # SSH key for VM access (generate fresh)
│   │   └── dev_ed25519.pub
│   ├── init/
│   │   ├── Makefile         # Butane → Ignition compilation
│   │   ├── base.bu          # Base FCOS config (project-specific)
│   │   ├── users.bu         # SSH key injection for core user
│   │   └── config.bu        # Merges all .bu → config.ign
│   ├── goss.yaml            # VM validation tests
│   └── test.sh              # E2E test entrypoint
└── .gitignore               # Must include /tmp/ and hack/init/*.ign
```

## Step-by-step

### 1. Generate SSH keypair

```sh
mkdir -p hack/dev
ssh-keygen -t ed25519 -f hack/dev/dev_ed25519 -N "" -C "fcos-harness-dev"
```

Commit both the private and public key — this is a dev-only key for local VM testing.

### 2. Create Butane configs in `hack/init/`

**`hack/init/users.bu`** — always the same pattern:
```yaml
---
variant: "fcos"
version: "1.5.0"

passwd:
  users:
    - name: "core"
      ssh_authorized_keys_local:
        - "hack/dev/dev_ed25519.pub"

# vim: ft=yaml
```

**`hack/init/base.bu`** — project-specific FCOS configuration. Examples:
- Kernel arguments, systemd units, storage layout, rpm-ostree packages
- For btrfs: disk partitioning + filesystem + subvolume creation service (see subvault)
- For simple projects: can be just kernel args or empty aside from variant header

**`hack/init/config.bu`** — merge file, lists all .bu dependencies:
```yaml
---
variant: "fcos"
version: "1.5.0"

ignition:
  config:
    merge:
      - local: "base.ign"
      - local: "users.ign"

# vim: ft=yaml
```

Add more `- local:` entries for each additional .bu file (e.g. `storage.ign`).

### 3. Create Makefile in `hack/init/`

```makefile
BUTANE ?= butane

%.ign: %.bu
	$(BUTANE) --strict --files-dir ../.. < $< > $@

config.ign: base.ign users.ign
	$(BUTANE) --strict --files-dir . < config.bu > $@

.PHONY: clean
clean:
	rm -f *.ign
```

The `config.ign` dependency list must match the `merge:` entries in `config.bu`.
`--files-dir ../..` points at the repo root (for `ssh_authorized_keys_local` paths).

### 4. Create `hack/goss.yaml`

Minimal starting point:
```yaml
---
command:
  ssh-accessible:
    exec: "whoami"
    exit-status: 0
    stdout:
      - "core"
  sudo-works:
    exec: "sudo whoami"
    exit-status: 0
    stdout:
      - "root"
```

Add project-specific checks: services running, ports listening, mounts existing, files present.
Use `--sudo` on `fh goss` when checks need root (btrfs commands, systemctl, etc.).

### 5. Create `hack/test.sh`

There are two patterns. Choose based on whether the project needs fast VM restarts.

#### Pattern A: Snapshot caching (kerosene, subvault, swanny, syringe)

Use when: tests modify VM state or deploy binaries, and you want instant VM restore.
The script saves a QEMU snapshot after initial boot+goss, then restores from it for tests.

```bash
#!/usr/bin/env bash
# E2E test: boot a FCOS VM, validate with goss, run PROJECT tests.
# Uses QEMU savevm/loadvm to cache a booted VM snapshot for fast restarts.
#
# Env vars:
#   TEST_SSH_PORT      SSH port forward (default: 2223)
#   REBUILD_SNAPSHOT   Set to 1 to force snapshot recreation
#   KEEP_VM            Set to 1 to keep VM running after tests
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
work_dir="${root}/tmp/vm"
ssh_port="${TEST_SSH_PORT:-2223}"
ssh_key="${root}/hack/dev/dev_ed25519"

snapshot_disk="${work_dir}/fcos-snapshot.qcow2"
snapshot_name="ssh-ready"
snapshot_hash_file="${work_dir}/snapshot.hash"
monitor_sock="${work_dir}/qemu-monitor.sock"
pid_file="${work_dir}/qemu.pid"

chmod 600 "${ssh_key}"

fh() {
    fcos-harness --work-dir "${work_dir}" "$@"
}
fh_ssh() {
    fh ssh --ssh-key "${ssh_key}" --ssh-port "${ssh_port}" "$@"
}

# -- Build Ignition config --
make -C "${root}/hack/init" config.ign
ign="${root}/hack/init/config.ign"

# -- (Optional) Build project binary here --
# echo ">>> Building PROJECT..."
# cargo build --release 2>&1

# -- Ensure FCOS base image --
fh image

# -- Snapshot caching --
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

current_hash="$(sha256 "${ign}")"
use_snapshot=false

if [ "${REBUILD_SNAPSHOT:-}" != "1" ] \
    && [ -f "${snapshot_disk}" ] \
    && [ -f "${snapshot_hash_file}" ] \
    && [ "$(cat "${snapshot_hash_file}")" = "${current_hash}" ] \
    && qemu-img snapshot -l "${snapshot_disk}" 2>/dev/null | grep -q "${snapshot_name}"; then
    use_snapshot=true
    echo ">>> Valid VM snapshot found, skipping boot+goss"
fi

if [ "${use_snapshot}" = false ]; then
    echo ">>> Creating VM snapshot (first run or config changed)..."
    rm -f "${snapshot_disk}" "${snapshot_hash_file}"
    fh disk --base "${work_dir}/fcos.qcow2" --overlay "${snapshot_disk}"

    fh start \
        --disk "${snapshot_disk}" \
        --ignition "${ign}" \
        --ssh-port "${ssh_port}" \
        --hostname PROJECT-test \
        --serial-log "${work_dir}/serial-test.log" \
        --qmp "${monitor_sock}" \
        --pid-file "${pid_file}"

    cleanup_snapshot() {
        fh stop --pid-file "${pid_file}" 2>/dev/null || true
        rm -f "${monitor_sock}"
    }
    trap cleanup_snapshot EXIT

    echo ">>> Waiting for SSH..."
    fh_ssh --wait 180 -- true

    echo ">>> Running goss validation..."
    fh goss "${root}/hack/goss.yaml" --ssh-key "${ssh_key}" --ssh-port "${ssh_port}" --retry-timeout-secs 30

    echo ">>> Saving VM snapshot '${snapshot_name}'..."
    fh qmp --socket "${monitor_sock}" savevm "${snapshot_name}"

    echo ">>> Stopping snapshot VM..."
    fh qmp --socket "${monitor_sock}" quit
    sleep 1
    fh stop --pid-file "${pid_file}" 2>/dev/null || true
    rm -f "${monitor_sock}"
    trap - EXIT

    echo "${current_hash}" > "${snapshot_hash_file}"
    echo ">>> Snapshot created"
fi

# -- Boot from snapshot --
echo ">>> Booting test VM from snapshot..."
fh start \
    --disk "${snapshot_disk}" \
    --ignition "${ign}" \
    --ssh-port "${ssh_port}" \
    --hostname PROJECT-test \
    --serial-log "${work_dir}/serial-test.log" \
    --loadvm "${snapshot_name}" \
    --pid-file "${pid_file}"

cleanup() {
    echo ">>> Shutting down test VM..."
    fh stop --pid-file "${pid_file}" 2>/dev/null || true
}
trap cleanup EXIT

echo ">>> Waiting for SSH (should be instant from snapshot)..."
fh_ssh --wait 30 -- true

# -- Run project-specific tests here --
echo ">>> Running tests..."
# ...

echo ">>> All tests passed!"

# -- Keep VM running if requested --
if [ "${KEEP_VM:-}" = "1" ]; then
    echo ">>> VM is still running (ssh -p ${ssh_port} core@127.0.0.1)"
    echo ">>> Press Ctrl-C to stop..."
    trap cleanup INT
    wait "$(cat "${pid_file}")"
fi
```

#### Pattern B: Simple cold boot (fcos-k3s)

Use when: no snapshot needed, just boot → goss → done.

```bash
#!/usr/bin/env bash
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
work_dir="${root}/tmp/vm"
ssh_port="${TEST_SSH_PORT:-2223}"
ssh_key="${root}/hack/dev/dev_ed25519"   # or dev/ at repo root
pid_file="${work_dir}/qemu.pid"
diffdisk="${work_dir}/diff-test.qcow2"

fh() {
    fcos-harness --work-dir "${work_dir}" "$@"
}

# -- Build Ignition config (using fh ignition for overlays) --
fh ignition --base "${root}/init/config.ign" \
    --overlay "${root}/hack/overlay/users.bu" \
    --overlay "${root}/hack/overlay/hostname.bu" \
    -o "${work_dir}/config.ign"
ign="${work_dir}/config.ign"

# -- Ensure base image + overlay --
fh image
if ! [ -f "${diffdisk}" ]; then
    fh disk --base "${work_dir}/fcos.qcow2" --overlay "${diffdisk}"
fi

# -- Boot --
fh start \
    --disk "${diffdisk}" \
    --ignition "${ign}" \
    --ssh-port "${ssh_port}" \
    --hostname test \
    --serial-log "${work_dir}/serial-test.log" \
    --pid-file "${pid_file}"

cleanup() { fh stop --pid-file "${pid_file}" 2>/dev/null || true; }
trap cleanup EXIT

fh ssh --ssh-key "${ssh_key}" --ssh-port "${ssh_port}" --wait 180 -- true

# -- Validate --
fh goss "${root}/hack/goss.yaml" \
    --ssh-key "${ssh_key}" --ssh-port "${ssh_port}" \
    --retry-timeout-secs 300 --sudo

echo ">>> All tests passed"
```

### 6. Add fcos-harness to `flake.nix`

Add the input:
```nix
inputs = {
  # ... existing inputs ...
  fcos-harness.url = "github:ZentriaMC/fcos-harness";
  fcos-harness.inputs.nixpkgs.follows = "nixpkgs";
};
```

Add to `outputs` function args: `fcos-harness`

Add to `devShells.default` packages:
```nix
packages = [
  # ... project deps ...
  pkgs.butane
  pkgs.qemu
  fcos-harness.packages.${system}.default
];

BUTANE = "${pkgs.butane}/bin/butane";
```

### 7. Update `.gitignore`

Add:
```
/tmp/
hack/init/*.ign
```

## Variations

### Cross-compiled binary deployed to VM

For projects where the binary runs inside the VM (subvault, swanny, syringe):

1. Detect target arch:
   ```bash
   case "$(uname -s).$(uname -m)" in
       Linux.x86_64)   cargo_target="x86_64-unknown-linux-musl"   ;;
       Darwin.arm64)   cargo_target="aarch64-unknown-linux-musl"  ;;
       Linux.aarch64)  cargo_target="aarch64-unknown-linux-musl"  ;;
   esac
   ```
2. Cross-compile: `cargo zigbuild --release --target "${cargo_target}"`
3. SCP into VM after SSH is ready:
   ```bash
   scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
       -P "${ssh_port}" -i "${ssh_key}" \
       "${binary}" core@127.0.0.1:/usr/local/bin/name
   ```
4. For Rust cross-compile, add `pkgs.zig` to the Nix devShell and `cargo-zigbuild` to dependencies.
5. For Go: `GOOS=linux CGO_ENABLED=0 go build -o "${work_dir}/binary" ./cmd/binary`

### Metal 4K block size images

Pass `--variant metal4k` to `fh image` and `fh disk` commands. The overlay filenames are
automatically segregated (e.g. `diff-boot-4k.qcow2`). Useful for testing 4K-native storage.

### Overlay-based ignition (fcos-k3s pattern)

Instead of Makefile-compiled Butane, use `fh ignition` with `--base` and multiple `--overlay` flags.
Overlays are standalone `.bu` files that get merged at compile time. Use `--files-dir` to resolve
`local:` references. Good when the base ignition lives outside `hack/` or overlays are per-environment.

## Checklist

- [ ] `hack/dev/dev_ed25519{,.pub}` generated and committed
- [ ] `hack/init/base.bu` with project-specific FCOS config
- [ ] `hack/init/users.bu` with SSH key reference
- [ ] `hack/init/config.bu` merging all .bu files
- [ ] `hack/init/Makefile` with correct dependency list
- [ ] `hack/goss.yaml` with at least SSH + sudo checks
- [ ] `hack/test.sh` using appropriate pattern (snapshot or cold boot)
- [ ] `flake.nix` has fcos-harness input + butane/qemu in devShell
- [ ] `.gitignore` has `/tmp/` and `hack/init/*.ign`
- [ ] `chmod 600 hack/dev/dev_ed25519` (or done in test.sh)
