#!/usr/bin/env bash
# S013 验收 2/2：同卷跨子卷 reflink（FICLONE）能力与跨文件系统边界。
#
# design §11.5 要求“正常同卷 CoW 样本必须通过 reflink 原生验收”。S013 只在原型层面证明该卷
# 支持跨子卷克隆，并把跨文件系统的失败如实记录；`files/` 里的 FICLONE 封装属 S014。
#
# 运行：nix develop --command bash tests/platform/linux/run-reflink.sh
set -euo pipefail
PROG=${0##*/}
# shellcheck source=tests/platform/linux/common.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

require_dev_shell
require_sudo_noninteractive
require_tools filefrag chattr

run_id=${VAULTSYNCD_RUN_ID:-s013-reflink-$$}
RUN_DIR=$LAB_VAULT/runs/$run_id
SOURCE_SUBVOL=$RUN_DIR/reflink-a
TARGET_SUBVOL=$RUN_DIR/reflink-b

declare -a TRACK_SUBVOLS=() TRACK_PATHS=()
MOUNTS_BEFORE=""
LOOPS_BEFORE=""

track_subvol() { TRACK_SUBVOLS+=("$1"); }
track_path() { TRACK_PATHS+=("$1"); }

cleanup() {
	local status=$?
	set +e
	log ""
	step "清理 reflink 样本子卷"
	local index subvol path
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
				rm -rf -- "$subvol"/* "$subvol"/.[!.]* 2>/dev/null
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

step "0) 建立 run 目录（run_id=$run_id）"
MOUNTS_BEFORE=$(snapshot_mounts)
LOOPS_BEFORE=$(snapshot_loops)
"$TEST_VOLUME" create "$run_id"
LOOP_MOUNT=$RUN_DIR/mnt
own_run_path "$RUN_DIR/volume"
own_run_path "$LOOP_MOUNT"

step "1) 在 /data 卷上建立两个兄弟子卷（同一 btrfs 文件系统，不同子卷）"
for subvol in "$SOURCE_SUBVOL" "$TARGET_SUBVOL"; do
	sudo_run btrfs subvolume create -- "$subvol" >/dev/null
	track_subvol "$subvol"
	own_run_path "$subvol"
done
src_source=$(sudo_run btrfs subvolume show -- "$SOURCE_SUBVOL" |
	awk -F: '/Subvolume ID/ { gsub(/[ \t]/, "", $2); print $2; exit }')
dst_source=$(sudo_run btrfs subvolume show -- "$TARGET_SUBVOL" |
	awk -F: '/Subvolume ID/ { gsub(/[ \t]/, "", $2); print $2; exit }')
[ -n "$src_source" ] && [ -n "$dst_source" ] || die "无法读取子卷 ID"
if [ "$src_source" != "$dst_source" ]; then
	pass "两个样本子卷 ID 不同（$src_source vs $dst_source）"
else
	fail_case "两个样本子卷 ID 相同：$src_source"
fi

step "2) 写入 4 MiB 随机内容并统计已分配块"
payload=$SOURCE_SUBVOL/src.bin
dd if=/dev/urandom of="$payload" bs=1M count=4 status=none
sync -- "$payload"
src_blocks=$(stat -c %b -- "$payload")
log "源文件 $payload：逻辑 $(stat -c %s -- "$payload") B，分配 $(($src_blocks * 512)) B"

step "3) 同卷跨子卷 FICLONE（cp --reflink=always 走 FICLONE ioctl）"
clone=$TARGET_SUBVOL/dst.bin
set +e
clone_err=$(cp --reflink=always -- "$payload" "$clone" 2>&1)
clone_status=$?
set -e
log "cp --reflink=always 退出码 $clone_status${clone_err:+，stderr：$clone_err}"
if [ "$clone_status" -eq 0 ]; then
	pass "同卷跨子卷 FICLONE 成功"
else
	fail_case "同卷跨子卷 FICLONE 失败：$clone_err"
fi
if cmp -s -- "$payload" "$clone"; then
	pass "克隆内容与源逐字节一致"
else
	fail_case "克隆内容与源不一致"
fi
dst_blocks=$(stat -c %b -- "$clone")
log "克隆目标 $clone：逻辑 $(stat -c %s -- "$clone") B，分配 $(($dst_blocks * 512)) B"
shared=$(filefrag -v -- "$clone" 2>/dev/null | grep -c 'shared' || true)
if [ "${shared:-0}" -ge 1 ]; then
	pass "filefrag 报告共享 extent（shared 行 $shared 条），是 CoW 克隆而非完整复制"
else
	fail_case "filefrag 未报告共享 extent，无法证明是 reflink"
fi

step "4) 同子卷内 FICLONE 同样可用"
same=$SOURCE_SUBVOL/copy.bin
if cp --reflink=always -- "$payload" "$same" 2>/dev/null; then
	pass "同子卷 FICLONE 成功"
else
	fail_case "同子卷 FICLONE 失败"
fi

step "5) 跨文件系统（/data 卷 → loop btrfs 镜像）必须失败而不是静默复制"
cross=$LOOP_MOUNT/dst.bin
set +e
cross_err=$(cp --reflink=always -- "$payload" "$cross" 2>&1)
cross_status=$?
set -e
log "跨文件系统 cp --reflink=always 退出码 $cross_status${cross_err:+，stderr：$cross_err}"
if [ "$cross_status" -ne 0 ] && [ ! -e "$cross" ]; then
	pass "跨文件系统克隆明确失败（EXDEV 类错误），未留下目标文件"
else
	fail_case "跨文件系统克隆未按预期失败（退出码 $cross_status）"
fi

step "6) 源文件被原地改写后，克隆副本内容不变（CoW 独立）"
dd if=/dev/urandom of="$payload" bs=4096 count=1 conv=notrunc status=none
if cmp -s -- "$payload" "$clone"; then
	fail_case "源文件改写后与克隆副本仍相同，CoW 隔离未生效"
else
	pass "源文件改写后克隆副本保持旧字节（CoW 隔离）"
fi

summary || die "存在失败用例"
