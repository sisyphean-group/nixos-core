# Stage 2 activation and handoff

The `stage2` crate, exposed as `nixos-core stage-2-init`, or `stage-2-init`
directly when symlinked from the `nixos-core` binary, replaces the upstream
`stage-2-init.sh` script used to finish activation and hand off to systemd.

Default behavior is intentionally bash-compatible. Additional behavior borrowed
from `nixos-init` exists, but is opt-in through command-line flags and, where
needed, compile-time features.

## How it works

Stage 2 reads most configuration from CLI flags with environment bindings. The
required input is the system configuration path (`--system-config` /
`SYSTEM_CONFIG`). Other core inputs include the greeting, `/nix/store` mount
options, systemd executable path, post-boot hook path, post-boot shell, early
mount script path, PATH value, host `resolv.conf` behavior, and strict
activation behavior.

The bash-compatible path remounts `/` read-write when applicable, mounts the
special filesystems (`/proc`, `/dev`, `/sys`, `/dev/pts`, `/dev/shm`) or runs
the Nix-generated early mount script, prepares `/nix/store`, creates required
runtime directories, runs the system activation script, creates
`/run/booted-system`, and runs post-boot commands when configured.

After core activation, optional compatibility toggles can apply extra setup:
atomic recreation of `/run/booted-system`, creation of `/run/current-system`,
FHS links (`/usr/bin/env`, `/bin/sh`), `/proc/sys/kernel/modprobe`, and firmware
search path setup.

`run()` performs activation/setup only. `run_and_handoff()` performs activation
and then transfers control to the init path. In the normal path, handoff is a
raw exec of the configured systemd executable. When
`IN_NIXOS_SYSTEMD_STAGE1=true`, stage2 exits cleanly instead, because the
systemd initrd switch-root unit owns the final transition.

## Compile-time features

The crate has no optional default features.

- `bootspec`: enables `--use-bootspec` and `--bootspec-path`. At the moment,
  bootspec parsing is informational unless paired with other opt-in behavior.
- `systemd-integration`: enables `--use-systemctl-handoff`.
- `full-nixos-init-compat`: enables both `bootspec` and `systemd-integration`.

If `--use-systemctl-handoff` is requested and compiled in, stage2 tries
`systemctl switch-root` before falling back to raw exec on failure.

## Runtime options

Core options:

- `--system-config` / `SYSTEM_CONFIG`
- `--greeting` / `STAGE2_GREETING`
- `--nix-store-mount-opts` / `NIX_STORE_MOUNT_OPTS`
- `--systemd-executable` / `SYSTEMD_EXECUTABLE`
- `--post-boot-commands` / `POST_BOOT_COMMANDS`
- `--post-boot-shell` / `POST_BOOT_SHELL`
- `--early-mount-script` / `EARLY_MOUNT_SCRIPT`
- `--use-host-resolv-conf` / `USE_HOST_RESOLV_CONF`
- `--path` / `STAGE2_PATH`
- `--strict-activation` / `STAGE2_STRICT_ACTIVATION`

Opt-in compatibility options:

- `--atomic-symlinks`
- `--create-current-system`
- `--setup-fhs` with `--env-binary` and `--sh-binary`
- `--setup-modprobe` with `--modprobe-binary`
- `--setup-firmware` with `--firmware-path`

Trailing arguments are passed through unchanged to systemd, matching the old
script's `exec systemd "$@"` behavior.

## Differences from nixos-init

`nixos-init` uses bootspec and systemctl-based switch-root as first-class
behavior. This crate keeps the scripted-init contract as the baseline because
that is still useful for non-systemd initrd users and for gradual replacement of
the legacy scripts. The **nixos-init-style pieces are available** where they
make sense, but they do not silently change the default activation path.

## Failure semantics

Missing activation script is a warning by default so non-NixOS or partial
targets can still complete stage 2. With `--strict-activation`, a missing
`$systemConfig/activate` is fatal.

`/nix/store` ownership and mount option setup attempts to preserve the old
script's behavior while tolerating read-only or 9p-mounted stores in VM-like
environments where those operations may fail.
