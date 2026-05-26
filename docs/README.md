<!-- markdownlint-disable MD013 MD033-->

<h1 id="header" align="center">
    <pre>nixos-core</pre>
</h1>

<div align="center">
    <a alt="CI" href="https://github.com/sisyphean-group/nixos-core/actions">
        <img
          src="https://github.com/sisyphean-group/nixos-core/actions/workflows/rust.yml/badge.svg"
          alt="Build Status"
        />
    </a>
    <a alt="Dependencies" href="https://deps.rs/repo/github/sisyphean-group/nixos-core">
        <img
          src="https://deps.rs/repo/github/sisyphean-group/nixos-core/status.svg"
          alt="Dependency Status"
        />
    </a>
    <a alt="License" href="https://github.com/sisyphean-group/nixos-core/blob/master/LICENSE">
        <img
          src="https://img.shields.io/github/license/sisyphean-group/nixos-core?label=License"
          alt="License"
        />
    </a>
</div>

<div align="center">
    <code>nixos-core</code> is a multi-call binary implementing core NixOS system utilities in
    safe, portable Rust, replacing some of the Perl, Bash and Python scripts that
    are generally load-bearing for safety and performance. While some of those
    scripts _can_ be replaced through various means (such as the Perlless and
    Bashless profiles in Nixpkgs) the available methods usually change how your
    system behaves, and may yield general instability or even breakage as is the
    case with the `/etc` overlay system.
</div>

<div align="center">
  <br/>
  <a href="#synopsis">Synopsis</a><br/>
  <a href="#compatibility-matrixoverview">Compatibility</a> | <a href="#usage">Usage</a> | <a href="#motivation">Motivation</a><br/>
  <a href="#contributing">Contributing</a>
  <br/>
</div>

## Synopsis

