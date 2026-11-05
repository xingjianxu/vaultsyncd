#!/usr/bin/env bash
# S011 客体验证：把 CFAPI 原型 exe 传到 Windows 客体，在客体本地 NTFS 上验证内容供给
# （整份读取、范围水合、离线失败、超时、传输中断、重复请求），并做两端 SHA-256 比对。
# 前置：build-cross.sh 已产出 artifacts/windows/<target>/cfapi-prototype.exe；
#       客体已启动（tools/windows-lab/bootstrap-host.sh start / wait-ssh）。
# 不改变 S009/S010 的入口；操作说明与证据见 docs/operations/windows-lab.md §17。
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=tests/platform/windows/common.sh
. "$script_dir/common.sh"

artifact_dir="$(repo_root)/artifacts/$ARTIFACT_SUBDIR"
prototype_exe="$artifact_dir/$CFAPI_PROTOTYPE_BINARY.exe"
if [ ! -f "$prototype_exe" ]; then
    echo "run-cfapi-hydration: 缺少 $prototype_exe，先运行 build-cross.sh" >&2
    exit 1
fi

echo "== 宿主 SHA-256（$prototype_exe）"
host_hash=$(sha256sum "$prototype_exe" | awk '{ print $1 }')
printf '%s  %s\n' "$host_hash" "$CFAPI_PROTOTYPE_BINARY.exe"

# 宿主侧期望值来自固定内容源（同一份 Rust 生成器 + 自带 SHA-256，NIST 向量已锁定）。
# example 的 `expect` 子命令平台无关，可在这里用宿主目标直接运行。
echo "== 宿主侧期望哈希（cargo run --example $CFAPI_PROTOTYPE_BINARY -- expect）"
expect_out=$(cargo run --locked -q -p "$PLATFORM_PACKAGE" --example "$CFAPI_PROTOTYPE_BINARY" -- expect 2>/dev/null)
printf '%s\n' "$expect_out"
range_out=$(cargo run --locked -q -p "$PLATFORM_PACKAGE" --example "$CFAPI_PROTOTYPE_BINARY" -- \
    expect --name "$CFAPI_HYDRATION_RANGE_ENTRY" --offset "$CFAPI_HYDRATION_RANGE_OFFSET" \
    --length "$CFAPI_HYDRATION_RANGE_LENGTH" 2>/dev/null)
printf '%s\n' "$range_out"

guest_dir_win="${CFAPI_HYDRATION_GUEST_DIR//\//\\}"
echo "== 客体工作目录：$guest_dir_win；同步根：$CFAPI_ROOT"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir_win' | Out-Null\""

echo "== 传输 exe 与客体脚本"
GUEST_TARGET_DIR="$CFAPI_HYDRATION_GUEST_DIR" guest_scp \
    "$prototype_exe" "$script_dir/guest-cfapi-hydration.ps1"

echo "== 客体执行（读取 / 范围 / 离线 / 超时 / 中断 / 重复请求）"
guest_status=0
guest_output=$(guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $CFAPI_HYDRATION_GUEST_DIR/guest-cfapi-hydration.ps1 -Directory $CFAPI_HYDRATION_GUEST_DIR -Root $CFAPI_ROOT -Exe $CFAPI_PROTOTYPE_BINARY.exe" 2>&1 |
    tr -d '\r') || guest_status=$?
printf '%s\n' "$guest_output"

echo "== 两端 SHA-256 比对"
mismatch=0
guest_hash=$(printf '%s\n' "$guest_output" |
    awk -v file="$CFAPI_PROTOTYPE_BINARY.exe" '$1 == "guest_exe_sha256" && $2 == file { print $3 }' |
    tr 'A-F' 'a-f')
if [ -z "$guest_hash" ]; then
    echo "MISMATCH $CFAPI_PROTOTYPE_BINARY.exe：客体未打印哈希" >&2
    mismatch=1
