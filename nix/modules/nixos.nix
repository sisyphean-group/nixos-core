self: {
  config,
  pkgs,
  lib,
  ...
}: let
  inherit (lib.modules) mkIf mkForce;
  inherit (lib.options) mkOption mkEnableOption mkPackageOption literalExpression;
  inherit (lib.types) package lines str bool enum;
  inherit (lib.strings) optionalString escapeShellArg concatStringsSep;
  inherit (lib.meta) getExe';

  # Keep the default package on the caller's pkgs, including cross stdenvs.
  pkgsWithOverlay = pkgs.extend self.overlays.nixos-core;

  udev = config.systemd.package;
  extra-utils = config.system.build.extraUtils;
  useHostResolvConf = config.networking.resolvconf.enable && config.networking.useHostResolvConf;

  cfg = config.system.nixos-core;

  # Inlined from nixpkgs nixos/lib/utils.nix since those are not available in out-of-tree modules.
  pathsNeededForBoot = [
    "/"
    "/nix"
    "/nix/store"
    "/var"
    "/var/log"
    "/var/lib"
    "/var/lib/nixos"
    "/etc"
    "/usr"
  ];
  fsNeededForBoot = fs: fs.neededForBoot || builtins.elem fs.mountPoint pathsNeededForBoot;
  toShellPath = shell:
    if lib.types.shellPackage.check shell
    then "/run/current-system/sw${shell.shellPath}"
    else if lib.types.package.check shell
    then throw "${shell} is not a shell package"
    else shell;

  # linkUnits and udevRules are private to nixpkgs' stage-1.nix; reproduced here.
  linkUnits =
    pkgs.runCommand "link-units" {
      allowedReferences = [extra-utils];
      preferLocalBuild = true;
    } (''
        mkdir -p $out
        cp -v ${udev}/lib/systemd/network/*.link $out/
      ''
      + (
        let
          links = lib.filterAttrs (n: _: lib.hasSuffix ".link" n) config.systemd.network.units;
          files = lib.mapAttrsToList (n: v: "${v.unit}/${n}") links;
        in
          lib.concatMapStringsSep "\n" (f: "cp -v ${f} $out/") files
      ));

  udevRules =
    pkgs.runCommand "udev-rules" {
      allowedReferences = [extra-utils];
      preferLocalBuild = true;
    } ''
      mkdir -p $out
      cp -v ${udev}/lib/udev/rules.d/60-cdrom_id.rules $out/
      cp -v ${udev}/lib/udev/rules.d/60-persistent-storage.rules $out/
      cp -v ${udev}/lib/udev/rules.d/75-net-description.rules $out/
      cp -v ${udev}/lib/udev/rules.d/80-drivers.rules $out/
      cp -v ${udev}/lib/udev/rules.d/80-net-setup-link.rules $out/
      cp -v ${pkgs.lvm2}/lib/udev/rules.d/*.rules $out/
      ${config.boot.initrd.extraUdevRulesCommands}

      for i in "$out"/*.rules; do
        # XXX: --replace in 'substituteInPlace' has been deprecated and should be
        # replaced with --replace-fail and the such. Since those paths are not
        # guaranteed to exist, --replace-warn trades the verbosity of warnings
        # from --replace with explicit warning messages stating what is missing.
        # This is the cleanest solution I can think of.
        substituteInPlace "$i" \
          --replace-warn ata_id ${extra-utils}/bin/ata_id \
          --replace-warn scsi_id ${extra-utils}/bin/scsi_id \
          --replace-warn cdrom_id ${extra-utils}/bin/cdrom_id \
          --replace-warn ${getExe' pkgs.coreutils "basename"} ${extra-utils}/bin/basename \
          --replace-warn ${getExe' pkgs.util-linux "blkid"} ${extra-utils}/bin/blkid \
          --replace-warn ${getExe' pkgs.mdadm "mdadm"} ${extra-utils}/sbin \
          --replace-warn ${getExe' pkgs.bash "sh"} ${extra-utils}/bin/sh \
          --replace-warn ${lib.getBin pkgs.lvm2}/bin ${extra-utils}/bin \
          --replace-warn ${udev} ${extra-utils}
      done

      substituteInPlace "$out"/60-persistent-storage.rules \
        --replace-warn ID_CDROM_MEDIA_TRACK_COUNT_DATA ID_CDROM_MEDIA
    '';

  # Use the topologically-sorted list from nixpkgs, not raw attrValues.
  fileSystems = lib.filter fsNeededForBoot config.system.build.fileSystems;
  fsInfo = pkgs.writeText "initrd-fsinfo" (lib.concatStringsSep "\n" (lib.concatMap (fs: [
      fs.mountPoint
      (
        if fs.device != null
        then fs.device
        else if fs.label != null && fs.label != ""
        then "/dev/disk/by-label/${fs.label}"
        else fs.fsType # virtual filesystems (tmpfs, proc, etc.) use fsType as device
      )
      fs.fsType
      (builtins.concatStringsSep "," fs.options)
    ])
    fileSystems));

  resumeDevices =
    lib.filter (
      sd:
        lib.hasPrefix "/dev/" sd.device
        && !sd.randomEncryption.enable
        && !(lib.hasPrefix "/dev/zram" sd.device)
    )
    config.swapDevices;

  resumeDevicesList = lib.concatStringsSep " " (map (sd: sd.device or "/dev/disk/by-label/${sd.label}") resumeDevices);

  # Hook scripts: stage1 expects file paths, not inline text.
  preFailCommandsFile = pkgs.writeText "pre-fail-commands" config.boot.initrd.preFailCommands;
  preDeviceCommandsFile = pkgs.writeText "pre-device-commands" config.boot.initrd.preDeviceCommands;
  preLVMCommandsFile = pkgs.writeText "pre-lvm-commands" config.boot.initrd.preLVMCommands;
  postDeviceCommandsFile = pkgs.writeText "post-device-commands" config.boot.initrd.postDeviceCommands;
  postResumeCommandsFile = pkgs.writeText "post-resume-commands" config.boot.initrd.postResumeCommands;
  postMountCommandsFile = pkgs.writeText "post-mount-commands" config.boot.initrd.postMountCommands;
  postBootCommandsFile = pkgs.writeText "post-boot-commands" ''
    ${config.boot.postBootCommands}
    ${config.powerManagement.powerUpCommands}
  '';

  bootStage1 = pkgs.writeTextFile {
    name = "stage-1-init";
    executable = true;
    text = ''
      #!${extra-utils}/bin/ash
      export extraUtils=${extra-utils}
      export kernelModules=${escapeShellArg (concatStringsSep " " config.boot.initrd.kernelModules)}
      export resumeDevice=${escapeShellArg config.boot.resumeDevice}
      export resumeDevices=${escapeShellArg resumeDevicesList}
      export fsInfo=${fsInfo}
      export earlyMountScript=${config.system.build.earlyMountScript}
      export udevRules=${udevRules}
      export linkUnits=${linkUnits}
      export checkJournalingFS=${
        if config.boot.initrd.checkJournalingFS
        then "1"
        else "0"
      }
      export distroName=${escapeShellArg config.system.nixos.distroName}
      export preFailCommands=${preFailCommandsFile}
      export preDeviceCommands=${preDeviceCommandsFile}
      export preLVMCommands=${preLVMCommandsFile}
      export postDeviceCommands=${postDeviceCommandsFile}
      export postResumeCommands=${postResumeCommandsFile}
      export postMountCommands=${postMountCommandsFile}
      ${optionalString (config.networking.hostId != null) ''
        export HOST_ID=${escapeShellArg config.networking.hostId}
      ''}

      export DEVICE_MANAGER=${escapeShellArg cfg.components.bootStage1.deviceManager}
      ${optionalString (cfg.components.bootStage1.deviceManager == "udev") ''
        export UDEV_BINARY=${lib.escapeShellArg "${extra-utils}/bin/systemd-udevd"}
        export UDEVADM_BINARY=${lib.escapeShellArg "${extra-utils}/bin/udevadm"}
      ''}
      ${optionalString (cfg.components.bootStage1.deviceManager == "mdev") ''
        export MDEV_BINARY=${lib.escapeShellArg "${extra-utils}/bin/mdev"}
      ''}
      ${optionalString (cfg.components.bootStage1.deviceManager == "mdevd") ''
        export MDEVD_BINARY=${lib.escapeShellArg "${extra-utils}/bin/mdevd"}
        export MDEVD_COLDPLUG_BINARY=${lib.escapeShellArg "${extra-utils}/bin/mdevd-coldplug"}
      ''}
      export LINK_UNITS_DEST=/etc/systemd/network

      exec ${extra-utils}/bin/nixos-core stage-1-init
    '';
  };

  nixosInitCompat = cfg.components.nixosInitCompat.enable;
  nixosInitCompatFlags = optionalString nixosInitCompat " --setup-fhs --create-current-system";

  # top-level.nix does `substituteInPlace $out/init --subst-var-by systemConfig $out`
  # after copying bootStage2, so @systemConfig@ must be a literal string here.
  bootStage2 = pkgs.writeTextFile {
    name = "stage-2-init";
    executable = true;
    text = ''
      #!${pkgs.bash}/bin/bash
      export SYSTEM_CONFIG=@systemConfig@
      export NIX_STORE_MOUNT_OPTS=${escapeShellArg (concatStringsSep "," config.boot.nixStoreMountOpts)}
      export SYSTEMD_EXECUTABLE=${escapeShellArg config.boot.systemdExecutable}
      export STAGE2_PATH=${escapeShellArg (lib.makeBinPath ([pkgs.coreutils pkgs.util-linux] ++ lib.optional useHostResolvConf pkgs.openresolv))}
      export POST_BOOT_COMMANDS=${postBootCommandsFile}
      export POST_BOOT_SHELL=${getExe' pkgs.bash "bash"}
      export EARLY_MOUNT_SCRIPT=${config.system.build.earlyMountScript}
      export USE_HOST_RESOLV_CONF=${
        if useHostResolvConf
        then "true"
        else "false"
      }
      export STAGE2_GREETING=${escapeShellArg "<<< ${config.system.nixos.distroName} Stage 2 >>>"}
      ${optionalString cfg.strictActivation "export STAGE2_STRICT_ACTIVATION=true"}
      ${optionalString nixosInitCompat ''
        export ENV_BINARY=${escapeShellArg config.environment.usrbinenv}
        export SH_BINARY=${escapeShellArg config.environment.binsh}
      ''}
      exec ${cfg.package}/bin/stage-2-init${nixosInitCompatFlags}
    '';
  };

  usersSpec = pkgs.writeText "users-groups.json" (builtins.toJSON {
    inherit (config.users) mutableUsers;
    users = lib.mapAttrsToList (_: u: {
      inherit
        (u)
        name
        uid
        group
        description
        home
        homeMode
        createHome
        isSystemUser
        password
        hashedPasswordFile
        hashedPassword
        autoSubUidGidRange
        subUidRanges
        subGidRanges
        initialPassword
        initialHashedPassword
        expires
        ;
      shell = toShellPath u.shell;
    }) (lib.filterAttrs (_: u: u.enable) config.users.users);
    groups = lib.attrValues config.users.groups;
  });

  initialRamdisk = pkgs.makeInitrd {
    name = "initrd-${config.boot.kernelPackages.kernel.name or "kernel"}";
    inherit (config.boot.initrd) compressor compressorArgs prepend;
    contents =
      [
        {
          object = bootStage1;
          symlink = "/init";
        }
        {
          object = "${config.system.build.modulesClosure}/lib";
          symlink = "/lib";
        }
        {
          object = "${pkgs.kmod-blacklist-ubuntu}/modprobe.conf";
          symlink = "/etc/modprobe.d/ubuntu.conf";
        }
        {
          object = config.environment.etc."modprobe.d/nixos.conf".source;
          symlink = "/etc/modprobe.d/nixos.conf";
        }
        {
          object = pkgs.kmod-debian-aliases;
          symlink = "/etc/modprobe.d/debian.conf";
        }
      ]
      ++ lib.optionals config.services.multipath.enable [
        {
          object =
            pkgs.runCommand "multipath.conf" {
              src = config.environment.etc."multipath.conf".text;
              preferLocalBuild = true;
            } ''
              target=$out
              printf "$src" > $out
              substituteInPlace $out \
                --replace ${config.services.multipath.package}/lib ${extra-utils}/lib
            '';
          symlink = "/etc/multipath.conf";
        }
      ]
      ++ lib.mapAttrsToList (symlink: options: {
        inherit symlink;
        object = options.source;
      })
      config.boot.initrd.extraFiles;
  };
in {
  options.system.nixos-core = {
    enable = mkEnableOption "nixos-core multi-call binary";
    package = mkPackageOption pkgsWithOverlay "nixos-core" {
      pkgsText = literalExpression "pkgs.extend nixos-core.overlays.nixos-core";
    };

    strictActivation = mkOption {
      type = bool;
      default = false;
      description = ''
        Whether to fail stage 2 when {file}`$systemConfig/activate` is missing,
        instead of silently skipping activation. This corresponds to the
        `--strict-activation` CLI flag / {env}`STAGE2_STRICT_ACTIVATION` environment
        variable of `stage-2-init`.
      '';
    };

    stateDir = mkOption {
      type = str;
      default = "/var/lib/nixos";
      example = "/run/nixos";
      description = ''
        Directory used by nixos-core to store runtime state (etc manifest,
        uid/gid maps, etc.). Exported as {env}`NIXOS_CORE_STATE_DIR` to activation
        scripts. Override on NixOS variants that use a different state path.
      '';
    };

    # Resurface some of the hard-coded checks so that the user can selectively override
    # behaviour in non-standard environments. This is deliberately *not* named `settings`
    # to leave room for a future settings option in case we decide to set that up.
    components = {
      extraUtilsCommand.enable =
        mkEnableOption ""
        // {
          default = !config.boot.initrd.systemd.enable;
          defaultText = literalExpression "!config.boot.initrd.systemd.enable";
          description = "Whether to place nixos-core in extraUtils for the stage-1 wrapper";
        };

      bootStage1 = {
        enable =
          mkEnableOption ""
          // {
            default = !config.boot.initrd.systemd.enable;
            defaultText = literalExpression "!config.boot.initrd.systemd.enable";
            description = ''
              Whether to create {option}`system.build.bootStage1` wrapper with `nixos-core` available.

              ::: {.note}

              This option conflicts with NixOS' systemd-in-stage1 option, so it is
              **generally not recommended** to try and override this option. It may
              come in handy for Systemd-less NixOS variants still relying on Bash in stage 1.

              :::
            '';
          };

        package = mkOption {
          type = package;
          default = bootStage1;
          description = "The stage-1 init script package used as {file}`/init` inside the initrd.";
          readOnly = true; # we can't handle a modified package
        };

        deviceManager = mkOption {
          type = enum [
            "udev"
            "mdev"
            "mdevd"
          ];
          default = "udev";
          description = ''
            Device manager backend used by nixos-core's stage-1 init.

            `udev` matches the default NixOS scripted initrd behavior. `mdev`
            selects BusyBox mdev, and `mdevd` selects skarnet mdevd with
            synchronous coldplug support.
          '';
        };
      };

      bootStage2 = {
        enable =
          mkEnableOption ""
          // {
            default = true;
            description = ''
              Whether to replace {option}`system.build.bootStage2` with nixos-core's
              `stage-2-init`.

              Under a systemd initrd, nixpkgs' `initrd-nixos-activation.service`
              calls the resulting `prepare-root` via `chroot /sysroot`.

              `stage-2-init` detects {env}`IN_NIXOS_SYSTEMD_STAGE1=true` and exits
              after activation instead of `exec`-ing systemd.
            '';
          };

        package = mkOption {
          type = package;
          default = bootStage2;
          description = "The stage-2 init script package used as {option}`system.build.bootStage2`.";
          readOnly = true;
        };
      };

      initialRamdisk = {
        enable =
          mkEnableOption ""
          // {
            default = !config.boot.initrd.systemd.enable;
            defaultText = literalExpression "!config.boot.initrd.systemd.enable";
            description = ''
              Whether to override {option}`system.build.initialRamdisk` with
              nixos-core's initrd, which embeds the stage-1 wrapper as {file}`/init`.
            '';
          };

        package = mkOption {
          type = package;
          default = initialRamdisk;
          description = "The initrd package used as {option}`system.build.initialRamdisk`.";
          readOnly = true; # we can't handle a modified package
        };
      };

      bootloaderInstaller = {
        enable =
          mkEnableOption ""
          // {
            default = config.boot.loader.initScript.enable;
            defaultText = literalExpression "config.boot.loader.initScript.enable";
            description = "Whether to replace legacy bootloader installer with nixos-core";
          };

        package = mkOption {
          type = package;
          default = getExe' cfg.package "init-script-builder";
          defaultText = literalExpression "$${getExe' cfg.package \"init-script-builder\"}";
          description = "The bootloader installer package to use";
        };
      };

      etcActivation = {
        enable =
          mkEnableOption ""
          // {
            default = !config.system.etc.overlay.enable;
            defaultText = literalExpression "!config.system.etc.overlay.enable";
            description = "Whether to replace the {file}`/etc` activation script with nixos-core";
          };

        script = mkOption {
          type = lines;
          default = ''
            echo "setting up /etc..."
            export NIXOS_CORE_STATE_DIR=${escapeShellArg cfg.stateDir}
            ${getExe' cfg.package "setup-etc"} ${config.system.build.etc}/etc
          '';
          description = "Script contents passed to {option}`system.build.etcActivationCommands`";
        };
      };

      userGroupsActivation = {
        enable =
          mkEnableOption ""
          // {
            default = !config.systemd.sysusers.enable;
            defaultText = literalExpression "!config.systemd.sysusers.enable";
            description = "Whether to create users and groups with nixos-core";
          };

        script = mkOption {
          type = lines;
          default = ''
            install -m 0700 -d /root
            install -m 0755 -d /home
            export NIXOS_CORE_STATE_DIR=${escapeShellArg cfg.stateDir}
            ${getExe' cfg.package "update-users-groups"} ${usersSpec}
          '';
          description = "Script contents passed to the user activation script";
        };
      };

      nixosInitCompat = {
        enable =
          mkEnableOption ""
          // {
            default = config.boot.initrd.systemd.enable;
            defaultText = literalExpression "config.boot.initrd.systemd.enable";
            description = ''
              Whether to wire nixos-init-compatible setup into stage2 for the
              systemd-initrd path. When enabled, stage2 receives flags to set up
              `/run/current-system`, the firmware search path, the modprobe
              binary pointer, and FHS compatibility symlinks.
            '';
          };
      };
    };
  };

  config = mkIf cfg.enable {
    # Legacy initrd only: add nixos-core to extraUtils so stage-1 wrapper can exec it,
    # and override bootStage1/initialRamdisk with our wrapper.
    # With the systemd initrd stage-1 is handled by systemd; these don't apply.
    boot.initrd.extraUtilsCommands = mkIf cfg.components.extraUtilsCommand.enable ''
      copy_bin_and_libs ${getExe' cfg.package "nixos-core"}
      ${optionalString (cfg.components.bootStage1.deviceManager == "mdevd") ''
        copy_bin_and_libs ${getExe' pkgs.mdevd "mdevd"}
        copy_bin_and_libs ${getExe' pkgs.mdevd "mdevd-coldplug"}
      ''}
    '';

    system = {
      build = {
        # Stage 1
        bootStage1 = mkIf cfg.components.bootStage1.enable (mkForce cfg.components.bootStage1.package);

        # Rebuild the initrd with our bootStage1 as /init. This, at the cost of risking getting
        # out of sync, mirrors the contents list from nixpkgs' stage-1.nix.
        initialRamdisk = mkIf cfg.components.initialRamdisk.enable (mkForce cfg.components.initialRamdisk.package);

        # Stage 2
        bootStage2 = mkIf cfg.components.bootStage2.enable (mkForce cfg.components.bootStage2.package);

        # Bootloader Installer
        installBootLoader = mkIf cfg.components.bootloaderInstaller.enable (mkForce cfg.components.bootloaderInstaller.package);

        # /etc Activation
        etcActivationCommands = mkIf cfg.components.etcActivation.enable (mkForce cfg.components.etcActivation.script);
      };

      # Only force the `text`, not the whole record. Other modules (notably
      # agenix, which injects `users.deps = [ "agenixInstall" ]`) merge their
      # own attributes into this script; replacing the record wholesale with
      # mkForce nukes those contributions and lands agenixChown before
      # agenixNewGeneration, so $_agenix_generation is empty when chown runs.
      activationScripts.users = mkIf cfg.components.userGroupsActivation.enable {
        supportsDryActivation = lib.mkDefault true;
        text = mkForce cfg.components.userGroupsActivation.script;
      };
    };

    assertions = [
      {
        assertion = !config.system.nixos-init.enable;
        message = "nixos-core supersedes nixos-init, and cannot be used alongside it";
      }
      {
        assertion = !config.system.etc.overlay.enable;
        message = "nixos-core cannot be used with system.etc.overlay.enable";
      }
      {
        assertion = !config.services.userborn.enable && !config.systemd.sysusers.enable;
        message = "nixos-core cannot be used with services.userborn.enable or systemd.sysusers.enable";
      }
    ];
  };
}
