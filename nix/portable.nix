{ pkgs, rust, web, provenance, version, sourceInfo, source }:
let
  lib = pkgs.lib;
  target = pkgs.stdenv.hostPlatform.config;
in assert sourceInfo.commit != null || throw "portable release requires a clean committed flake";
pkgs.runCommand "vaultsyncd-portable-${version}-${target}" {
  nativeBuildInputs = with pkgs.buildPackages; [ python3 patchelf gnutar gzip ];
  runtimeLibraryRoots = lib.makeLibraryPath [ pkgs.glibc (lib.getLib pkgs.stdenv.cc.cc) pkgs.stdenv.cc.cc.libgcc pkgs.sqlite pkgs.zlib ];
  runtimeVersions = builtins.toJSON { glibc = pkgs.glibc.version; gcc = pkgs.stdenv.cc.cc.version; sqlite = pkgs.sqlite.version; zlib = pkgs.zlib.version; patchelf = pkgs.buildPackages.patchelf.version; };
  glibcSource = pkgs.glibc.src;
  gccSource = pkgs.stdenv.cc.cc.src;
  launchTemplate = source + "/packaging/linux/portable-launch.sh";
  preflight = source + "/nix/modules/preflight.py";
  unit = source + "/packaging/linux/systemd/vaultsyncd.service";
  exampleConfig = source + "/packaging/linux/vaultsyncd.toml";
  containerTemplates = source + "/packaging/linux/container";
  passthru = { inherit rust web; };
  meta.platforms = [ "x86_64-linux" ];
} ''
  mkdir -p "$out" corresponding-source
  # Ship the exact upstream runtime-library sources and the complete locked
  # Nixpkgs build recipes/patches alongside the binary archive, not in the image.
  cp ${pkgs.glibc.src} corresponding-source/glibc-source.tar.xz
  cp ${pkgs.stdenv.cc.cc.src} corresponding-source/gcc-source.tar.xz
  tar --sort=name --mtime=@1 --owner=0 --group=0 --numeric-owner \
    -C ${pkgs.path} -cf - . | gzip -n > corresponding-source/nixpkgs-recipes.tar.gz
  cat > corresponding-source/README.txt <<'EOF'
  Corresponding glibc and GCC runtime sources, including their license files,
  are supplied in the adjacent original archives. The complete locked Nixpkgs
  recipes and patches are in nixpkgs-recipes.tar.gz. The release manifest pins
  the source commit, NAR, lock hashes and toolchain; rebuilding its portable
  output reproduces the library selection and patching commands. Keep this
  source archive available alongside every distributed binary archive/image.
  EOF
  tar --sort=name --mtime=@1 --owner=0 --group=0 --numeric-owner \
    -C corresponding-source -cf - . | gzip -n > "$out/vaultsyncd-runtime-sources.tar.gz"
  python3 ${source}/packaging/linux/portable.py ${provenance} ${rust} ${web} "$out" "$out/vaultsyncd-runtime-sources.tar.gz"
  tar --sort=name --mtime=@1 --owner=0 --group=0 --numeric-owner \
    -C "$out/runtime" -cf - . | gzip -n > "$out/vaultsyncd-${version}-${target}.tar.gz"
''
