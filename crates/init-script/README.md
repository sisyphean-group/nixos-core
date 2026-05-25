# Generic init script builder

The `init-script` crate, exposed as `nixos-core init-script` (with
`init-script-builder` accepted as an alias), replaces the upstream
`init-script-builder.sh` script that creates the generic `/sbin/init` launcher
and the boot-time list of alternative configurations.

The generated files preserve the old shape: `/sbin/init` execs the default
configuration's `init`, while `/boot/init-other-configurations-contents.txt`
contains runnable stubs for the default entry, specialisations, and older
generations.

## How it works

The command takes one positional argument: the default system configuration path
(`/nix/store/...-system`). The parent directory of that path is treated as the
system directory for specialisation discovery.

Activation first ensures `/boot` and `/sbin` exist. It then checks whether
`/boot` and `/` live on the same filesystem and prints a warning when they do
not. This mirrors the old script's concern: the generated boot list lives under
`/boot`, so a separate `/boot` filesystem can change what is visible during
early boot.

The entry list is built from three sources. The default entry is always added
first and points at `<default-config>/init`. Specialisations are discovered from
`<system-dir>/specialisation/*/init`. Historical generations are discovered from
`/nix/var/nix/profiles/system-<N>-link/init`.

Generation entries are labelled with their generation number, mtime formatted as
UTC, and detected kernel version. Kernel version detection follows the
generation's `kernel` path into its `lib/modules` directory and selects the
highest version directory using numeric version comparison rather than simple
lexicographic sorting.

Default and specialisation entries stay at the top of the output. Numbered
generations are sorted newest-first by generation number.

## Output files

`/sbin/init` is written through `/sbin/init.tmp`, chmodded to `0755`, and then
renamed into place. The script body is deliberately small: it records the
default label as a comment, then execs the default generation's init.

`/boot/init-other-configurations-contents.txt` is also written through a temp
file and renamed into place. Each discovered entry becomes a standalone shell
stub, matching the shape expected by the existing boot tooling.

## Differences from the shell script

The Rust implementation keeps the same output contract, but is stricter about
handling malformed generation metadata locally. If a historical generation
cannot produce its label suffix (for example because the kernel/modules layout
is unexpected), that generation is skipped with a warning instead of failing the
entire run.

Required operations still fail the command: creating output directories, writing
temp files, setting `/sbin/init` permissions, and renaming final output must
succeed.

## Glossary/files

- `/sbin/init`: generic init launcher for the default system.
- `/boot/init-other-configurations-contents.txt`: generated shell snippets for
  boot-menu alternatives.
- `/nix/var/nix/profiles/system-<N>-link`: historical system generation link.
- `<system-dir>/specialisation/*/init`: specialisation init entries.
