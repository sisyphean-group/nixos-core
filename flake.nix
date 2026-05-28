{
  inputs.nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    inherit (nixpkgs.lib) genAttrs systems;
    forEachSystem = genAttrs systems.doubles.linux;
    pkgsForEach = system: nixpkgs.legacyPackages.${system};

    # Build system -> matching pkgsCross musl target.
    # powerpc-linux is omitted because packages.powerpc-linux is unevaluatable.
    muslCrossAttr = {
      x86_64-linux    = "musl64";
      i686-linux      = "musl32";
      aarch64-linux   = "aarch64-multiplatform-musl";
      armv6l-linux    = "muslpi";
      powerpc64-linux = "ppc64-musl";
      riscv64-linux   = "riscv64-musl";
    };
  in {
    nixosModules = {
      nixos-core = import ./nix/modules/nixos.nix self;
      default = self.nixosModules.nixos-core;
    };

    overlays = {
      nixos-core = final: _prev: {
        nixos-core = self.packages.${final.stdenv.hostPlatform.system}.nixos-core;
      };
      default = self.overlays.nixos-core;
    };

    checks = forEachSystem (system: let
      pkgs = pkgsForEach system;
    in
      import ./nix/checks self {inherit pkgs;});

    packages = forEachSystem (system: let
      pkgs = pkgsForEach system;
      muslAttr = muslCrossAttr.${system} or null;
    in
      {
        nixos-core = pkgs.callPackage ./nix/package.nix {};
        default = self.packages.${system}.nixos-core;
      }
      // nixpkgs.lib.optionalAttrs (muslAttr != null) {
        nixos-core-musl = pkgs.pkgsCross.${muslAttr}.callPackage ./nix/package.nix {};
      });

    devShells = forEachSystem (system: {
      default = (pkgsForEach system).callPackage ./nix/shell.nix {};
    });
  };
}