elif [ "$host_hash" != "$guest_hash" ]; then
    echo "MISMATCH $CFAPI_PROTOTYPE_BINARY.exe：host=$host_hash guest=$guest_hash" >&2
    mismatch=1
else
    echo "MATCH $CFAPI_PROTOTYPE_BINARY.exe $host_hash"
fi

compare_digest() {
    local label="$1" expected="$2" actual="$3"
    if [ -z "$expected" ] || [ -z "$actual" ]; then
        echo "MISMATCH $label：缺少期望值或客体哈希（expected='$expected' actual='$actual'）" >&2
        mismatch=1
    elif [ "$expected" != "$actual" ]; then
        echo "MISMATCH $label：host=$expected guest=$actual" >&2
        mismatch=1
    else
        echo "MATCH $label $expected"
    fi
}

# 整份读取：客体 PowerShell Get-FileHash 与宿主 `expect` 的 SHA-256 必须一致。
while read -r name guest_digest; do
    if [ -z "${name:-}" ]; then
        continue
    fi
    expected=$(printf '%s\n' "$expect_out" |
        awk -v name="$name" '$1 == "expect_entry" { for (i = 2; i <= NF; i++) { split($i, kv, "="); if (kv[1] == "name") { entry = kv[2] } ; if (kv[1] == "sha256") { digest = kv[2] } } if (entry == name) { print digest } }')
    compare_digest "file:$name" "$expected" "$guest_digest"
done < <(printf '%s\n' "$guest_output" |
    awk '$1 == "guest_file_sha256" { name = ""; digest = ""; for (i = 2; i <= NF; i++) { split($i, kv, "="); if (kv[1] == "name") { name = kv[2] } ; if (kv[1] == "sha256") { digest = kv[2] } } print name, digest }')

# 范围读取：客体按区间独立计算的 SHA-256 与宿主 `expect_range` 必须一致。
while read -r name guest_digest; do
    if [ -z "${name:-}" ]; then
        continue
    fi
    expected=$(printf '%s\n' "$range_out" |
        awk -v name="$name" '$1 == "expect_range" { for (i = 2; i <= NF; i++) { split($i, kv, "="); if (kv[1] == "name") { entry = kv[2] } ; if (kv[1] == "sha256") { digest = kv[2] } } if (entry == name) { print digest } }')
    compare_digest "range:$name" "$expected" "$guest_digest"
done < <(printf '%s\n' "$guest_output" |
    awk '$1 == "guest_range_sha256" { name = ""; digest = ""; for (i = 2; i <= NF; i++) { split($i, kv, "="); if (kv[1] == "name") { name = kv[2] } ; if (kv[1] == "sha256") { digest = kv[2] } } print name, digest }')

pass_count=$(printf '%s\n' "$guest_output" | grep -c '^check=pass ' || true)
fail_count=$(printf '%s\n' "$guest_output" | grep -c '^check=fail ' || true)
echo "== 客体检查统计：pass=$pass_count fail=$fail_count"
if [ "$pass_count" -eq 0 ]; then
    echo "run-cfapi-hydration: 客体没有输出任何 check=pass（脚本可能没跑起来）" >&2
    exit 1
fi
if [ "$fail_count" -ne 0 ]; then
    echo "run-cfapi-hydration: 客体检查失败（失败项见上）" >&2
    exit 1
fi
if [ "$mismatch" -ne 0 ]; then
    echo "run-cfapi-hydration: 两端哈希不一致" >&2
    exit 1
fi
if [ "$guest_status" -ne 0 ]; then
    echo "run-cfapi-hydration: 客体脚本退出码 $guest_status" >&2
    exit "$guest_status"
fi
if ! printf '%s\n' "$guest_output" | grep -q '^cfapi_hydration_result: OK$'; then
    echo "run-cfapi-hydration: 客体未报告 cfapi_hydration_result: OK" >&2
    exit 1
fi
printf '%s\n' "$guest_output" | grep '^cancel_callback_observed=' || true
echo "run-cfapi-hydration: OK（客体原生执行通过，逐项证据见上方输出）"
