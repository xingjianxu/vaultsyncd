#!/usr/bin/env bash
# 把 build-cross.sh 的产物传到 Windows 客体、原生执行，并核对两端 SHA-256。
# 前置：客体已启动（tools/windows-lab/bootstrap-host.sh start / wait-ssh）。
# 只传 exe 与 guest-run.ps1，不复制 target/、node_modules/。
# 操作说明与实测证据见 docs/operations/windows-lab.md §12。
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=tests/platform/windows/common.sh
. "$script_dir/common.sh"

artifact_dir="$(repo_root)/artifacts/$ARTIFACT_SUBDIR"
for binary in "${SMOKE_BINARIES[@]}"; do
    if [ ! -f "$artifact_dir/$binary.exe" ]; then
        echo "run-on-guest: 缺少 $artifact_dir/$binary.exe，先运行 build-cross.sh" >&2
        exit 1
    fi
done

echo "== 宿主 SHA-256（$artifact_dir）"
host_hashes=$(cd "$artifact_dir" && sha256sum -- *.exe)
printf '%s\n' "$host_hashes"

guest_dir_win="${GUEST_DIR//\//\\}"
echo "== 客体目录：$guest_dir_win"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir_win' | Out-Null\""

echo "== 传输产物与 guest-run.ps1"
guest_scp "$artifact_dir"/*.exe "$script_dir/guest-run.ps1"

exe_args=""
for binary in "${SMOKE_BINARIES[@]}"; do
    exe_args="${exe_args:+$exe_args,}$binary.exe"
done

echo "== 客体执行"
guest_status=0
# 客体输出是 CRLF；先去掉 CR，后续按行解析哈希。
guest_output=$(guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $GUEST_DIR/guest-run.ps1 -Directory $GUEST_DIR -Executables $exe_args" 2>&1 |
    tr -d '\r') || guest_status=$?
printf '%s\n' "$guest_output"

echo "== 两端 SHA-256 比对"
mismatch=0
for binary in "${SMOKE_BINARIES[@]}"; do
    host_hash=$(printf '%s\n' "$host_hashes" | awk -v file="$binary.exe" '$2 == file { print $1 }')
    guest_hash=$(printf '%s\n' "$guest_output" |
        awk -v file="$binary.exe" '$1 == "guest_sha256" && $2 == file { print $3 }' |
        tr 'A-F' 'a-f')
    if [ -z "$host_hash" ] || [ -z "$guest_hash" ]; then
        echo "MISMATCH $binary.exe：缺少哈希（host='$host_hash' guest='$guest_hash'）" >&2
        mismatch=1
    elif [ "$host_hash" != "$guest_hash" ]; then
        echo "MISMATCH $binary.exe：host=$host_hash guest=$guest_hash" >&2
        mismatch=1
    else
        echo "MATCH $binary.exe $host_hash"
    fi
done

if [ "$guest_status" -ne 0 ]; then
    echo "run-on-guest: 客体执行失败（退出码 $guest_status）" >&2
    exit "$guest_status"
fi
if [ "$mismatch" -ne 0 ]; then
    echo "run-on-guest: 产物哈希不一致" >&2
    exit 1
fi
echo "run-on-guest: OK（客体原生执行成功，两端 SHA-256 一致）"
