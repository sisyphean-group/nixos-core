# Activation common utilities

The `activation-common` crate is an internal shared library for activation
components. It is not exposed as a user-facing multicall command. Its current
purpose is deliberately small: provide shared mount-state helpers so stage and
activation crates do not each grow their own `/proc/mounts` parsing rules.

## How it works

`is_mounted(path)` opens `/proc/mounts`, decodes mountpoint fields, and returns
whether the requested path is currently mounted. `get_mount_options(path)` reads
the same source and returns the comma-separated mount options for the matching
mountpoint. If the mountpoint cannot be found, it returns an error instead of
pretending the path is unmounted or optionless.

> [!NOTE]
> Mount paths in `/proc/mounts` can contain octal escapes such as `\040` for
> spaces. `activation-common` decodes those before comparing paths, so callers
> can pass normal filesystem paths and get consistent behavior.

## Why this exists

Stage 2 already uses these helpers to decide whether special filesystems are
mounted and to preserve existing mount flags while remounting `/` or applying
`/nix/store` options. Keeping that logic in one crate avoids subtle differences
between activation paths.

The crate should stay boring: shared helpers belong here when they are genuinely
cross-cutting and behavior-sensitive, not just because two call sites happen to
look similar.
