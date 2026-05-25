# `/etc` activation

The `setup-etc` crate, exposed as `nixos-core setup-etc`, or `setup-etc`
directly when symlinked from the `nixos-core` binary, replaces the upstream
`setup-etc.pl` script used by Nixpkgs to populate `/etc` from the active NixOS
generation.

Both implementations agree on the on-disk shape of `/etc`: `/etc/static`
indirection, pass-through symlinks, `direct-symlink` entries, and copied files
with explicit `mode`/`uid`/`gid` sidecars. Where they differ is the tracking
model and activation strategy.

## How it works

[smfh]: https://github.com/feel-co/smfh

Activation starts by atomically swapping `/etc/static` to the new generation.
After that, setup-etc performs a conservative cleanup walk over `/etc` to remove
stale links that still point into `/etc/static/` but no longer exist in this
generation. This mirrors the legacy Perl `cleanup` pass and remains useful as a
safety net for entries that were not created from our manifest.

The crate then walks the generation etc tree and builds an [smfh] manifest for
_every_ managed entry: pass-through symlinks (`/etc/foo` -> `/etc/static/foo`),
direct symlinks (`/etc/foo` -> `/nix/store/...`), and copied files with explicit
ownership/mode metadata. It writes that manifest to
`$NIXOS_CORE_STATE_DIR/etc-manifest.json.new`, asks smfh to diff against the
current `$NIXOS_CORE_STATE_DIR/etc-manifest.json`, applies the resulting
activation/deactivation transitions, and finally atomically renames the new
manifest into place.

After the manifest commit, setup-etc replays legacy `/etc/.clean` migration once
by removing all listed relative paths and deleting the file itself. Running this
after manifest activation keeps migration retry-safe: if activation fails,
Perl-era tracked files remain in place and can be retried on the next run. The
final step is touching `/etc/NIXOS`.

> [!NOTE]
> `$NIXOS_CORE_STATE_DIR/etc-manifest.json` is the source of truth for what
> nixos-core has written into `/etc`. It supersedes `/etc/.clean`, which only
> tracked copied files.

## Differences from the Perl script

### Tracking model

The Perl path tracked copied-file cleanup via `/etc/.clean`, while stale
pass-through symlinks were cleaned heuristically by scanning `/etc` for
`/etc/static` targets that no longer existed. `nixos-core` keeps a complete
manifest of all managed entry types and drives cleanup from explicit manifest
old/new diffs instead of relying on filesystem heuristics.

The dangling-link `/etc` walk remains intentionally as a compatibility and
safety mechanism for entries that originated outside manifest control (for
example previous Perl activation or external tooling).

### Activation atomicity

Both implementations use temp-file + rename when replacing existing entries, so
steady-state updates keep each path fully old or fully new. The practical
difference is first activation of copied files that do not yet exist.

`setup-etc.pl` writes to `$target.tmp`, sets mode/owner, then renames. smfh's
current first-activation path writes to final location first and then applies
`chmod`/`chown`, which introduces a brief first-write permission window
(typically default umask like `0644`) before configured mode is applied.

That window only exists once per path. Subsequent updates use atomic replace. On
NixOS, impact is usually limited because `/etc/shadow` is managed by
`update-users-groups`, and most sidecar-mode entries are not secret material.

### Idempotent re-runs

smfh uses content/target checks (BLAKE3 for copies, target checks for symlinks)
to skip already-correct entries, so idempotent reruns avoid unnecessary rewrites
and complete faster. Copied entries are still `clobber: true`, so local edits
are overwritten on generation switch, matching Perl behavior in spirit.

## Migration from Perl path

No manual migration is required. First nixos-core activation handles replay of
`/etc/.clean`, removes stale Perl-tracked copies and stale pass-through links,
and writes a fresh manifest. After that, activation is manifest-diff driven.

## State directory

The state directory is controlled by `NIXOS_CORE_STATE_DIR`, with
`/var/lib/nixos` as default for backward compatibility.

## Glossary/files

- `/etc/NIXOS`: tag file marking the filesystem as a NixOS root.
- `/etc/static`: symlink to the active generation etc tree.
- `$NIXOS_CORE_STATE_DIR/etc-manifest.json`: current manifest, rewritten each
  activation.
- `$NIXOS_CORE_STATE_DIR/etc-manifest.json.new`: temp manifest used for atomic
  rename.
