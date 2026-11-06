{ pkgs, source }:
let
  cross = pkgs.pkgsCross.aarch64-multiplatform;
  target = cross.stdenv.hostPlatform.config;
in cross.rustPlatform.buildRustPackage {
  pname = "vaultsyncd-arm64-platform-acceptance";
  version = (builtins.fromTOML (builtins.readFile (source + "/Cargo.toml"))).workspace.package.version;
  src = source;
  cargoLock.lockFile = source + "/Cargo.lock";
  cargoBuildFlags = [ "-p" "vaultsyncd-platform-linux-server" "--tests" "--example" "path-boundaries" ];
  nativeBuildInputs = [ cross.pkg-config ];
  buildInputs = [ cross.sqlite ];
  doCheck = false; # Executed in the actual Ubuntu ARM64 kernel, not this build CPU.
  installPhase = ''
    mkdir -p "$out"
    count=0
    for file in target/${target}/release/deps/vaultsyncd_platform_linux_server-*; do
      if [ -f "$file" ] && [ -x "$file" ]; then
        cp "$file" "$out/platform-tests"
        count=$((count + 1))
      fi
    done
    test "$count" = 1
    cp target/${target}/release/examples/path-boundaries "$out/path-boundaries"
  '';
}
