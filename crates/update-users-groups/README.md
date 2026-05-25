# User and group activation

The `update-users-groups` crate, exposed as `nixos-core update-users-groups`, or
`update-users-groups` directly when symlinked from the `nixos-core` binary,
replaces the upstream `update-users-groups.pl` script used by Nixpkgs to
reconcile declarative users and groups.

It owns the activation-time contents of `/etc/passwd`, `/etc/group`,
`/etc/shadow`, `/etc/subuid`, and `/etc/subgid`, using the Nix-generated JSON
spec as input. The implementation keeps the same broad contract as the Perl
script: declarative users/groups are applied, mutable local state is preserved
only where `mutableUsers` permits it, and automatically allocated IDs remain
stable across activations.

## How it works

Activation starts by determining whether this is a dry activation. Dry mode is
enabled either with `--dry-activate` or `NIXOS_ACTION=dry-activate`. The crate
then resolves its state directory from `NIXOS_CORE_STATE_DIR`, falling back to
`/var/lib/nixos` for compatibility with existing NixOS state.

The persistent allocator files are loaded first: `uid-map`, `gid-map`, and
`auto-subuid-map`. The declarative ownership lists (`declarative-users`,
`declarative-groups`) are loaded alongside them so the mutable-user merge rules
can distinguish declarative entries from local ones. After that, the JSON spec
is parsed and the current `/etc/group` and `/etc/passwd` files are read to
establish the existing system state.

Groups are reconciled before users. Explicit `gid` values are reserved, missing
GIDs are allocated from the system range while avoiding conflicts, and group
members are merged according to `mutableUsers`. Users are then reconciled
against the effective group set: explicit `uid` values are respected, missing
UIDs are allocated in the correct system/normal range, primary groups are
resolved, and home/shell/password/expiry fields are applied.

Finally, the crate rebuilds `/etc/shadow`, `/etc/subuid`, and `/etc/subgid`,
writes the updated state maps, and invalidates `nscd` caches on best effort.
Managed output is deterministic, so repeated activations do not reorder files
without an actual input change.

## Allocation model

The allocator treats explicit IDs in the current spec as already claimed before
assigning anything automatically. Auto-allocated IDs also avoid IDs persisted in
the previous maps and IDs already claimed by NSS (`getpwuid` / `getgrgid`).

This matters after partial edits and local changes: removing a declarative user
and adding another one should not cause allocator drift or silently reuse an ID
that the system already knows about.

Subordinate ID ranges follow the same persistence model through
`auto-subuid-map`. Automatically assigned ranges are stable unless a conflict is
detected, in which case the crate warns and assigns a non-conflicting range.

## Password and shadow handling

The input spec can provide `hashedPassword`, `initialPassword`,
`initialHashedPassword`, `password`, or `hashedPasswordFile`. The effective
hashed password is carried forward into shadow generation, with direct password
fields hashed during activation. Existing shadow entries are preserved where
mutable semantics require it.

`expires` values are parsed and written into the shadow expiry field. Invalid
expiry dates are fatal because writing a malformed shadow file would be worse
than aborting activation.

After writing `/etc/shadow`, ownership is set to `root:shadow` when the
effective group set contains `shadow`; otherwise it falls back to gid `0`.

## Mutable users behavior

`mutableUsers` controls how local, non-declarative account state is treated.

Declarative users and groups remain authoritative either way. With mutable mode
enabled, compatible local state is retained for entries that are not owned by
the declarative configuration. Without mutable mode, declarative state is the
strict source of truth and previously declarative-but-now-missing entries are
removed.

## State directory

The state directory is controlled by `NIXOS_CORE_STATE_DIR`, defaulting to
`/var/lib/nixos`.

Files used by this crate:

- `uid-map`: persistent user ID allocations.
- `gid-map`: persistent group ID allocations.
- `declarative-users`: users owned by the previous declarative activation.
- `declarative-groups`: groups owned by the previous declarative activation.
- `auto-subuid-map`: persistent automatically assigned subordinate ID ranges.

These files are activation state and should not be edited manually.

## Failure semantics

Spec parse errors, invalid date values, unresolved required groups, file write
failures, and reconciliation errors are fatal. Dry mode still performs the
reconciliation work, but reports planned changes without mutating system files.

`nscd` invalidation is intentionally non-fatal: `nscd` may not be installed or
running on the target system.
