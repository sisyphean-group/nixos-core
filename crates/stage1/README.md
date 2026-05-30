# Stage 1 initrd bootstrap

The `stage1` crate, exposed as `nixos-core stage-1-init`, or `stage-1-init`
directly when symlinked from the `nixos-core` binary, replaces the upstream
`stage-1-init.sh` initrd bootstrap script.

It is responsible for bringing the initrd environment far enough to mount the
real root filesystem and `switch_root` into stage 2. That includes early
pseudo-filesystem setup, device discovery, hooks, module loading, LVM, resume,
root mounting, optional fsck, persistence/copy-to-RAM handling, and final
process cleanup.

## Configuration model

Stage 1 configuration is assembled from NixOS-generated environment variables,
kernel command line parameters, and a small CLI override layer.

CLI overrides:

- `--target-root` / `-t`
- `--extra-utils`
- `--distro-name`

Common environment inputs include `targetRoot`, `extraUtils`, `kernelModules`,
`resumeDevice`, `resumeDevices`, `fsInfo`, hook paths such as
`preDeviceCommands`/`postMountCommands`, `earlyMountScript`, `HOST_ID`,
`checkJournalingFS`, and `distroName`.

Device management defaults to udev. It can be switched to BusyBox mdev with
`DEVICE_MANAGER=mdev`, or to mdevd with `DEVICE_MANAGER=mdevd`. Backend binary
paths can be overridden with `UDEV_BINARY`, `UDEVADM_BINARY`, `MDEV_BINARY`,
`MDEVD_BINARY`, `MDEVD_COLDPLUG_BINARY`, and `MDEV_CONF`; udev rules can be
provided with `udevRules`.

Kernel command line handling covers the expected stage1 controls: `root=`,
`init=`, `resume=`, `boot.shell_on_fail`, `boot.panic_on_fail`,
`boot.debug1devices`, `boot.debug1mounts`, `boot.copytoram`, persistence,
console, quiet/debug/trace toggles, and related `boot.*` parameters.

## How it works

Stage1 mounts `/proc` early so it can parse `/proc/cmdline`, then resolves the
effective configuration. It prepares PATH and optional `LD_LIBRARY_PATH` from
`extraUtils`, links extra-utils secrets, creates required directories and device
nodes, mounts essential pseudo filesystems, copies initrd secrets into `/run`,
creates required files, and writes `/etc/hostid` when `HOST_ID` is configured.

After the basic environment exists, the crate runs pre-device hooks, loads
kernel modules unless modprobe was disabled by cmdline, starts the selected
device manager, triggers device events, and waits for devices to settle. It then
runs pre-LVM hooks, activates LVM, provides the `boot.debug1devices` checkpoint,
runs post-device hooks, handles resume devices, and runs post-resume hooks.

Filesystem metadata from `fsInfo` is parsed before root mounting so `root=` can
fall back to it when needed. Once root is mounted, stage1 handles lustrate,
optionally checks additional filesystems, mounts non-root filesystems, applies
`earlyMountScript` `specialMount` entries, and runs post-mount hooks.

The final phase emits the `/dev/root` udev rule when using udev, handles
copy-to-RAM and persistence flows, stops the device manager, kills remaining
processes, provides the `boot.debug1mounts` checkpoint, and finally calls
`switch_root`.

## Mount behavior

Mount options are parsed into kernel flags plus remaining data options. Options
with the `x-` prefix are ignored for the kernel mount call, matching fstab-style
userspace option behavior, and pass-through data is preserved.

Regular-file-backed filesystem sources are mounted through loop devices set up
directly in Rust. Stage1 opens `/dev/loop-control`, asks the kernel for a free
loop device, attaches the backing file with loop ioctls, applies read-only and
autoclear flags where appropriate, then mounts the resulting loop device. This
keeps that path inside stage1 rather than delegating to external `mount`.

Fsck is intentionally selective. Journaling filesystems are skipped by default
unless configured otherwise, self-checking or unsupported filesystems are not
sent to generic fsck, and fsck return codes are interpreted according to their
bitmap semantics (corrected errors, reboot requested, unrepaired errors, and
tool failure are handled differently).

## Hooks and debug behavior

Hook files are optional and run via `sh` at fixed lifecycle points. Non-zero
hook exits are warned with context; critical stage failures still abort the
bootstrap path.

Failure behavior follows kernel policy. `boot.shell_on_fail` opens the recovery
shell path, `boot.panic_on_fail` escalates to panic behavior, and the default is
fatal logging. `boot.debug1devices` stops after device/LVM setup and
`boot.debug1mounts` stops after mount/process cleanup, deliberately giving
operators stable inspection points.

## Differences from the shell script

The goal is script compatibility where NixOS relies on it, but not literal shell
reimplementation for its own sake. The regular-file mount path is the clearest
example: stage1 uses the kernel loop APIs directly instead of shelling out to
`mount` and relying on util-linux loop handling.

Device management is also explicit: udev, mdev, and mdevd share a common
lifecycle in Rust, with backend-specific details contained inside their
implementations.
