# Isolated module fixture, not a host deployment. Values are explicit test inputs.
{ nixpkgs, package, parameters ? "", module ? ../../nix/modules/vaultsyncd.nix, existingUser ? false }:
let
  params = if parameters == "" then { } else builtins.fromJSON (builtins.readFile parameters);
  system = import (nixpkgs + "/nixos/lib/eval-config.nix") {
    system = "x86_64-linux";
    modules = [ module {
      system.stateVersion = "26.11";
      boot.loader.grub.enable = false;
      fileSystems."/" = { device = "/dev/test-only"; fsType = "btrfs"; };
      users.users = if existingUser then {
        operator = { isNormalUser = true; group = "users"; description = "Existing operator"; };
      } else { };
      services.vaultsyncd = { enable = true; package = if builtins.isAttrs package then package else builtins.storePath package; }
        // (if existingUser then { createUser = false; user = "operator"; group = "users"; } else { }) // params;
    } ];
  };
  failures = builtins.filter (a: !a.assertion) system.config.assertions;
in assert failures == [ ];
assert !existingUser || (system.config.users.users.operator.isNormalUser
  && !system.config.users.users.operator.isSystemUser
  && system.config.users.users.operator.description == "Existing operator"); {
  system = system.config.system.build.toplevel;
  unit = system.config.systemd.units."vaultsyncd.service".unit;
  policy = system.config.systemd.services.vaultsyncd.serviceConfig;
  # Forces whole-system evaluation including upstream NixOS assertions, without activation.
  toplevel = system.config.system.build.toplevel.drvPath;
}
