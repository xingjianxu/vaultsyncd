{
  description = "vaultsyncd — 锁定工具链的 Linux 开发 shell";

  inputs = {
    # 选择 nixos-unstable 而不是 nixos-26.05：开发主机本身运行 26.11.20261102（NixOS 26.11
    # 尚未发布稳定通道），稳定通道 26.05 比主机旧一个大版本，Rust/Node 明显更旧；unstable
    # 与主机同源 master，二进制缓存命中率最高。可复现性由 flake.lock 的固定 rev 保证。
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      # 首发仅 Linux 服务端/控制面在 Nix 内构建；按 design ADR-013（2026-11-05），Windows
      # 客户端在本 NixOS 宿主交叉编译、Windows 客体只做原生运行验收。交叉目标不是 flake 的
      # system 列表：S009 用独立的 `windows` devShell 提供 x86_64-pc-windows-msvc 交叉能力
      # （目标、Rust 标准库与 SDK 分工见 docs/environment.md §13）。
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      nixosModules.default = { pkgs, lib, ... }: {
        imports = [ ./nix/modules/vaultsyncd.nix ];
        services.vaultsyncd.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      };
      nixosModules.vaultsyncd = self.nixosModules.default;
      checks = forAllSystems (pkgs: {
        nixos-module = (import ./deploy/nixos/eval.nix {
          nixpkgs = nixpkgs.outPath;
          module = self.nixosModules.default;
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        }).unit;
      });
      packages = forAllSystems (pkgs: import ./nix/package.nix {
        inherit pkgs;
        src = self.outPath;
        sourceInfo = { commit = self.rev or null; narHash = self.narHash; };
        nixpkgsRevision = nixpkgs.rev;
      });
      devShells = forAllSystems (pkgs: {
        portable-tests = pkgs.mkShell {
          name = "vaultsyncd-portable-tests";
          packages = [ pkgs.podman pkgs.docker-compose pkgs.chromium pkgs.btrfs-progs
            pkgs.qemu pkgs.xorriso pkgs.openssh
            pkgs.patchelf pkgs.binutils pkgs.util-linux
            (pkgs.pkgsStatic.qemu-user.override { hostCpuTargets = [ "aarch64-linux-user" ]; })
            (pkgs.python3.withPackages (ps: [ ps.playwright ])) ];
          PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright-driver.browsers}";
          PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
        };
        web-tests = pkgs.mkShell {
          name = "vaultsyncd-web-tests";
          packages = [ pkgs.nodejs pkgs.pnpm pkgs.chromium pkgs.openssh pkgs.openssl (pkgs.python3.withPackages (ps: [ ps.playwright ])) ];
          PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright-driver.browsers}";
          PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
        };
        default = pkgs.mkShell {
          name = "vaultsyncd-dev";
          packages = [
            # Rust 工具链（来自锁定 nixpkgs，版本由 flake.lock 决定）
            pkgs.rustc
            pkgs.cargo
            pkgs.rustfmt
            pkgs.clippy

            # C 工具链与链接期探测（构建依赖 C 的 crate、SQLite/OpenSSL 等）
            pkgs.gcc
            pkgs.binutils
            pkgs.pkg-config

            # 前端与脚本
            pkgs.nodejs
            pkgs.pnpm
            pkgs.python3

            # S013：存储准入验收需要 mkfs.ext4（拒绝样本）、chattr/lsattr（NOCOW 标志样本）
            # 与 filefrag（reflink 共享 extent 证据）。只用于 tests/platform/linux/ 下的脚本，
            # 不引入任何产品运行依赖。
            pkgs.e2fsprogs

            # 版本控制与数据库
            pkgs.git
            pkgs.sqlite
            pkgs.sqlite.dev # sqlite3.h 与 libsqlite3.so，供 pkg-config/链接使用

            # 基本诊断
            pkgs.curl
            pkgs.jq
            pkgs.strace
            pkgs.lsof
            pkgs.tree
          ];

          shellHook = ''
            # 未加额外 overlay：锁定 nixpkgs 已提供上述全部能力。
            export VAULTSYNCD_DEV_SHELL=1
          '';
        };

        # S008：Windows 客体实验室专用 shell（`nix develop .#quickemu`）。与 default 分开，
        # 避免把 QEMU/Virtualisation 依赖塞进日常开发环境；版本同样由 flake.lock 固定。
        # xorriso/wimlib/p7zip 用于核对 install.wim 版本与重打 unattended.iso。
        quickemu = pkgs.mkShell {
          name = "vaultsyncd-quickemu";
          packages = [
            pkgs.quickemu
            pkgs.qemu
            pkgs.xorriso
            pkgs.wimlib
            pkgs.p7zip
            pkgs.openssh
            pkgs.curl
          ];

          shellHook = ''
            export VAULTSYNCD_QUICKEMU_SHELL=1
          '';
        };

        # S009：Windows x64 交叉编译 shell（`nix develop .#windows`）。与 default 分开，
        # 避免日常 Linux 构建引入 rustup/xwin 的下载缓存。
        #
        # 分工（全部可由 `tests/platform/windows/setup-toolchain.sh` 复现）：
        # - Nix（flake.lock 固定）：rustup 1.29.2、cargo-xwin 0.23.1、LLVM 21.1.8
        #   （clang-cl、lld-link、llvm-lib）、openssh、git。
        # - rustup 按 rust-toolchain.toml 的 channel 下载官方 Rust 1.99.0 宿主 rustc 与
        #   x86_64-pc-windows-msvc 标准库；cargo-xwin 下载 MSVC 14.44 CRT 与 Windows SDK
        #   10.0.26100 的头文件/导入库（含 cldapi.lib）。
        # - 缓存写到工作树根的 .cache/windows/（已被 .gitignore 忽略），不碰用户全局
        #   ~/.rustup、~/.cargo；清理用 `rm -rf .cache/windows`。
        windows = pkgs.mkShell {
          name = "vaultsyncd-windows";
          packages = [
            pkgs.rustup
            pkgs.cargo-xwin
            pkgs.llvmPackages.clang
            # clang-cl（MSVC 兼容驱动）只由 unwrapped 包提供，cargo-xwin 依赖它。
            pkgs.llvmPackages.clang-unwrapped
            pkgs.llvmPackages.lld
            pkgs.llvmPackages.llvm
            pkgs.openssh
            # Native GUI runner writes bounded SHA-256 transport manifests on the host.
            pkgs.openssl # S081 private TLS acceptance certificates
            pkgs.nsis # S099 host-built Unicode per-user Windows installer, pinned by flake.lock
            pkgs.nodejs
            pkgs.pnpm
            pkgs.python3
            pkgs.git
          ];

          shellHook = ''
            export VAULTSYNCD_WINDOWS_SHELL=1
            repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
            export VAULTSYNCD_WINDOWS_CACHE="''${VAULTSYNCD_WINDOWS_CACHE:-$repo_root/.cache/windows}"
            export RUSTUP_HOME="$VAULTSYNCD_WINDOWS_CACHE/rustup"
            export CARGO_HOME="$VAULTSYNCD_WINDOWS_CACHE/cargo"
            export XWIN_CACHE_DIR="$VAULTSYNCD_WINDOWS_CACHE/xwin"
            mkdir -p "$RUSTUP_HOME" "$CARGO_HOME" "$XWIN_CACHE_DIR"

            # `rustup which cargo` 按 rust-toolchain.toml 解析固定 channel，给出已安装工具链的
            # cargo 路径；这样 flake 里不重复版本号，缺失时也不隐式安装。NixOS 用 nix-ld 提供
            # /lib64 解释器与基础库，rustup 下载的官方二进制可直接运行（S009 实测）。
            if cargo_path=$(rustup which cargo 2>/dev/null); then
              export PATH="$(dirname "$cargo_path"):$PATH"
            else
              echo "vaultsyncd-windows: rust-toolchain.toml 指定的 Rust 工具链尚未安装。" >&2
              echo "  先运行 tests/platform/windows/setup-toolchain.sh，再重进本 shell。" >&2
            fi
          '';
        };
      });
    };
}
