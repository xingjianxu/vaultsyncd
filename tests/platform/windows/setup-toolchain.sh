#!/usr/bin/env bash
# 把 rust-toolchain.toml 固定的 Rust 工具链与 x86_64-pc-windows-msvc 标准库安装到
# 仓库内被 .gitignore 忽略的 .cache/windows/rustup，并打印工具版本与 xwin 缓存位置。
# 幂等：已安装时只复核。必须在 `nix develop .#windows` 内运行。
# 操作说明见 docs/operations/windows-lab.md §12。
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=tests/platform/windows/common.sh
. "$script_dir/common.sh"

if [ "${VAULTSYNCD_WINDOWS_SHELL:-}" != "1" ]; then
    echo "setup-toolchain: 请先在 nix develop .#windows 内运行（需要 rustup/cargo-xwin/clang）" >&2
    exit 1
fi

root=$(repo_root)
cd "$root"

# 只安装 rust-toolchain.toml 声明的 channel 与 targets；--profile minimal 不下载
# rustfmt/clippy/docs（Linux 侧由锁定 nixpkgs 提供同版本组件）。
echo "== rustup: 安装 rust-toolchain.toml 指定的工具链（channel + targets）"
rustup toolchain install --profile minimal --no-self-update

echo "== 复核工具链"
cargo --version
rustc --version
rustc -vV | sed -n 's/^host: //p; s/^release: //p'
target_libdir=$(rustc --print target-libdir --target "$TARGET_TRIPLE")
if [ ! -d "$target_libdir" ]; then
    echo "setup-toolchain: $TARGET_TRIPLE 标准库目录不存在：$target_libdir" >&2
    exit 1
fi
echo "target_libdir: $target_libdir"
ls "$target_libdir"/libstd-*.rlib >/dev/null

echo "== 复核交叉工具"
cargo xwin --version
clang-cl --version | head -1
lld-link --version | head -1
echo "llvm-lib binary: $(command -v llvm-lib)"

echo "== 版本来源"
echo "rustup binary: $(command -v rustup)"
echo "cargo-xwin binary: $(command -v cargo-xwin)"
echo "clang-cl binary: $(command -v clang-cl)"
for path in "$RUSTUP_HOME" "$CARGO_HOME" "$XWIN_CACHE_DIR"; do
    echo "cache: $path ($(du -sh "$path" | cut -f1))"
done
echo "setup-toolchain: OK（Windows SDK/CRT 由 build-cross.sh 首次调用 cargo-xwin 时下载）"
