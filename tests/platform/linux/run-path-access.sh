#!/usr/bin/env bash
# S032 验收：Linux 安全路径访问正式接口。
#
# 覆盖的验收点（实施手册 S032）：
#   1) Vault 句柄下的列举/打开/创建不跟随符号链接，也不因“换目标”竞态访问外部样本；
#   2) 目录别名（bind mount 把外部目录挂进 Vault）不穿越挂载边界：不列举、不读取、不写入；
#   3) 权限失败返回明确错误，不返回空目录（不把扫描故障当删除）；
#   4) Vault 与内部目录的父子/别名/bind 重叠被识别，字符串前缀不算父子；
#   5) Vault 身份可复核对：路径被替换、被 bind 覆盖、换成别的子卷或后端时报出漂移原因。
#
# 全部样本只在 tools/test-volume/test-volume.sh 建立的 run 目录内，结束时清理并核对。
# 运行：nix develop --command bash tests/platform/linux/run-path-access.sh
set -euo pipefail
PROG=${0##*/}
# shellcheck source=tests/platform/linux/common.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

require_dev_shell
require_sudo_noninteractive
require_tools sha256sum

run_id=${VAULTSYNCD_RUN_ID:-s032-path-$$}
RUN_DIR=$LAB_VAULT/runs/$run_id
VOLUME=$RUN_DIR/volume
VAULT_DIR=$VOLUME/vault
EXTERNAL=$RUN_DIR/external
SNAPSHOT=$RUN_DIR/vault.snapshot
MARKER=leaked-marker-s032
EXTERNAL_FILE=secret.txt

declare -a TRACK_MOUNTS=() TRACK_SUBVOLS=() TRACK_PATHS=() TRACK_SUDO_PATHS=()
MOUNTS_BEFORE=""
LOOPS_BEFORE=""

track_mount() { TRACK_MOUNTS+=("$1"); }
track_subvol() { TRACK_SUBVOLS+=("$1"); }
track_path() { TRACK_PATHS+=("$1"); }
track_sudo_path() { TRACK_SUDO_PATHS+=("$1"); }

cleanup() {
	local status=$?
	set +e
	log ""
	step "清理 S032 挂载、子卷与样本"
	local index path subvol
	if [ "${#TRACK_MOUNTS[@]}" -gt 0 ]; then
		for ((index = ${#TRACK_MOUNTS[@]} - 1; index >= 0; index--)); do
			umount_tolerant "${TRACK_MOUNTS[index]}"
		done
	fi
	if [ "${#TRACK_SUDO_PATHS[@]}" -gt 0 ]; then
		for ((index = ${#TRACK_SUDO_PATHS[@]} - 1; index >= 0; index--)); do
			path=${TRACK_SUDO_PATHS[index]}
			case $path in
			"$RUN_DIR"/*) sudo_run rm -rf -- "$path" ;;
			*) log "跳过非本 run 的 sudo 路径：$path" ;;
			esac
		done
	fi
	if [ "${#TRACK_SUBVOLS[@]}" -gt 0 ]; then
		for ((index = ${#TRACK_SUBVOLS[@]} - 1; index >= 0; index--)); do
			subvol=${TRACK_SUBVOLS[index]}
			case $subvol in
			"$RUN_DIR"/*) ;;
			*)
				log "跳过非本 run 子卷：$subvol"
				continue
				;;
			esac
			if [ -d "$subvol" ]; then
				sudo_run rm -rf -- "$subvol"/* "$subvol"/.[!.]* 2>/dev/null
				sudo_run btrfs subvolume delete -- "$subvol" >/dev/null || log "子卷删除失败：$subvol"
			fi
		done
	fi
	if [ "${#TRACK_PATHS[@]}" -gt 0 ]; then
		for ((index = ${#TRACK_PATHS[@]} - 1; index >= 0; index--)); do
			path=${TRACK_PATHS[index]}
			case $path in
			"$RUN_DIR"/*)
				if [ -e "$path" ] || [ -L "$path" ]; then
					rm -rf -- "$path"
				fi
				;;
			*) log "跳过非本 run 路径：$path" ;;
			esac
		done
	fi
	if [ -d "$RUN_DIR" ]; then
		rm -f -- "$SNAPSHOT" 2>/dev/null
		"$TEST_VOLUME" cleanup "$run_id" || log "test-volume.sh cleanup 失败，请人工核查 $RUN_DIR"
	fi
	if [ -e "$RUN_DIR" ]; then
		log "run 目录仍存在：$RUN_DIR"
	else
		log "run 目录已删除：$RUN_DIR"
	fi
	if [ "$(snapshot_loops)" != "$LOOPS_BEFORE" ]; then
		log "注意：losetup 与开始时不一致"
		diff <(printf '%s\n' "$LOOPS_BEFORE") <(snapshot_loops) || true
	else
		log "losetup -a 与开始时一致"
	fi
	if [ "$(snapshot_mounts)" != "$MOUNTS_BEFORE" ]; then
		log "注意：挂载表与开始时不一致"
		diff <(printf '%s\n' "$MOUNTS_BEFORE") <(snapshot_mounts) || true
	else
		log "findmnt 与开始时一致"
	fi
	log "cleanup status=$status"
}
trap cleanup EXIT

sha_of() { sha256sum -- "$1" | awk '{print $1}'; }

# expect_no_entries <label>：最近一次输出里不得出现任何目录项（说明没有列出外部样本）。
expect_no_entries() {
	local label=$1
	if grep -q '^entry=' <<<"$CMD_OUT"; then
		fail_case "$label：输出里列出了外部样本的目录项"
		printf '%s\n' "$CMD_OUT" >&2
	else
		pass "$label：未列出外部样本目录项"
	fi
}

# expect_no_content_leak <label>：最近一次输出里不得出现外部样本的内容标记。
expect_no_content_leak() {
	local label=$1
	if grep -q "$MARKER" <<<"$CMD_OUT"; then
		fail_case "$label：输出里出现了外部样本内容"
		printf '%s\n' "$CMD_OUT" >&2
	else
		pass "$label：未读取外部样本内容"
	fi
}

# bash 的 run_path_access 把输出放在 CMD_OUT/CMD_STATUS。
step "0) 建立 run 目录与样本（run_id=$run_id）"
MOUNTS_BEFORE=$(snapshot_mounts)
LOOPS_BEFORE=$(snapshot_loops)
"$TEST_VOLUME" create "$run_id"
own_run_path "$RUN_DIR/volume"
build_path_access_bin

mkdir -p -- "$VAULT_DIR/sub" "$VOLUME/vaultx" "$EXTERNAL"
track_sudo_path "$VAULT_DIR"
track_path "$VOLUME/vaultx"
track_path "$EXTERNAL"
printf 'hello-s032\n' >"$VAULT_DIR/doc.bin"
printf 'inside\n' >"$VAULT_DIR/sub/inside.txt"
printf '%s\n' "$MARKER" >"$EXTERNAL/$EXTERNAL_FILE"
external_sha=$(sha_of "$EXTERNAL/$EXTERNAL_FILE")

step "1) Vault 句柄列举：openat2 与逐段打开结果一致"
run_path_access list "$VAULT_DIR" ""
expect_status "vault 列举退出码" 0
expect_field "vault 列举机制" resolve openat2
expect_field "vault 列出的条目数" entries 2
expect_line "vault 列举包含 doc.bin" '^entry=doc.bin kind=file '
expect_line "vault 列举包含 sub 目录" '^entry=sub kind=dir '
vault_mount_id=$(field vault_mount_id)
if [ -n "$vault_mount_id" ] && [ "$vault_mount_id" != "none" ]; then
	pass "vault 挂载 ID 已取得（$vault_mount_id）"
else
	fail_case "vault 挂载 ID 缺失"
fi

run_path_access list "$VAULT_DIR" "sub"
expect_field "子目录列举机制" resolve openat2
expect_field "子目录条目数" entries 1
expect_line "子目录条目是普通文件" '^entry=inside.txt kind=file '

run_path_access --resolve walk list "$VAULT_DIR" ""
expect_status "逐段打开列举退出码" 0
expect_field "逐段打开机制" resolve component_walk
expect_field "逐段打开与 openat2 条目数相同" entries 2

step "2) 符号链接逃逸：目录、文件与中间段都不被跟随"
ln -s -- "$EXTERNAL" "$VAULT_DIR/link-dir"
ln -s -- "$EXTERNAL/$EXTERNAL_FILE" "$VAULT_DIR/link-file"
track_path "$VAULT_DIR/link-dir"
track_path "$VAULT_DIR/link-file"

run_path_access list "$VAULT_DIR" link-dir
expect_status "列举符号链接目录：退出码 3" 3
expect_decision "列举符号链接目录被拒绝" refused symlink_rejected
expect_no_entries "列举符号链接目录"

run_path_access --resolve walk list "$VAULT_DIR" link-dir
expect_status "逐段打开列举符号链接目录：退出码 3" 3
expect_no_entries "逐段打开列举符号链接目录"

run_path_access open "$VAULT_DIR" link-file
expect_status "打开指向外部的符号链接文件：退出码 3" 3
expect_decision "打开符号链接文件被拒绝" refused symlink_rejected
expect_no_content_leak "打开符号链接文件"

ln -s -- "$EXTERNAL" "$VAULT_DIR/sub/external-link"
track_path "$VAULT_DIR/sub/external-link"
run_path_access list "$VAULT_DIR" sub/external-link
expect_status "中间段符号链接：退出码 3" 3
expect_no_entries "中间段符号链接"

run_path_access list "$VAULT_DIR" ../external
expect_status ".. 逃逸：退出码 3" 3
expect_decision ".. 逃逸被拒绝" refused invalid_relative_path
run_path_access list "$VAULT_DIR" "$EXTERNAL"
expect_status "绝对路径：退出码 3" 3
expect_decision "绝对路径被拒绝" refused invalid_relative_path
if [ "$(sha_of "$EXTERNAL/$EXTERNAL_FILE")" = "$external_sha" ]; then
	pass "外部样本未被读取/改动（哈希不变）"
else
	fail_case "外部样本哈希变化，说明被访问过"
fi

step "3) 符号链接换目标竞态：任意时刻都不访问外部样本"
mkdir -p -- "$VAULT_DIR/race"
printf 'inside\n' >"$VAULT_DIR/race/inside.txt"
track_path "$VAULT_DIR/race"
(
	for _ in $(seq 1 120); do
		rm -rf -- "$VAULT_DIR/race" 2>/dev/null
		ln -s -- "$EXTERNAL" "$VAULT_DIR/race" 2>/dev/null || true
		rm -f -- "$VAULT_DIR/race" 2>/dev/null
		mkdir -p -- "$VAULT_DIR/race" 2>/dev/null || true
		printf 'inside\n' >"$VAULT_DIR/race/inside.txt" 2>/dev/null || true
		# 目录状态多停留一点，让“攻击进行中的正常列举”也有足够样本；
		# 仍然是并发换目标，不是先停攻击再验证。
		sleep 0.01
	done
) &
attacker=$!
race_ok=0
race_refused=0
race_leak=0
for _ in $(seq 1 40); do
	run_path_access list "$VAULT_DIR" race
	case $CMD_STATUS in
	0)
		race_ok=$((race_ok + 1))
		# 竞态里目录可能刚被重建、inside.txt 尚未写入：entries=0 是 Vault 内的合法中间状态。
		if grep -q '^entry=' <<<"$CMD_OUT" && ! grep -q '^entry=inside.txt kind=file ' <<<"$CMD_OUT"; then
			race_leak=$((race_leak + 1))
			printf '%s\n' "$CMD_OUT" >&2
		fi
		;;
	3) race_refused=$((race_refused + 1)) ;;
	*) race_leak=$((race_leak + 1)) ; printf '%s\n' "$CMD_OUT" >&2 ;;
	esac
	if grep -q "$MARKER" <<<"$CMD_OUT"; then
		race_leak=$((race_leak + 1))
	fi
done
wait "$attacker" 2>/dev/null || true
if [ "$race_leak" -eq 0 ]; then
	pass "换目标竞态：40 次访问全部只看到 Vault 内文件或明确拒绝（成功 $race_ok 次、拒绝 $race_refused 次）"
else
	fail_case "换目标竞态出现 $race_leak 次异常结果"
fi

step "4) 目录别名（bind mount 外部目录进 Vault）：不跨挂载边界"
mkdir -p -- "$VAULT_DIR/alias"
track_path "$VAULT_DIR/alias"
mount_bind "$EXTERNAL" "$VAULT_DIR/alias"
track_mount "$VAULT_DIR/alias"

run_path_access list "$VAULT_DIR" alias
expect_status "列举 bind 别名目录：退出码 3" 3
expect_decision "列举 bind 别名目录被拒绝" refused mount_boundary_crossed
expect_no_entries "列举 bind 别名目录"
expect_line "报告 Vault 挂载与实际挂载" '^detail=.*Vault 挂载 [0-9]+，实际 [0-9]+'

run_path_access open "$VAULT_DIR" "alias/$EXTERNAL_FILE"
expect_status "打开 bind 别名里的文件：退出码 3" 3
expect_decision "打开 bind 别名里的文件被拒绝" refused mount_boundary_crossed
expect_no_content_leak "打开 bind 别名里的文件"

run_path_access create "$VAULT_DIR" "alias/pwned.txt"
expect_status "在 bind 别名里创建：退出码 3" 3
expect_decision "在 bind 别名里创建被拒绝" refused mount_boundary_crossed
if [ -e "$EXTERNAL/pwned.txt" ]; then
	fail_case "外部样本目录被写入了 pwned.txt"
else
	pass "外部样本目录没有被写入（未创建 pwned.txt）"
fi
umount_checked "$VAULT_DIR/alias"
unset 'TRACK_MOUNTS[-1]'

step "5) 权限失败返回明确错误，不返回空目录"
sudo_run mkdir -p -- "$VAULT_DIR/locked"
sudo_run sh -c "printf 'secret\\n' >'$VAULT_DIR/locked/secret.txt'"
sudo_run chmod 700 "$VAULT_DIR/locked"
track_sudo_path "$VAULT_DIR/locked"

run_path_access list "$VAULT_DIR" locked
expect_status "列举无权限目录：退出码 3" 3
expect_decision "列举无权限目录被拒绝" refused permission_denied
if grep -qx 'entries=.*' <<<"$CMD_OUT"; then
	fail_case "无权限目录返回了 entries= 行（被当成了空目录）"
	printf '%s\n' "$CMD_OUT" >&2
else
	pass "无权限目录没有返回 entries= 行（不是空目录）"
fi
expect_line "保留原始 syscall 错误" '^detail=.*Permission denied'

run_path_access open "$VAULT_DIR" locked/secret.txt
expect_status "打开无权限目录内的文件：退出码 3" 3
expect_decision "打开无权限文件被拒绝" refused permission_denied

run_path_access list "$VAULT_DIR" ""
expect_status "有权限部分仍可列举" 0
expect_line "被锁目录本身仍作为条目出现" '^entry=locked kind=dir '

step "6) 创建：Vault 内新建成功、父目录被拒时不落盘"
run_path_access create "$VAULT_DIR" created.txt
expect_status "Vault 内创建退出码" 0
expect_field "创建后的挂载与 Vault 一致" mount_id "$vault_mount_id"
expect_field "创建文件权限" mode 0644
expect_field "创建后已刷盘" fsynced true
if [ -f "$VAULT_DIR/created.txt" ]; then
	pass "创建的文件存在于 Vault 内"
else
	fail_case "创建的文件不存在"
fi
run_path_access create "$VAULT_DIR" created.txt
expect_status "重复创建同一名称：退出码 3" 3
expect_decision "重复创建被拒绝" refused already_exists
run_path_access create "$VAULT_DIR" sub/nested.txt
expect_status "父目录是普通子目录：退出码 0" 0
expect_field "子目录内创建的挂载一致" mount_id "$vault_mount_id"

run_path_access create "$VAULT_DIR" locked/inside.txt
expect_status "父目录无权限时创建：退出码 3" 3
expect_decision "父目录无权限时创建被拒绝" refused permission_denied
if sudo_run test -e "$VAULT_DIR/locked/inside.txt"; then
	fail_case "无权限目录里被写入了 inside.txt"
else
	pass "无权限目录里没有落盘"
fi

step "7) Vault 身份复核与漂移原因"
run_path_access snapshot "$VAULT_DIR"
expect_status "读取 Vault 快照退出码" 0
expect_field "快照后端" backend btrfs
expect_line "快照记录挂载 ID" "^mount_id=$vault_mount_id$"
printf '%s\n' "$CMD_OUT" >"$SNAPSHOT"

run_path_access verify --expect-from "$SNAPSHOT" "$VAULT_DIR"
expect_status "未变化时复核退出码" 0
expect_field "未变化时复核结果" verify unchanged

mount_bind "$VAULT_DIR" "$VAULT_DIR"
track_mount "$VAULT_DIR"
run_path_access verify --expect-from "$SNAPSHOT" "$VAULT_DIR"
expect_status "路径被同目录 bind 覆盖：退出码 3" 3
expect_decision "路径被 bind 覆盖时报告挂载漂移" refused mount_changed
umount_checked "$VAULT_DIR"
unset 'TRACK_MOUNTS[-1]'

sudo_run btrfs subvolume create -- "$RUN_DIR/state" >/dev/null
track_subvol "$RUN_DIR/state"
own_run_path "$RUN_DIR/state"
mount_bind "$RUN_DIR/state" "$VAULT_DIR"
track_mount "$VAULT_DIR"
run_path_access verify --expect-from "$SNAPSHOT" "$VAULT_DIR"
expect_status "换成别的子卷：退出码 3" 3
expect_decision "换成别的子卷时报告子卷漂移" refused subvolume_changed
umount_checked "$VAULT_DIR"
unset 'TRACK_MOUNTS[-1]'

mount_tmpfs "$VAULT_DIR"
track_mount "$VAULT_DIR"
run_path_access verify --expect-from "$SNAPSHOT" "$VAULT_DIR"
expect_status "换成 tmpfs：退出码 3" 3
expect_decision "换成不支持的后端时报告后端漂移" refused backend_changed
umount_checked "$VAULT_DIR"
unset 'TRACK_MOUNTS[-1]'

mv -- "$VAULT_DIR" "$VAULT_DIR-old"
track_path "$VAULT_DIR-old"
mkdir -p -- "$VAULT_DIR"
run_path_access verify --expect-from "$SNAPSHOT" "$VAULT_DIR"
expect_status "目录被替换：退出码 3" 3
expect_decision "目录被替换时报告目录漂移" refused directory_changed
rm -rf -- "$VAULT_DIR"
mv -- "$VAULT_DIR-old" "$VAULT_DIR"
run_path_access verify --expect-from "$SNAPSHOT" "$VAULT_DIR"
expect_status "换回原目录后复核：退出码 0" 0
expect_field "换回原目录后复核结果" verify unchanged

step "8) Vault 与内部目录的父子/别名/bind 重叠"
mkdir -p -- "$RUN_DIR/state-plain" "$VAULT_DIR/state-inside"
track_path "$RUN_DIR/state-plain"
track_path "$VAULT_DIR/state-inside"

run_path_access overlap --vault "$VAULT_DIR" --internal "$RUN_DIR/state-plain"
expect_status "兄弟目录无重叠：退出码 0" 0
expect_field "兄弟目录重叠数" overlap 0
expect_field "兄弟目录判定" decision clean

run_path_access overlap --vault "$VAULT_DIR" --vault "$VOLUME/vaultx"
expect_status "字符串前缀不是父子：退出码 0" 0
expect_field "前缀陷阱重叠数" overlap 0

run_path_access overlap --vault "$VAULT_DIR" --internal "$VAULT_DIR/state-inside"
expect_status "内部目录在 Vault 内：退出码 3" 3
expect_line "内部目录在 Vault 内的原因" 'kind=internal_inside_vault'

run_path_access overlap --vault "$VAULT_DIR" --vault "$VAULT_DIR/sub"
expect_status "两个 Vault 父子重叠：退出码 3" 3
expect_line "Vault 父子重叠的原因" 'kind=parent_child'

run_path_access overlap --vault "$VAULT_DIR" --vault "$VAULT_DIR"
expect_status "同一路径注册两次：退出码 3" 3
expect_line "同一路径的原因" 'kind=same_path'

mkdir -p -- "$RUN_DIR/alias-internal"
track_path "$RUN_DIR/alias-internal"
mount_bind "$EXTERNAL" "$RUN_DIR/alias-internal"
track_mount "$RUN_DIR/alias-internal"
run_path_access overlap --vault "$EXTERNAL" --internal "$RUN_DIR/alias-internal"
expect_status "bind 别名重叠：退出码 3" 3
expect_line "bind 别名重叠的原因" 'kind=alias'
alias_mount_ids=$(sed -n 's/.* mount_ids=//p' <<<"$CMD_OUT")
alias_left=${alias_mount_ids%%:*}
alias_right=${alias_mount_ids##*:}
if [ -n "$alias_left" ] && [ "$alias_left" != "$alias_right" ]; then
	pass "bind 别名的挂载 ID 不同（$alias_left vs $alias_right），身份比较没有只看路径"
else
	fail_case "bind 别名挂载 ID 相同或缺失：$alias_mount_ids"
fi
umount_checked "$RUN_DIR/alias-internal"
unset 'TRACK_MOUNTS[-1]'

step "9) 用法错误与失败路径不静默成功"
run_path_access list "$VAULT_DIR"
expect_status "缺少参数：退出码 2" 2
run_path_access overlap --vault "$VAULT_DIR"
expect_status "overlap 只有一个目录：退出码 2" 2
run_path_access verify --expect-from "$SNAPSHOT"
expect_status "verify 缺少 VAULT：退出码 2" 2
run_path_access snapshot "$RUN_DIR/does-not-exist"
expect_status "不存在的 Vault：退出码 1" 1
expect_field "不存在的 Vault 原因" decision failed

summary || die "存在失败用例"