This monorepository provides a multi-call [^1] Rust binary to replace some of
these fragile scripts, and in some cases utilities _already_ written in Rust.
See the [motivation section](#motivation) for why we also try to replace Rust
code with more Rust code.

[here]: https://github.com/orangecms/multicall/blob/main/README.md

<!--markdownlint-disable MD059-->

[^1]: Multicall in Unix refers to a type of binary that allows multiple commands
    to be executed through a single executable, depending on the name used to
    invoke it. See [here] for a more comprehensive explanation. Historically
    multicall binaries have also been called "BusyBox-like", which should
    _probably_ be what you're thinking of for the purposes of this project.

<!--markdownlint-enable MD059-->

The NixOS module and the `nixos-core` crate replaces legacy Bash and Perl
scripts that execute on every NixOS system during boot and activation. The
multi-call binary design, like BusyBox, reduces binary size and simplifies
deployment. The project is intentionally modular to fit the particular needs of
any user through the [NixOS module](./nix/modules/nixos.nix) and
[feature flags](#feature-flagsoverrides).

## Compatibility Matrix/Overview

The scope of this program is currently limited to what Nixpkgs permits in terms
of replacing the legacy solutions _cleanly_ through the module system. Here is a
general overview of what can currently be replaced:

<!--markdownlint-disable MD013-->

| Command               | Original Script          | Purpose                                           | Status      |
| --------------------- | ------------------------ | ------------------------------------------------- | ----------- |
| `update-users-groups` | `update-users-groups.pl` | Manage `/etc/passwd`, `/etc/group`, `/etc/shadow` | First-class |
| `setup-etc`           | `setup-etc.pl`           | Atomically update `/etc/static`                   | First-class |
| `init-script-builder` | `init-script-builder.sh` | Create generic `/sbin/init`                       | First-class |
| `stage-1-init`        | `stage-1-init.sh`        | Initrd bootstrap                                  | First-class |
| `stage-2-init`        | `stage-2-init.sh`        | System activation                                 | First-class |

<!--markdownlint-enable MD013-->

While the project is rather new, base rewrites are complete and mostly verified
through VM tests and even in real systems. If you're interested in testing
`nixos-core` on your system, but are skeptical, you may be interested in
enabling it gradually through the NixOS module knobs and test each individual
component. As it stands, `nixos-core` does not change anything that can "brick"
your system, i.e., you can easily roll back a generation in case something goes
wrong.

## Usage

The `nixos-core` crate provides a multi-call binary invocable either as a
symlink:

```bash
# Invoke the update-users-groups command
$ update-users-groups /nix/store/...-users-groups.json
```

Or explicitly as a subcommand:

```bash
# Subcommand of update-users-groups
$ nixos-core update-users-groups /nix/store/...-users-groups.json
```

This usage pattern, like the mono-repo design, is a deliberate choice that
allows reusing code and shared patterns without the cognitive overhead of
cross-referencing separate projects. It also lets us provide a relatively small
binary that does everything.

### Feature Flags/Overrides

All commands provided by the `nixos-core` binary are enabled by default, but
also feature-gated through Rust's feature flags. This design lets you **disable
commands that you don't want or need** at build time and choose which binaries
to install, and what components to replace. Feature flags work both with `cargo`
(`--features`) and as package arguments when building with Nix:

```nix
[(prev: final: {
  nixos-core = prev.nixos-core.override {
    withStage1 = false; # e.g., skip initrd bootstrap
  }
})];
```

[`stage2` crate's README]: ./crates/stage2/README.md

Additional feature flags, also exposed as overridable package attributes,
control the behavior of **stage 2**. These are `bootspec`,
`systemd-integration`, and `full-nixos-init-compat`, providing complete
compatibility with the pre-existing behavior of Nixpkgs' stage 2. If you are
developing a systemd-less NixOS variant but still want to manage stage 2, you
can disable `systemd-integration` while retaining `bootspec` support. Most of
these features are described in detail in the [`stage2` crate's README].

## Motivation

[MicrOS]: https://github.com/snugnug/micros
[Finix]: https://github.com/finix-community/finix

`nixos-core` aims to be a safe, independent core utility for NixOS and NixOS
derivatives that build their own tooling from scratch, such as [MicrOS] and
[Finix]. The main goals of this project is being a fast, portable and consistent
utilities written in clean Rust, modular enough through feature flags and NixOS
module knobs to fit into any system and derivative project. It is an out-of-tree
module to meet those goals without the behavioral changes imposed by Nixpkgs
alternatives like Userborn or nixos-init. That is not to say those are
fundamentally incompatible with this project, but they are _different_. There
may be room for collaboration in the future.

## Contributing

We're always open to new contributions. Please keep in mind that as a critical
system utility, nixos-core has strict contribution requirements. We expect you
to write safe, principled Rust and test your code accordingly through unit and
integration tests. If you have a good idea, but can't _exactly_ be sure how to
proceed you may open an issue to discuss your changes with us beforehand.

### Hacking

`nixos-core` is built with the latest stable Rust available in Nixpkgs, which is
1.94.0 at the time of writing. The Minimum Supported Rust Version (MSRV) is
therefore set at 1.94.0, and may change as the language evolves.

#### Safety

- No `unsafe` code except for unavoidable syscalls (`crypt`, `geteuid`)
- Explicit error handling with the `?` operator
- Dry-run mode available for all destructive operations

#### Testing

This repository provides a few VM test to verify correct behaviour. Those are
rather limited at the time but should provide some amount of guidance for you.

```bash
# Run all VM tests that are exposed under `checks`
$ nix flake check

# Run a specific VM tests
$ nix build .#checks.x86_64-linux.boot
```

You can find the available NixOS VM tests in the [nix/vm-tests](./nix/vm-tests/)
directory. If you're adding new features, you should add a new test component or
some subtests that verify that your code works exactly as expected.

## License

[provided here]: https://www.mozilla.org/en-US/MPL/2.0/

This project is derived from Nixpkgs, originally licensed under the MIT License.
Original copyright and MIT license text are preserved in `licenses/MIT.txt`
located in the repository root. This project and all project code/documentation
are distributed under the **Mozilla Public License (MPL) version 2.0**. See
[LICENSE](LICENSE) for more details on the exact conditions. An online copy is
[provided here].
