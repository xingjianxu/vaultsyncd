#!/usr/bin/env bash
# S010 客体验证：把 CFAPI 原型 exe 传到 Windows 客体，在客体本地 NTFS 上执行
# create → inspect → cleanup，并核对两端 SHA-256。
# 前置：build-cross.sh 已产出 artifacts/windows/<target>/cfapi-prototype.exe；
#       客体已启动（tools/windows-lab/bootstrap-host.sh start / wait-ssh）。
# 不改变 S009 的 run-on-guest.sh 用法；操作说明与证据见 docs/operations/windows-lab.md §16。
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=tests/platform/windows/common.sh
. "$script_dir/common.sh"

artifact_dir="$(repo_root)/artifacts/$ARTIFACT_SUBDIR"
prototype_exe="$artifact_dir/$CFAPI_PROTOTYPE_BINARY.exe"
if [ ! -f "$prototype_exe" ]; then
    echo "run-cfapi-prototype: 缺少 $prototype_exe，先运行 build-cross.sh" >&2
    exit 1
fi

echo "== 宿主 SHA-256（$prototype_exe）"
host_hash=$(sha256sum "$prototype_exe" | awk '{ print $1 }')
printf '%s  %s\n' "$host_hash" "$CFAPI_PROTOTYPE_BINARY.exe"

guest_dir_win="${CFAPI_GUEST_DIR//\//\\}"
echo "== 客体工作目录：$guest_dir_win；同步根：$CFAPI_VAULT"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir_win' | Out-Null\""

echo "== 传输 exe 与客体脚本"
# 原型用自己的客体目录，不写进 S009 的 C:\vaultsyncd-lab\s009。
GUEST_TARGET_DIR="$CFAPI_GUEST_DIR" guest_scp "$prototype_exe" "$script_dir/guest-cfapi-prototype.ps1"

echo "== 客体执行（create / inspect / cleanup）"
guest_status=0
# 客体输出是 CRLF；先去掉 CR，后续按行解析哈希。
guest_output=$(guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $CFAPI_GUEST_DIR/guest-cfapi-prototype.ps1 -Directory $CFAPI_GUEST_DIR -Vault $CFAPI_VAULT -Exe $CFAPI_PROTOTYPE_BINARY.exe" 2>&1 |
    tr -d '\r') || guest_status=$?
printf '%s\n' "$guest_output"

echo "== 两端 SHA-256 比对"
guest_hash=$(printf '%s\n' "$guest_output" |
    awk -v file="$CFAPI_PROTOTYPE_BINARY.exe" '$1 == "guest_exe_sha256" && $2 == file { print $3 }' |
    tr 'A-F' 'a-f')
if [ -z "$guest_hash" ]; then
    echo "MISMATCH $CFAPI_PROTOTYPE_BINARY.exe：客体未打印哈希" >&2
    exit 1
elif [ "$host_hash" != "$guest_hash" ]; then
    echo "MISMATCH $CFAPI_PROTOTYPE_BINARY.exe：host=$host_hash guest=$guest_hash" >&2
    exit 1
fi
echo "MATCH $CFAPI_PROTOTYPE_BINARY.exe $host_hash"

if [ "$guest_status" -ne 0 ]; then
    echo "run-cfapi-prototype: 客体检查失败（退出码 $guest_status）" >&2
    exit "$guest_status"
fi
echo "run-cfapi-prototype: OK（客体原生执行通过，占位/大小/清理证据见上方输出）"
