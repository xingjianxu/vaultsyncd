{ pkgs, src, sourceInfo, nixpkgsRevision }:
let
  lib = pkgs.lib;
  version = (builtins.fromTOML (builtins.readFile (src + "/Cargo.toml"))).workspace.package.version;
  clean = name: roots: files: lib.cleanSourceWith {
    inherit name src;
    filter = path: type:
      let rel = lib.removePrefix (toString src + "/") (toString path);
          parts = lib.splitString "/" rel;
          belongs = prefix: rel == prefix || lib.hasPrefix (prefix + "/") rel
            || (type == "directory" && lib.hasPrefix (rel + "/") prefix);
      in rel == "" || (!(lib.any (part: builtins.elem part [ "target" "node_modules" "dist" ".cache" ".git" ]) parts)
        && (builtins.elem rel files || lib.any belongs roots));
  };
  rustSource = clean "vaultsyncd-rust-source"
    [ ".cargo" "api" "apps/agent" "apps/cli" "apps/server" "apps/server-admin"
      "apps/desktop/src-tauri" "crates" "fixtures" "tests" "tools" ]
    [ "Cargo.toml" "Cargo.lock" "rust-toolchain.toml" ];
  webSource = clean "vaultsyncd-web-source"
    [ "apps/web" "packages" ]
    [ "package.json" "pnpm-lock.yaml" "pnpm-workspace.yaml" "tsconfig.base.json" "apps/desktop/package.json" ];
  rust = pkgs.rustPlatform.buildRustPackage {
    pname = "vaultsyncd-rust";
    inherit version;
    src = rustSource;
    # Every fetched crate uses the actual checksum already pinned in Cargo.lock.
    cargoLock.lockFile = src + "/Cargo.lock";
    cargoBuildFlags = [ "-p" "vaultsyncd-server" "-p" "vaultsyncd-admin" ];
    cargoTestFlags = [ "-p" "vaultsyncd-server" "-p" "vaultsyncd-admin" "--lib" "--bins" ];
    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [ pkgs.sqlite ];
    doCheck = true;
    meta.platforms = lib.platforms.linux;
  };
  webDeps = pkgs.fetchPnpmDeps {
    pname = "vaultsyncd-web";
    src = webSource;
    fetcherVersion = 5;
    hash = "sha256-kYr78clfL0b5jXI0uuO9ePOPkk7f5e9+lsqHRnjCf68=";
  };
  web = pkgs.stdenvNoCC.mkDerivation {
    pname = "vaultsyncd-web";
    inherit version;
    src = webSource;
    pnpmDeps = webDeps;
    nativeBuildInputs = [ pkgs.nodejs pkgs.pnpmConfigHook pkgs.pnpm ];
    buildPhase = ''
      runHook preBuild
      test ! -d apps/web/dist
      pnpm --filter @vaultsyncd/web build
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      mkdir -p "$out"
      cp -r apps/web/dist/. "$out/"
      test -s "$out/index.html"
      test -d "$out/assets"
      runHook postInstall
    '';
  };
  inputs = lib.genAttrs
    [ "Cargo.lock" "pnpm-lock.yaml" "flake.lock" "rust-toolchain.toml"
      "api/openapi.yaml" "api/local-ipc.schema.json" "api/generation.toml" ]
    (file: builtins.hashFile "sha256" (src + "/" + file));
  # Nix scans for complete store hashes even without /nix/store/. Split them so
  # provenance does not retain .drv build closures (including Node) at runtime.
  reference = value:
    let base = builtins.baseNameOf (toString value);
    in { hash_groups = builtins.genList (i: builtins.substring (i * 8) 8 base) 4;
         name = builtins.substring 33 (builtins.stringLength base - 33) base; };
  provenance = pkgs.writeText "vaultsyncd-build-inputs.json" (builtins.toJSON {
    format_version = 1;
    kind = "nix-component-set";
    inherit version inputs sourceInfo nixpkgsRevision;
    target = pkgs.stdenv.hostPlatform.config;
    components = {
      rust = { source = reference rustSource; derivation = reference rust.drvPath; };
      web = { source = reference webSource; derivation = reference web.drvPath; dependencies = reference webDeps; };
    };
    toolchain = { rustc = pkgs.rustc.version; cargo = pkgs.cargo.version; node = pkgs.nodejs.version; pnpm = pkgs.pnpm.version; };
    guarantee = "Components are built from one locked flake snapshot; sandbox builds use offline Cargo/pnpm dependencies. This is not a forged git worktree receipt.";
  });
  release = assert sourceInfo.commit != null || throw "vaultsyncd release requires a clean committed flake; build components while developing";
    pkgs.runCommand "vaultsyncd-${version}" {
      nativeBuildInputs = [ pkgs.makeWrapper pkgs.python3 ];
      passthru = { inherit rust web; };
      meta.platforms = lib.platforms.linux;
    } ''
      mkdir -p "$out/bin" "$out/share/vaultsyncd"
      ln -s ${web} "$out/share/vaultsyncd/web"
      makeWrapper ${rust}/bin/vaultsyncd-admin "$out/bin/vaultsyncd-admin" \
        --prefix PATH : ${lib.makeBinPath [ pkgs.btrfs-progs ]}
      makeWrapper ${rust}/bin/vaultsyncd-server "$out/bin/vaultsyncd-server" \
        --set-default VAULTSYNCD_WEB_DIR ${web} \
        --prefix PATH : ${lib.makeBinPath [ pkgs.btrfs-progs ]}
      python3 ${src}/packaging/linux/nix-manifest.py ${provenance} "$out"
    '';
in {
  default = release;
  inherit rust web;
  web-deps = webDeps;
}
