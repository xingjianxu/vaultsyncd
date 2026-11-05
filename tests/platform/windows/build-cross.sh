#!/usr/bin/env bash
# 在宿主交叉编译 Windows x64 冒烟二进制，把产物复制到 artifacts/windows/<target>/ 并
# 写 SHA256SUMS。必须在 `nix develop .#windows` 内运行。
# 操作说明见 docs/operations/windows-lab.md §12。
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=tests/platform/windows/common.sh
. "$script_dir/common.sh"

if [ "${VAULTSYNCD_WINDOWS_SHELL:-}" != "1" ]; then
    echo "build-cross: 请先在 nix develop .#windows 内运行（需要 rustup/cargo-xwin/clang）" >&2
    exit 1
fi

root=$(repo_root)
cd "$root"

# 客体（Windows 11 Enterprise Evaluation）只有 CLR 版 vcruntime140_clr0400.dll，没有
# VCRUNTIME140.dll，动态 CRT 产物会以 0xC0000135 (STATUS_DLL_NOT_FOUND) 退出。冒烟产物
# 因此静态链接 MSVC CRT：导入表只剩 KERNEL32/ntdll/VERSION/cldapi 等系统 DLL，客体
# 不需要安装 VC++ 运行库。（产品客户端的 CRT 部署方式属打包卡决定，不在此固化。）
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static"

echo "== cargo xwin build --release --locked --target $TARGET_TRIPLE -p $TEST_PACKAGE"
cargo xwin build --release --locked --target "$TARGET_TRIPLE" -p "$TEST_PACKAGE"

artifact_dir="$root/artifacts/$ARTIFACT_SUBDIR"
mkdir -p "$artifact_dir"
for binary in "${SMOKE_BINARIES[@]}"; do
    built="target/$TARGET_TRIPLE/release/$binary.exe"
    if [ ! -f "$built" ]; then
        echo "build-cross: 缺少产物 $built" >&2
        exit 1
    fi
    install -m 0644 "$built" "$artifact_dir/$binary.exe"
done

echo "== 产物与 SHA-256（$artifact_dir）"
(
    cd "$artifact_dir"
    for binary in "${SMOKE_BINARIES[@]}"; do
        file "$binary.exe"
    done
    sha256sum -- *.exe | tee SHA256SUMS
)
echo "build-cross: OK"
