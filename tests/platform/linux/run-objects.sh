#!/usr/bin/env bash
# S033 验收：不可变内容对象的写入与读取（design §11.1—11.2、§11.5、I12、C37）。
#
# 验收点：
#   1. 同卷（btrfs）采集得到 reflink 副本，发布到内容地址；对象 inode 与工作文件不同。
#   2. 改写、截断、删除工作文件后，已保存对象仍按原内容地址读出原字节（CoW 独立）。
#   3. 错误摘要、错误长度（短写/截断）不产生对象，也不留下未发布暂存副本。
#   4. 跨文件系统（对象区在 tmpfs）退回完整复制并暴露 exdev 原因，结果仍独立于工作文件。
#   5. 对象区满盘（小 tmpfs）报 no_space，不产生可提交对象。
#   6. 读取不存在的内容地址明确失败，不返回空内容。
#
# 运行：nix develop --command bash tests/platform/linux/run-objects.sh
set -euo pipefail
PROG=${0##*/}
# shellcheck source=tests/platform/linux/common.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

require_dev_shell
require_tools cmp dd sha256sum stat truncate

run_id=${VAULTSYNCD_RUN_ID:-s033-objects-$$}
RUN_DIR=$repo_root/artifacts/s033/$run_id
WORK_DIR=$RUN_DIR/work
OBJ_DIR=$RUN_DIR/objects
CROSS_DIR=/tmp/vaultsyncd-$run_id-objects
ENOSPC_MOUNT=$RUN_DIR/objects-enospc
MOUNTED_ENOSPC=0

cleanup() {
	local status=$?
	set +e
	log ""
	step "清理 S033 样本"
	if [ "$MOUNTED_ENOSPC" = 1 ]; then
		sudo_run umount -- "$ENOSPC_MOUNT" || log "卸载失败（需人工核查）：$ENOSPC_MOUNT"
	fi
	rm -rf -- "$RUN_DIR" "$CROSS_DIR"
	for path in "$RUN_DIR" "$CROSS_DIR"; do
		if [ -e "$path" ]; then
			log "样本目录仍存在：$path"
		else
			log "样本目录已删除：$path"
		fi
	done
	log "cleanup status=$status"
}
trap cleanup EXIT

digest_of() { sha256sum -- "$1" | cut -d' ' -f1; }
size_of() { stat -c %s -- "$1"; }
inode_of() { stat -c %i -- "$1"; }

build_object_capture_bin
mkdir -p "$WORK_DIR" "$OBJ_DIR"

step "1) 准备 4 MiB 工作文件并保存一份普通副本用于逐字节比对"
SRC=$WORK_DIR/file.bin
dd if=/dev/urandom of="$SRC" bs=1M count=4 status=none
SRC_DIGEST=$(digest_of "$SRC")
SRC_SIZE=$(size_of "$SRC")
cp -- "$SRC" "$RUN_DIR/pristine.bin"

step "2) 同卷（btrfs）采集：FICLONE 副本、内容地址发布"
run_object_capture capture --objects "$OBJ_DIR" --source "$SRC" --staging cas-1 \
	--digest "$SRC_DIGEST" --bytes "$SRC_SIZE"
expect_decision "同卷采集成功" captured "" 0
expect_field "采集方法为 reflink" method reflink
expect_field "同卷不需要退回复制" fallback none
expect_field "首次发布" reuse freshly_published
expect_field "暂存已由发布消费" staging_cleanup not_needed
STORAGE_REL=$(field storage_path)
OBJ=$OBJ_DIR/$STORAGE_REL
[ "$STORAGE_REL" = "objects/${SRC_DIGEST:0:2}/$SRC_DIGEST" ] &&
	pass "对象落在内容地址 $STORAGE_REL" ||
	fail_case "对象路径不符合内容地址布局：$STORAGE_REL"
[ -f "$OBJ" ] && pass "对象文件已发布" || fail_case "对象文件不存在：$OBJ"
if cmp -s -- "$OBJ" "$RUN_DIR/pristine.bin"; then
	pass "对象字节与原文件逐字节一致"
else
	fail_case "对象字节与原文件不一致"
fi
[ "$(inode_of "$OBJ")" != "$(inode_of "$SRC")" ] &&
	pass "对象 inode 与工作文件 inode 不同（$(inode_of "$OBJ") vs $(inode_of "$SRC")）" ||
	fail_case "对象与工作文件共用 inode"
MODE=$(stat -c %a -- "$OBJ")
[ "$MODE" = 444 ] && pass "对象权限为 0444（不向外部写入者开放）" || fail_case "对象权限为 $MODE"
[ -z "$(ls -A "$OBJ_DIR/staging" 2>/dev/null)" ] &&
	pass "staging 目录没有未发布残留" || fail_case "staging 目录有残留：$(ls -A "$OBJ_DIR/staging")"

step "3) 改写工作文件后对象不变"
dd if=/dev/urandom of="$SRC" bs=1M count=4 conv=notrunc status=none
[ "$(digest_of "$SRC")" != "$SRC_DIGEST" ] && pass "工作文件内容已改写" ||
	fail_case "工作文件改写未生效"
run_object_capture read --objects "$OBJ_DIR" --digest "$SRC_DIGEST" --bytes "$SRC_SIZE"
expect_decision "改写后按内容地址读取成功" read "" 0
expect_field "读出摘要仍是原内容" observed "$SRC_DIGEST"
cmp -s -- "$OBJ" "$RUN_DIR/pristine.bin" && pass "对象字节未被工作文件改写影响" ||
	fail_case "对象字节被改写"

step "4) 截断与删除工作文件后对象不变"
truncate -s 0 -- "$SRC"
[ "$(size_of "$SRC")" = 0 ] && pass "工作文件已截断为 0 字节" || fail_case "截断失败"
run_object_capture read --objects "$OBJ_DIR" --digest "$SRC_DIGEST" --bytes "$SRC_SIZE"
expect_decision "截断后读取成功" read "" 0
expect_field "截断后读出摘要不变" observed "$SRC_DIGEST"
rm -f -- "$SRC"
[ ! -e "$SRC" ] && pass "工作文件已删除" || fail_case "删除工作文件失败"
run_object_capture read --objects "$OBJ_DIR" --digest "$SRC_DIGEST" --bytes "$SRC_SIZE"
expect_decision "删除后读取成功" read "" 0
expect_field "删除后读出摘要不变" observed "$SRC_DIGEST"
cmp -s -- "$OBJ" "$RUN_DIR/pristine.bin" && pass "对象字节仍与原文件一致" ||
	fail_case "对象字节已改变"

step "5) 错误摘要与错误长度不产生对象"
OTHER=$WORK_DIR/other.bin
dd if=/dev/urandom of="$OTHER" bs=4096 count=1 status=none
OTHER_DIGEST=$(digest_of "$OTHER")
OTHER_SIZE=$(size_of "$OTHER")
WRONG_DIGEST=0000000000000000000000000000000000000000000000000000000000000000
run_object_capture capture --objects "$OBJ_DIR" --source "$OTHER" --staging cas-bad \
	--digest "$WRONG_DIGEST" --bytes "$OTHER_SIZE"
expect_decision "错误摘要被拒绝" rejected "" 1
expect_field "错误摘要错误码" code digest_mismatch
[ ! -e "$OBJ_DIR/objects/${WRONG_DIGEST:0:2}/$WRONG_DIGEST" ] &&
	pass "错误摘要没有产生对象" || fail_case "错误摘要产生了对象"
[ -z "$(ls -A "$OBJ_DIR/staging" 2>/dev/null)" ] &&
	pass "错误摘要没有留下暂存副本" || fail_case "错误摘要留下暂存副本"
run_object_capture capture --objects "$OBJ_DIR" --source "$OTHER" --staging cas-short \
	--digest "$OTHER_DIGEST" --bytes "$((OTHER_SIZE + 4096))"
expect_decision "长度不符被拒绝" rejected "" 1
expect_field "长度不符错误码" code length_mismatch
[ ! -e "$OBJ_DIR/objects/${OTHER_DIGEST:0:2}/$OTHER_DIGEST" ] &&
	pass "长度不符没有产生对象" || fail_case "长度不符产生了对象"

step "6) 跨文件系统：对象区在 tmpfs，退回校验复制并暴露 exdev"
mkdir -p "$CROSS_DIR"
run_object_capture capture --objects "$CROSS_DIR" --source "$OTHER" --staging cas-cross \
	--digest "$OTHER_DIGEST" --bytes "$OTHER_SIZE"
expect_decision "跨卷采集成功" captured "" 0
expect_field "跨卷走校验复制" method verified_copy
expect_field "跨卷原因如实暴露" fallback exdev
CROSS_OBJ=$CROSS_DIR/objects/${OTHER_DIGEST:0:2}/$OTHER_DIGEST
cmp -s -- "$CROSS_OBJ" "$OTHER" && pass "跨卷对象内容一致" || fail_case "跨卷对象内容不一致"
dd if=/dev/urandom of="$OTHER" bs=4096 count=1 conv=notrunc status=none
run_object_capture read --objects "$CROSS_DIR" --digest "$OTHER_DIGEST" --bytes "$OTHER_SIZE"
expect_decision "改写源后跨卷对象仍可读" read "" 0
expect_field "跨卷对象摘要不变" observed "$OTHER_DIGEST"

step "7) 对象区满盘（ENOSPC）不产生可提交对象"
if sudo -n true 2>/dev/null; then
	mkdir -p "$ENOSPC_MOUNT"
	sudo_run mount -t tmpfs -o size=16M tmpfs -- "$ENOSPC_MOUNT"
	MOUNTED_ENOSPC=1
	own_run_path "$ENOSPC_MOUNT"
	dd if=/dev/zero of="$ENOSPC_MOUNT/filler.bin" bs=1M count=12 status=none
	BIG=$WORK_DIR/big.bin
	dd if=/dev/urandom of="$BIG" bs=1M count=8 status=none
	BIG_DIGEST=$(digest_of "$BIG")
	BIG_SIZE=$(size_of "$BIG")
	run_object_capture capture --objects "$ENOSPC_MOUNT" --source "$BIG" --staging cas-enospc \
		--digest "$BIG_DIGEST" --bytes "$BIG_SIZE"
	expect_decision "满盘采集被拒绝" rejected "" 1
expect_field "满盘错误码" code no_space
	[ ! -e "$ENOSPC_MOUNT/objects/${BIG_DIGEST:0:2}/$BIG_DIGEST" ] &&
		pass "满盘没有产生对象" || fail_case "满盘产生了对象"
	[ -z "$(ls -A "$ENOSPC_MOUNT/staging" 2>/dev/null)" ] &&
		pass "满盘没有留下暂存副本" || fail_case "满盘留下暂存副本"
	sudo_run umount -- "$ENOSPC_MOUNT"
	MOUNTED_ENOSPC=0
else
	log "跳过 ENOSPC 真实样本：当前环境没有非交互 sudo（核心假端口用例已覆盖该分类）"
fi

step "8) 不存在的内容地址明确失败"
run_object_capture read --objects "$OBJ_DIR" --digest "$WRONG_DIGEST" --bytes 16
expect_decision "缺失对象被拒绝" rejected "" 1
expect_field "缺失对象错误码" code object_missing

summary || die "存在失败用例"
