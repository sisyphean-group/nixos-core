{
  mkTest,
  nixosModule,
  testCommons,
}:
mkTest {
  name = "nixos-core-boot";

  nodes = {
    machine = {
      imports = [nixosModule testCommons];
      system.nixos-core.enable = true;
      boot = {
        loader.grub.enable = false;
        # nixos-core's stage1 only runs with the scripted initrd; systemd initrd
        # would bypass it entirely and also forbids postMountCommands.
        initrd.systemd.enable = false;

        # Canary launched during stage1's postMountCommands. After switch_root,
        # stage1 calls kill_remaining_processes; a plain shell process (cmdline
        # does not start with '@') must be killed. /run is moved to the new root
        # via MS_MOVE so the pid-file survives into the booted system.
        initrd.postMountCommands = ''
          sh -c 'while true; do sleep 1; done' &
          echo $! > /run/canary.pid
          while [ ! -s /run/canary.pid ]; do sleep 0.1; done
        '';

        # Marker written by stage2's postBootCommands hook.
        postBootCommands = "touch /etc/post-boot-ran";
      };

      networking.hostId = "cafebabe";
    };

    # Separate VM for lustrate tests so machine state is not shared.
    lustrate = {
      imports = [nixosModule testCommons];
      system.nixos-core.enable = true;
      boot = {
        loader.grub.enable = false;
        initrd.systemd.enable = false;
      };

      networking.hostId = "deadbeef";
    };

    # VM with strict activation mode enabled: stage2 must fail when activate is
    # absent. In a standard NixOS system the activate script is always present,
    # so this node should boot successfully, which tells us that strict mode
    # does not break a normal boot.
    strictActivation = {
      imports = [nixosModule testCommons];
      system.nixos-core = {
        enable = true;
        strictActivation = true;
      };
      boot = {
        loader.grub.enable = false;
        initrd.systemd.enable = false;
      };

      networking.hostId = "00c0ffee";
    };
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    with subtest("basic presence"):
      machine.succeed("test -f /etc/os-release")
      machine.succeed("test -e /etc/passwd")

    with subtest("stage1 kills regular initrd processes before switch_root"):
      machine.succeed("test -s /run/canary.pid")
      machine.fail("kill -0 $(cat /run/canary.pid) 2>/dev/null")

    with subtest("stage2 system symlinks point into the store"):
      machine.succeed("readlink /run/current-system | grep -q '^/nix/store/'")
      machine.succeed("readlink /run/booted-system  | grep -q '^/nix/store/'")
      machine.succeed("test /run/current-system -ef /run/booted-system")

    with subtest("nix store mount options"):
      for opt in ["ro", "nosuid", "nodev"]:
        machine.succeed(f'[[ "$(findmnt --direction backward --first-only --noheadings --output OPTIONS /nix/store)" =~ (^|,){opt}(,|$) ]]')
      machine.fail("touch /nix/store/should-not-work")

    with subtest("postBootCommands ran"):
      machine.succeed("test -f /etc/post-boot-ran")

    with subtest("HOST_ID written as 4 native-endian bytes"):
      machine.succeed("test -f /etc/hostid")
      machine.succeed("test $(wc -c < /etc/hostid) -eq 4")
      machine.succeed("od -An -tx1 /etc/hostid | tr -d ' \\n' | grep -qx 'bebafeca'")

    with subtest("stage1 wipes environment before exec /init"):
      # Upstream pivots with `exec env -i` so LD_LIBRARY_PATH=@extraUtils@/lib
      # doesn't leak into PID 1 and break systemd's libseccomp dlopen.
      machine.fail("tr '\\0' '\\n' < /proc/1/environ | grep -q '^LD_LIBRARY_PATH='")

      # Without seccomp, systemd drops the service PATH that resolves
      # relative ExecStart names, so tmpfiles-setup and friends 203/EXEC.
      machine.succeed("test -z \"$(systemctl --failed --no-legend)\"")

    # Two passes:
    #   1. Files outside nix/boot are moved to /old-root and
    #      the lustrate file is removed; system still boots after.
    #   2. Pre-existing /old-root.tmp must be correctly absorbed
    #      into the new /old-root via the atomic rename, not left
    #      orphaned.
    lustrate.start()
    lustrate.wait_for_unit("multi-user.target")

    with subtest("lustrate - non-protected entry moved to old-root"):
      lustrate.succeed("echo lustrate-content > /lustrate-marker")
      lustrate.succeed("touch /nixos-lustrate")
      lustrate.shutdown()
      lustrate.start()
      lustrate.wait_for_unit("multi-user.target")

      # /nixos-lustrate is deleted by handle_lustrate after completion.
      lustrate.fail("test -f /nixos-lustrate")

      # The marker must have been moved, not left at root.
      lustrate.fail("test -f /lustrate-marker")
      lustrate.succeed("test -f /old-root/lustrate-marker")

    with subtest("lustrate - pre-existing old-root.tmp is absorbed"):
      # Simulate a crash that left a partial old-root.tmp but no old-root.
      # rename(2) only needs write permission on /'s directory entry, so this
      # works even though /old-root/var/empty carries chattr +i.
      lustrate.succeed("mv /old-root /old-root-discarded")
      lustrate.succeed("mkdir -p /old-root.tmp && echo sentinel > /old-root.tmp/sentinel")
      lustrate.succeed("echo another-marker > /another-marker")
      lustrate.succeed("touch /nixos-lustrate")
      lustrate.shutdown()
      lustrate.start()
      lustrate.wait_for_unit("multi-user.target")

      # Rename must have promoted old-root.tmp to old-root.
      lustrate.succeed("test -d /old-root")
      lustrate.fail("test -e /old-root.tmp")

      # Entries from the aborted run are preserved.
      lustrate.succeed("test -f /old-root/sentinel")

      # Entries from the new run are also present.
      lustrate.succeed("test -f /old-root/another-marker")

    # With STAGE2_STRICT_ACTIVATION=true and an activate script present (normal
    # NixOS), stage2 must not abort. If it did, the VM would never reach
    # multi-user.target.
    strictActivation.start()
    strictActivation.wait_for_unit("multi-user.target")

    with subtest("strict activation - boots normally when activate script is present"):
      # Verify the option was threaded through to the stage-2 init script: the
      # env var export must be present in the script that was actually used for
      # this boot.
      strictActivation.succeed("grep -q 'STAGE2_STRICT_ACTIVATION=true' $(readlink /run/current-system)/init")
  '';
}
