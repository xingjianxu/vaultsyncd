{ config, lib, pkgs, ... }:
let
  inherit (lib) mkEnableOption mkIf mkOption types;
  cfg = config.services.vaultsyncd;
  absolute = types.strMatching "/[^\n]*";
  canonical = p: p == "/" || (builtins.match ".*(//|/\\.(/|$)|/\\.\\.(/|$)|/$).*" p == null);
  within = base: path: path == base || lib.hasPrefix "${base}/" path;
  protected = [ "/nix" "/etc" "/usr" "/bin" "/sbin" "/run" "/proc" "/sys" "/dev" ];
  safeRoot = p: p != "/" && canonical p && !(builtins.any (base: within base p || within p base) protected)
    && !(within cfg.stateDir p || within p cfg.stateDir);
  managedState = lib.hasPrefix "/var/lib/" cfg.stateDir;
  toml = (pkgs.formats.toml { }).generate "vaultsyncd.toml" ({
    server = cfg.settings.server;
    log = cfg.settings.log;
    state.dir = cfg.stateDir;
  } // lib.optionalAttrs (cfg.vaultPaths != [ ]) {
    vaults.allowed_bases = cfg.vaultPaths;
  });
  initScript = pkgs.writeShellScriptBin "vaultsyncd-init" ''
    exec ${cfg.package}/bin/vaultsyncd-admin init --state-dir ${cfg.stateDir} --login ${cfg.adminUser} "$@"
  '';
  preflightConfig = pkgs.writeText "vaultsyncd-preflight.json" (builtins.toJSON {
    inherit (cfg) stateDir;
    vaultPaths = cfg.vaultPaths;
  });
  preflight = pkgs.writeShellScript "vaultsyncd-preflight" ''
    exec ${pkgs.python3}/bin/python3 ${./preflight.py} ${pkgs.util-linux}/bin/findmnt ${preflightConfig}
  '';
in {
  options.services.vaultsyncd = {
    enable = mkEnableOption "vaultsyncd Linux server (explicit btrfs storage required)";
    package = mkOption { type = types.package; description = "Server/admin/Web release package; the flake module supplies its own package."; };
    createUser = mkOption { type = types.bool; default = true; description = "Create a dedicated system account. Disable to use an existing non-root account without changing its account settings."; };
    user = mkOption { type = types.strMatching "[a-z_][a-z0-9_-]*"; default = "vaultsyncd"; description = "Non-root service account; must already exist when createUser is false."; };
    group = mkOption { type = types.strMatching "[a-z_][a-z0-9_-]*"; default = "vaultsyncd"; description = "Non-root service group; must already exist when createUser is false."; };
    extraGroups = mkOption { type = types.listOf types.str; default = [ ]; description = "Explicit operator-managed Root access groups; no ownership changes are made to Roots."; };
    stateDir = mkOption { type = absolute; default = "/var/lib/vaultsyncd"; description = "Private btrfs internal state. Under /var/lib systemd manages StateDirectory (0700); elsewhere the operator must provision it owned by the service UID. Never point at user data."; };
    vaultPaths = mkOption { type = types.listOf absolute; default = [ ]; description = "Explicit writable Vault base allowlist, disjoint from state and system paths. Registration still checks identity, nested mounts/subvolumes and CoW. Permissions must be granted separately by ACL or group."; };
    adminUser = mkOption {
      type = types.addCheck types.str (value: !(lib.hasInfix "\n" value) && !(lib.hasInfix " " value));
      default = cfg.user;
      description = "Login name for the first owner account created by `vaultsyncd-init`; defaults to the service user. Must be set before the offline init; it is not a password.";
    };
    settings = {
      server.listen = mkOption { type = types.str; default = "127.0.0.1:8721"; description = "HTTP address; loopback by default. TLS proxy and firewall are managed separately."; };
      log.level = mkOption { type = types.enum [ "trace" "debug" "info" "warn" "error" ]; default = "info"; description = "Log level."; };
      log.format = mkOption { type = types.enum [ "text" "json" ]; default = "json"; description = "Log format."; };
    };
  };
  config = mkIf cfg.enable {
    environment.systemPackages = [ initScript ];
    assertions = [
      { assertion = cfg.user != "root" && cfg.group != "root" && (config.users.users.${cfg.user}.uid or null) != 0 && (config.users.groups.${cfg.group}.gid or null) != 0; message = "vaultsyncd requires a non-root user/group"; }
      { assertion = cfg.createUser || (builtins.hasAttr cfg.user config.users.users && builtins.hasAttr cfg.group config.users.groups); message = "vaultsyncd createUser=false requires an existing user and group"; }
      { assertion = cfg.stateDir != "/" && canonical cfg.stateDir && !(builtins.any (base: within base cfg.stateDir || within cfg.stateDir base) protected); message = "vaultsyncd stateDir must be canonical, private and outside system paths"; }
      { assertion = builtins.all safeRoot cfg.vaultPaths; message = "vaultsyncd vaultPaths must be canonical, disjoint from state and outside protected system paths"; }
      { assertion = builtins.length cfg.vaultPaths == builtins.length (lib.unique cfg.vaultPaths); message = "vaultsyncd vaultPaths must be unique"; }
      { assertion = cfg.adminUser != ""; message = "vaultsyncd adminUser must not be empty"; }
    ];
    users.groups = lib.optionalAttrs cfg.createUser { ${cfg.group} = { }; };
    users.users = lib.optionalAttrs cfg.createUser {
      ${cfg.user} = {
        isSystemUser = true;
        group = cfg.group;
        extraGroups = cfg.extraGroups;
        description = "vaultsyncd service";
      };
    };
    systemd.services.vaultsyncd = {
      description = "vaultsyncd server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      unitConfig.RequiresMountsFor = [ cfg.stateDir "${cfg.stateDir}/backups" ] ++ cfg.vaultPaths;
      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        SupplementaryGroups = cfg.extraGroups;
        ExecStartPre = preflight;
        ExecStart = "${cfg.package}/bin/vaultsyncd-server --config ${toml}";
        Restart = "on-failure";
        RestartSec = "5s";
        UMask = "0077";
        NoNewPrivileges = true;
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
        ProtectSystem = "strict";
        ProtectHome = "read-only";
        ReadWritePaths = [ cfg.stateDir ] ++ cfg.vaultPaths;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" ];
        LockPersonality = true;
        WorkingDirectory = cfg.stateDir;
      } // lib.optionalAttrs managedState {
        StateDirectory = lib.removePrefix "/var/lib/" cfg.stateDir;
        StateDirectoryMode = "0700";
      };
    };
  };
}
