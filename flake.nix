{
  description = "vaultsyncd — 锁定工具链的 Linux 开发 shell";

  inputs = {
    # 选择 nixos-unstable 而不是 nixos-26.05：开发主机本身运行 26.11.20261102（NixOS 26.11
    # 尚未发布稳定通道），稳定通道 26.05 比主机旧一个大版本，Rust/Node 明显更旧；unstable
    # 与主机同源 master，二进制缓存命中率最高。可复现性由 flake.lock 的固定 rev 保证。
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { nixpkgs, ... }:
    let
      # 首发仅 Linux 服务端/控制面在 Nix 内构建；Windows 客户端由 S009 用 rustup 与 MSVC
      # 固定同一 Rust 版本，不在本 flake 做交叉编译，因此这里不列 Windows 系统。
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (pkgs: {
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
      });
    };
}
