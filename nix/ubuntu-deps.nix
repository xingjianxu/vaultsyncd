{ pkgs, lockFile, arch }:
let
  lock = builtins.fromJSON (builtins.readFile lockFile);
  packages = lock.packages.${arch};
  files = map (p: { inherit (p) filename; path = pkgs.fetchurl { inherit (p) url sha256; name = p.filename; }; }) packages;
in pkgs.runCommand "vaultsyncd-ubuntu-${arch}-dependencies" { } ''
  mkdir -p "$out"
  ${pkgs.lib.concatMapStringsSep "\n" (p: "cp ${p.path} \"$out/${p.filename}\"") files}
''
