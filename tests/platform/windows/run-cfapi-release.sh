#!/usr/bin/env bash
# S012 客体验证：把 CFAPI 原型 exe 传到 Windows 客体，在客体本地 NTFS 上验证释放（dehydrate）
# 与写入保护（protect）：干净样本可释放、释放后再读字节正确、dirty/使用中内容不可释放，
# 以及四类编辑器保存（原地写 / 映射写 / 独占与共享锁 / rename-over）的实际行为。
# 前置：build-cross.sh 已产出 artifacts/windows/<target>/cfapi-prototype.exe；
#       客体已启动（tools/windows-lab/bootstrap-host.sh start / wait-ssh）。
# 不改变 S009/S010/S011 的入口；操作说明与证据见 docs/operations/windows-lab.md §18。
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=tests/platform/windows/common.sh
. "$script_dir/common.sh"

artifact_dir="$(repo_root)/artifacts/$ARTIFACT_SUBDIR"
prototype_exe="$artifact_dir/$CFAPI_PROTOTYPE_BINARY.exe"
if [ ! -f "$prototype_exe" ]; then
    echo "run-cfapi-release: 缺少 $prototype_exe，先运行 build-cross.sh" >&2
    exit 1
fi

echo "== 宿主 SHA-256（$prototype_exe）"
host_hash=$(sha256sum "$prototype_exe" | awk '{ print $1 }')
printf '%s  %s\n' "$host_hash" "$CFAPI_PROTOTYPE_BINARY.exe"

# 宿主侧期望值来自固定内容源：`expect` 与内容源都是平台无关的纯逻辑。
echo "== 宿主侧期望哈希（cargo run --example $CFAPI_PROTOTYPE_BINARY -- expect）"
expect_out=$(cargo run --locked -q -p "$PLATFORM_PACKAGE" --example "$CFAPI_PROTOTYPE_BINARY" -- expect 2>/dev/null)
printf '%s\n' "$expect_out"

guest_dir_win="${CFAPI_RELEASE_GUEST_DIR//\//\\}"
echo "== 客体工作目录：$guest_dir_win；同步根：$CFAPI_RELEASE_VAULT；rename 专用根：$CFAPI_RELEASE_RENAME_VAULT"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir_win' | Out-Null\""

echo "== 传输 exe 与客体脚本"
GUEST_TARGET_DIR="$CFAPI_RELEASE_GUEST_DIR" guest_scp \
    "$prototype_exe" "$script_dir/guest-cfapi-release.ps1"

echo "== 客体执行（protect / release / 编辑器保存场景）"
guest_status=0
guest_output=$(guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $CFAPI_RELEASE_GUEST_DIR/guest-cfapi-release.ps1 -Directory $CFAPI_RELEASE_GUEST_DIR -Vault $CFAPI_RELEASE_VAULT -RenameVault $CFAPI_RELEASE_RENAME_VAULT -Exe $CFAPI_PROTOTYPE_BINARY.exe" 2>&1 |
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

# 客体打印的 `guest_file_sha256` 只覆盖固定内容版本（水合后、释放再水合后、rename 基线），
# 因此每个都必须等于宿主 `expect` 的整份 SHA-256；编辑器改写后的内容用另一个关键字
# （`guest_written_sha256`）打印，由客体自身前后比较。
while read -r name guest_digest; do
    if [ -z "${name:-}" ]; then
        continue
    fi
    expected=$(printf '%s\n' "$expect_out" |
        awk -v name="$name" '$1 == "expect_entry" { for (i = 2; i <= NF; i++) { split($i, kv, "="); if (kv[1] == "name") { entry = kv[2] } ; if (kv[1] == "sha256") { digest = kv[2] } } if (entry == name) { print digest } }')
    compare_digest "file:$name" "$expected" "$guest_digest"
done < <(printf '%s\n' "$guest_output" |
    awk '$1 == "guest_file_sha256" { name = ""; digest = ""; for (i = 2; i <= NF; i++) { split($i, kv, "="); if (kv[1] == "name") { name = kv[2] } ; if (kv[1] == "sha256") { digest = kv[2] } } print name, digest }' |
    sort -u)

pass_count=$(printf '%s\n' "$guest_output" | grep -c '^check=pass ' || true)
fail_count=$(printf '%s\n' "$guest_output" | grep -c '^check=fail ' || true)
blocked_count=$(printf '%s\n' "$guest_output" | grep -c '^blocked_item=' || true)
echo "== 客体检查统计：pass=$pass_count fail=$fail_count blocked=$blocked_count"
if [ "$pass_count" -eq 0 ]; then
    echo "run-cfapi-release: 客体没有输出任何 check=pass（脚本可能没跑起来）" >&2
    exit 1
fi
if [ "$fail_count" -ne 0 ]; then
    echo "run-cfapi-release: 客体检查失败（失败项见上）" >&2
    exit 1
fi
if [ "$mismatch" -ne 0 ]; then
    echo "run-cfapi-release: 两端哈希不一致" >&2
    exit 1
fi
if [ "$guest_status" -ne 0 ]; then
    echo "run-cfapi-release: 客体脚本退出码 $guest_status" >&2
    exit "$guest_status"
fi
if ! printf '%s\n' "$guest_output" | grep -q '^cfapi_release_result: OK$'; then
    echo "run-cfapi-release: 客体未报告 cfapi_release_result: OK" >&2
    exit 1
fi
printf '%s\n' "$guest_output" | grep '^blocked_item=' || true
echo "run-cfapi-release: OK（客体原生执行通过；blocked 项见上，明细见 docs/operations/windows-lab.md §18.4）"
