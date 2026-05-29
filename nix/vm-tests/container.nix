{
  mkTest,
  nixosModule,
  testCommons,
}:
mkTest {
  name = "nixos-core-container";

  nodes.machine = {
    imports = [
      nixosModule
      testCommons
    ];
    system.nixos-core.enable = true;
    boot.loader.grub.enable = false;

    containers.demo = {
      autoStart = true;
      config = {
        imports = [ nixosModule ];
        system.nixos-core.enable = true;
        system.stateVersion = "26.05";
      };
    };
  };

  testScript = ''
    # A nixos-core container should successfully boot to multi-user under nspawn.
    machine.wait_for_unit("container@demo.service")
    machine.wait_until_succeeds("nixos-container run demo -- systemctl is-active multi-user.target")
  '';
}
