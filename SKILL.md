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
Both use `fh up` / `fh down` which handle image, disk, VM startup, and SSH readiness in one step.

#### Pattern A: Snapshot caching (kerosene, subvault, swanny, syringe)

Use when: tests modify VM state or deploy binaries, and you want instant VM restore.
`fh up --snapshot` hashes the ignition config, saves a QEMU snapshot after first boot,
and restores from it on subsequent runs (until the config changes).

```bash
#!/usr/bin/env bash
# E2E test: boot a FCOS VM, validate with goss, run PROJECT tests.
#
# Env vars:
#   TEST_SSH_PORT           SSH port forward (default: 2223)
#   FCOS_HARNESS_SSH_KEY    SSH key (set below)
#   KEEP_VM                 Set to 1 to keep VM running after tests
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
export FCOS_HARNESS_WORK_DIR="${root}/tmp/vm"
export FCOS_HARNESS_SSH_KEY="${root}/hack/dev/dev_ed25519"
ssh_port="${TEST_SSH_PORT:-2223}"

chmod 600 "${FCOS_HARNESS_SSH_KEY}"

fh() { fcos-harness "$@"; }
fh_ssh() { fh ssh --ssh-key "${FCOS_HARNESS_SSH_KEY}" --ssh-port "${ssh_port}" "$@"; }

# -- Build Ignition config --
make -C "${root}/hack/init" config.ign
ign="${root}/hack/init/config.ign"

# -- (Optional) Build project binary here --
# echo ">>> Building PROJECT..."
# cargo build --release 2>&1

# -- Bring up VM (image + disk + start + wait SSH, with snapshot) --
fh up \
    --ignition "${ign}" \
    --hostname PROJECT-test \
    --snapshot ssh-ready \
    --snapshot-goss "${root}/hack/goss.yaml"

trap 'fh down' EXIT

# -- Run project-specific tests here --
echo ">>> Running tests..."
# fh_ssh -- command-on-vm ...

echo ">>> All tests passed!"

# -- Keep VM running if requested --
if [ "${KEEP_VM:-}" = "1" ]; then
    echo ">>> VM is still running (ssh -p ${ssh_port} core@127.0.0.1)"
    echo ">>> Press Ctrl-C to stop..."
    trap 'fh down' INT
    wait "$(cat "${FCOS_HARNESS_WORK_DIR}/qemu.pid")"
fi
```

#### Pattern B: Simple cold boot (fcos-k3s)

Use when: no snapshot needed, just boot → validate → done.

```bash
#!/usr/bin/env bash
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
export FCOS_HARNESS_WORK_DIR="${root}/tmp/vm"
export FCOS_HARNESS_SSH_KEY="${root}/hack/dev/dev_ed25519"
ssh_port="${TEST_SSH_PORT:-2223}"

chmod 600 "${FCOS_HARNESS_SSH_KEY}"

# -- Build Ignition config (using fh ignition for overlays) --
fcos-harness ignition --base "${root}/init/config.ign" \
    --overlay "${root}/hack/overlay/users.bu" \
    --overlay "${root}/hack/overlay/hostname.bu" \
    -o "${FCOS_HARNESS_WORK_DIR}/config.ign"

# -- Bring up VM --
fcos-harness up \
    --ignition "${FCOS_HARNESS_WORK_DIR}/config.ign" \
    --hostname test

trap 'fcos-harness down' EXIT

# -- Validate --
fcos-harness goss "${root}/hack/goss.yaml" \
    --ssh-key "${FCOS_HARNESS_SSH_KEY}" --ssh-port "${ssh_port}" \
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
