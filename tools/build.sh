#!/usr/bin/env bash
# 把当前工作树交叉编译成 dist/ 下的 Windows x64 免安装程序目录，可从任意目录调用。
#
# 与 tools/release.sh 的分工：
# - release.sh/packaging/release.py 走发布链：要求干净已提交的源码，产物按
#   v<版本>-<提交> 归档，含收据、SHA256SUMS 与版本号命名的 ZIP，供 GitHub Release 使用。
# - 本脚本只编译当前代码：允许脏树，不写收据、不记版本号目录，每次清空 dist/ 重新构建，
#   产物直接就是可拷到 Windows 11 的程序目录（无版本号 ZIP 的解压结果）。
# 源码提交、脏树状态与工具链版本记录在 dist/build-info.txt，不伪造发布来源。
set -euo pipefail

vault=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

if [ "$#" -gt 0 ]; then
    echo "build: 不接受参数；产物固定为 $vault/dist" >&2
    exit 2
fi

# 交叉工具链只在 .#windows shell 内提供（rustup 官方 toolchain + cargo-xwin）。
if [ "${VAULTSYNCD_WINDOWS_SHELL:-}" != "1" ]; then
    cd "$vault"
    exec nix develop .#windows --command bash "$vault/tools/build.sh" "$@"
fi

cd "$vault"
out=$vault/dist
target=x86_64-pc-windows-msvc
# 与 cargo xtask release build 相同：静态 CRT（客体无 VCRUNTIME140.dll），忽略静态库
# 预编译 obj 的 LNK4099 调试信息警告。
rustflags="-C target-feature=+crt-static -C link-arg=/ignore:4099"
# PRODUCT_EXECUTABLES 是 installed-files.json 记录的四个已安装程序；
# 专用 ZIP 入口 vaultsyncd-zip.exe 由 packaging/windows/zip/build.py 单独纳入清单。
product_executables=(vaultsyncd-agent.exe vaultsyncd-session.exe vaultsyncd-cli.exe vaultsyncd-desktop.exe)
zip_files=(
    vaultsyncd-agent.exe vaultsyncd-session.exe vaultsyncd-cli.exe vaultsyncd-desktop.exe
    vaultsyncd-zip.exe installed-files.json UNSIGNED-TEST.txt
    "Start vaultsyncd.cmd" "Stop vaultsyncd.cmd" README-ZIP.txt
)

# 端点目录先整体清空：dist 是构造成果，不是需要保留的现场。
echo "== 清空 $out"
rm -rf "$out"
mkdir -p "$out"
stage=$out/.stage
mkdir -p "$stage/payload" "$stage/build"

echo "== pnpm：锁定依赖离线准备"
pnpm install --frozen-lockfile --offline

echo "== 前端：重建 apps/desktop 生产资源"
rm -rf apps/desktop/dist
pnpm --filter @vaultsyncd/desktop build

echo "== Rust：交叉编译 $target release（静态 CRT）"
env -u CARGO_ENCODED_RUSTFLAGS RUSTFLAGS="$rustflags" \
    cargo xwin build --locked --release --target "$target" \
    -p vaultsyncd-agent -p vaultsyncd-cli -p vaultsyncd-desktop --bins
# 尊重调用方的 CARGO_TARGET_DIR（相对路径按仓库根解析），默认用仓库 target/ 以复用缓存。
built=${CARGO_TARGET_DIR:-$vault/target}/$target/release
case $built in /*) ;; *) built=$vault/$built ;; esac
for binary in "${product_executables[@]}" vaultsyncd-zip.exe; do
    if [ ! -f "$built/$binary" ]; then
        echo "build: 缺少交叉编译产物 $built/$binary" >&2
        exit 1
    fi
done
install -m 0644 "${product_executables[@]/#/$built/}" "$stage/payload/"
install -m 0644 "$built/vaultsyncd-zip.exe" "$stage/build/vaultsyncd-zip.exe"

echo "== 来源与程序清单（installed-files.json / zip-files.json）"
commit=$(git rev-parse HEAD)
tree=$(git rev-parse 'HEAD^{tree}')
version=$(python3 -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["workspace"]["package"]["version"])' Cargo.toml)
if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
    dirty=true
else
    dirty=false
fi
export COMMIT="$commit" TREE="$tree" VERSION="$version" DIRTY="$dirty"
python3 - "$stage/payload" <<'PY'
import hashlib, json, os, sys
from pathlib import Path

payload = Path(sys.argv[1])
names = ["vaultsyncd-agent.exe", "vaultsyncd-session.exe", "vaultsyncd-cli.exe", "vaultsyncd-desktop.exe"]
source = {"commit": os.environ["COMMIT"], "tree": os.environ["TREE"],
          "version": os.environ["VERSION"], "dirty": os.environ["DIRTY"] == "true"}


def entry(name):
    data = (payload / name).read_bytes()
    return {"path": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


(payload / "UNSIGNED-TEST.txt").write_text(
    "Unsigned test build. No production signing identity was provided. "
    "Controlled upgrade preserves recovery/state backups; default uninstall removes programs only. "
    "Complete files, dirty content and state are retained; cloud-only placeholders are not complete offline files.\n",
    encoding="utf-8")
installed = {"format": 1, "source": source, "signature_status": "unsigned-test",
             "files": [entry(name) for name in names],
             "startup": {"scope": "current-user", "mechanism": "HKCU Run", "launcher": "vaultsyncd-session.exe"},
             "state": "%LOCALAPPDATA%/vaultsyncd/agent"}
(payload / "installed-files.json").write_text(json.dumps(installed, indent=2) + "\n", encoding="utf-8")
PY

# 复用发布链同一段清单代码：它写 zip-files.json、打无版本号 ZIP 并立即逐文件复核。
echo "== 便携清单与整包复核"
python3 packaging/windows/zip/build.py \
    "$stage/payload" "$stage/build/vaultsyncd-zip.exe" "$vault/packaging/windows/zip" \
    "$stage/unsigned.zip"
mv "$stage/payload/zip"/* "$out"/
rm -rf "$stage"

for name in "${zip_files[@]}"; do
    if [ ! -f "$out/$name" ]; then
        echo "build: 缺少产物 $out/$name" >&2
        exit 1
    fi
done

{
    echo "vaultsyncd Windows x64 开发构建（unsigned-test，不是发布产物，无发布收据）"
    echo "提交：$commit"
    echo "树：$tree"
    echo "版本：$version"
    echo "脏树：$dirty"
    echo "目标：$target"
    echo "RUSTFLAGS：$rustflags"
    echo "构建时间：$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "rustc：$(rustc --version)"
    echo "cargo：$(cargo --version)"
    echo "node：$(node --version)"
    echo "pnpm：$(pnpm --version)"
} >"$out/build-info.txt"

echo "== 产物（$out）"
ls -l "$out"
echo "build: OK。$out 是免安装程序目录：整个目录拷到 Windows 11 本机普通路径后运行 Start vaultsyncd.cmd"
