#!/usr/bin/env bash
# S014 验收：受限目录句柄读写、目标文件系统内的暂存、FICLONE 与跨文件系统校验复制、受控替换
# 与“外部竞争时暂停并报告”。
#
# 覆盖的验收点（实施手册 S014）：
#   1) 工作文件被外部改写后，内容对象的旧字节与 SHA-256 不变（CoW 独立）；
#   2) EXDEV 不被忽略：跨文件系统采集/发布走校验复制并暴露原因，跨设备 rename 明确失败；
#   3) 外部竞争导致前提不成立时暂停、保留候选、不覆盖目标（失败路径，不吞错）。
#
# 运行：nix develop --command bash tests/platform/linux/run-file-publish.sh
set -euo pipefail
PROG=${0##*/}
# shellcheck source=tests/platform/linux/common.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

require_dev_shell
require_sudo_noninteractive
require_tools filefrag sha256sum cmp dd

run_id=${VAULTSYNCD_RUN_ID:-s014-publish-$$}
RUN_DIR=$LAB_VAULT/runs/$run_id
WORK_SUBVOL=$RUN_DIR/work
STATE_SUBVOL=$RUN_DIR/state
CROSS_MOUNT=$RUN_DIR/mnt

declare -a TRACK_SUBVOLS=() TRACK_PATHS=()
MOUNTS_BEFORE=""
LOOPS_BEFORE=""

track_subvol() { TRACK_SUBVOLS+=("$1"); }
track_path() { TRACK_PATHS+=("$1"); }

cleanup() {
	local status=$?
	set +e
	log ""
	step "清理 S014 样本子卷与临时文件"
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
	# run 目录里剩下的本 run 临时文件（身份规格、外部写入样本等）一并清掉，便于
	# test-volume.sh cleanup 的条目白名单核对。
	if [ -d "$RUN_DIR" ]; then
		rm -rf -- "$RUN_DIR"/obj*.before "$RUN_DIR"/*.identity* "$RUN_DIR"/external*.bin 2>/dev/null
		rm -rf -- "$CROSS_MOUNT/objects" 2>/dev/null
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

sha_of() { sha256sum -- "$1" | awk '{print $1}'; }

step "0) 建立 run 目录（run_id=$run_id）"
MOUNTS_BEFORE=$(snapshot_mounts)
LOOPS_BEFORE=$(snapshot_loops)
"$TEST_VOLUME" create "$run_id"
own_run_path "$RUN_DIR/volume"
own_run_path "$CROSS_MOUNT"

step "1) 在 /data 卷上建立 work 与 state 两个兄弟子卷（同一文件系统，不同子卷）"
for subvol in "$WORK_SUBVOL" "$STATE_SUBVOL"; do
	sudo_run btrfs subvolume create -- "$subvol" >/dev/null
	track_subvol "$subvol"
	own_run_path "$subvol"
done
work_subvol_id=$(sudo_run btrfs subvolume show -- "$WORK_SUBVOL" |
	awk -F: '/Subvolume ID/ { gsub(/[ \t]/, "", $2); print $2; exit }')
state_subvol_id=$(sudo_run btrfs subvolume show -- "$STATE_SUBVOL" |
	awk -F: '/Subvolume ID/ { gsub(/[ \t]/, "", $2); print $2; exit }')
if [ -n "$work_subvol_id" ] && [ -n "$state_subvol_id" ]; then
	pass "work/state 子卷 ID 已取得（$work_subvol_id / $state_subvol_id）"
else
	fail_case "无法读取 work/state 子卷 ID"
fi
if [ "$work_subvol_id" != "$state_subvol_id" ]; then
	pass "work 与 state 是不同子卷（跨子卷 reflink 场景）"
else
	fail_case "work 与 state 的子卷 ID 相同：$work_subvol_id"
fi

build_file_publish_bin

step "2) 受限访问：解析机制、身份与哈希"
payload=$WORK_SUBVOL/doc.bin
dd if=/dev/urandom of="$payload" bs=1M count=4 status=none
sync -- "$payload"
src_sha=$(sha_of "$payload")
log "工作文件 $payload：$(stat -c %s -- "$payload") B，sha256=$src_sha"

run_file_publish identity "$WORK_SUBVOL" doc.bin
expect_status "identity 退出码" 0
expect_field "identity 使用 openat2" resolve openat2
expect_field "identity 逻辑大小" size 4194304
openat2_inode=$(field inode)

run_file_publish --resolve walk identity "$WORK_SUBVOL" doc.bin
expect_status "逐段打开 identity 退出码" 0
expect_field "逐段打开机制" resolve component_walk
if [ "$(field inode)" = "$openat2_inode" ]; then
	pass "openat2 与逐段打开得到同一 inode（$openat2_inode）"
else
	fail_case "两种机制得到不同 inode：$openat2_inode vs $(field inode)"
fi

run_file_publish hash "$WORK_SUBVOL" doc.bin
expect_status "hash 退出码" 0
expect_field "hash 与 sha256sum 一致" sha256 "$src_sha"
expect_field "hash 字节数" bytes 4194304

step "3) 同卷跨子卷采集：FICLONE 到 state 子卷的对象"
run_file_publish capture \
	--source-vault "$WORK_SUBVOL" --source doc.bin \
	--object-vault "$STATE_SUBVOL" --object obj1.bin
expect_status "capture 退出码" 0
expect_decision "capture 发布成功" published
expect_field "capture 走 FICLONE" method ficlone
expect_field "capture 未退避" fallback none
expect_field "capture 对象哈希与工作文件一致" sha256 "$src_sha"
expect_field "capture 对象权限只读" mode 0444
expect_field "capture 是新建" replaced false
expect_field "capture 目录已刷盘" directory_fsynced true
expect_field "capture 未保留无用候选" candidate none
if [ "$(field staging_mount_id)" = "$(field target_mount_id)" ]; then
	pass "暂存文件与目标在同一挂载（staging_mount_id=$(field staging_mount_id)）"
else
	fail_case "暂存文件与目标挂载不同：$(field staging_mount_id) vs $(field target_mount_id)"
fi
if [ "$(stat -c %a -- "$STATE_SUBVOL/obj1.bin")" = "444" ]; then
	pass "对象文件权限实测为 444（外部写入者不可写）"
else
	fail_case "对象文件权限实测为 $(stat -c %a -- "$STATE_SUBVOL/obj1.bin")，期望 444"
fi
shared=$(filefrag -v -- "$STATE_SUBVOL/obj1.bin" 2>/dev/null | grep -c 'shared' || true)
if [ "${shared:-0}" -ge 1 ]; then
	pass "filefrag 报告共享 extent（$shared 行）：对象是 CoW 克隆"
else
	fail_case "filefrag 未报告共享 extent，无法证明对象是 reflink"
fi
if (echo x >>"$STATE_SUBVOL/obj1.bin") 2>/dev/null; then
	fail_case "普通写入仍能追加到对象文件，只读保护未生效"
else
	pass "以当前用户追加写入对象文件被拒绝（0444）"
fi

step "4) 外部改写工作文件后，对象旧字节与 SHA-256 不变"
cp -- "$STATE_SUBVOL/obj1.bin" "$RUN_DIR/obj1.before"
dd if=/dev/urandom of="$payload" bs=1M count=4 conv=notrunc status=none
sync -- "$payload"
work_sha_now=$(sha_of "$payload")
if [ "$work_sha_now" != "$src_sha" ]; then
	pass "工作文件确实被外部改写（sha256 变化）"
else
	fail_case "工作文件改写后哈希未变化，样本无效"
fi
run_file_publish hash "$STATE_SUBVOL" obj1.bin
expect_status "对象 hash 退出码" 0
expect_field "对象哈希仍是改写前的值" sha256 "$src_sha"
if [ "$(sha_of "$STATE_SUBVOL/obj1.bin")" = "$src_sha" ]; then
	pass "对象文件 sha256sum 未随工作文件写入改变"
else
	fail_case "对象文件哈希已变化，CoW 独立性不成立"
fi
if cmp -s -- "$RUN_DIR/obj1.before" "$STATE_SUBVOL/obj1.bin"; then
	pass "对象字节与改写前保存的副本逐字节一致"
else
	fail_case "对象字节与改写前副本不同"
fi

step "5) 从对象发布新工作文件（--expect absent）"
mkdir -p -- "$WORK_SUBVOL/issued"
run_file_publish publish \
	--source-vault "$STATE_SUBVOL" --source obj1.bin \
	--target-vault "$WORK_SUBVOL" --target-dir issued --target-name restored.bin \
	--expect absent
expect_status "首次发布退出码" 0
expect_decision "首次发布成功" published
expect_field "首次发布走 FICLONE" method ficlone
expect_field "首次发布为新建" replaced false
expect_field "首次发布前目标不存在" target_inode_before none
expect_field "首次发布内容哈希" sha256 "$src_sha"
expect_field "首次发布权限" mode 0644
if [ "$(sha_of "$WORK_SUBVOL/issued/restored.bin")" = "$src_sha" ]; then
	pass "发布后的工作文件内容与对象一致"
else
	fail_case "发布后的工作文件内容与对象不一致"
fi

step "6) 按身份前提原子替换已存在的工作文件"
run_file_publish identity "$WORK_SUBVOL" issued/restored.bin
expect_status "读取目标身份退出码" 0
printf '%s\n' "$CMD_OUT" >"$RUN_DIR/restored.identity"
target_inode_before=$(field inode)

run_file_publish capture \
	--source-vault "$WORK_SUBVOL" --source doc.bin \
	--object-vault "$STATE_SUBVOL" --object obj2.bin
expect_status "第二个对象采集退出码" 0
expect_field "第二个对象内容为改写后的工作文件" sha256 "$work_sha_now"

run_file_publish publish \
	--source-vault "$STATE_SUBVOL" --source obj2.bin \
	--target-vault "$WORK_SUBVOL" --target-dir issued --target-name restored.bin \
	--expect-from "$RUN_DIR/restored.identity"
expect_status "替换发布退出码" 0
expect_decision "替换发布成功" published
expect_field "替换发布为替换" replaced true
expect_field "替换前目标身份与新内容一致" target_inode_before "$target_inode_before"
if [ "$(field target_inode_after)" != "$target_inode_before" ]; then
	pass "替换后目标换成暂存文件的 inode（$(field target_inode_after) vs $target_inode_before）"
else
	fail_case "替换后 inode 未变化，无法证明是原子替换而非原地写入"
fi
if [ "$(sha_of "$WORK_SUBVOL/issued/restored.bin")" = "$work_sha_now" ]; then
	pass "替换后的工作文件内容为新对象内容"
else
	fail_case "替换后的工作文件内容不是新对象"
fi

step "7a) 外部竞争（暂存前）：前提不成立时直接暂停，不创建暂存"
run_file_publish identity "$WORK_SUBVOL" issued/restored.bin
printf '%s\n' "$CMD_OUT" >"$RUN_DIR/restored.identity2"
head -c 12345 /dev/urandom >"$RUN_DIR/external.bin"
sync -- "$RUN_DIR/external.bin"
mv -f -- "$RUN_DIR/external.bin" "$WORK_SUBVOL/issued/restored.bin"
external_sha=$(sha_of "$WORK_SUBVOL/issued/restored.bin")

run_file_publish publish \
	--source-vault "$STATE_SUBVOL" --source obj1.bin \
	--target-vault "$WORK_SUBVOL" --target-dir issued --target-name restored.bin \
	--expect-from "$RUN_DIR/restored.identity2"
expect_status "外部竞争（暂存前）：退出码 3（暂停）" 3
expect_decision "外部竞争（暂存前）：暂停并报告" paused target_changed
expect_line "外部竞争（暂存前）：列出变化字段（含 inode）" '^changed_fields=[a-z,]*inode'
expect_field "外部竞争（暂存前）：未创建暂存所以没有候选" candidate none
if [ "$(sha_of "$WORK_SUBVOL/issued/restored.bin")" = "$external_sha" ]; then
	pass "外部竞争（暂存前）：目标仍是外部写入的内容，未被覆盖"
else
	fail_case "外部竞争（暂存前）：目标被改写，暂停逻辑失效"
fi

step "7b) 外部竞争（暂存后、替换前）：暂停并保留候选"
run_file_publish identity "$WORK_SUBVOL" issued/restored.bin
expect_status "读取当前目标身份退出码" 0
printf '%s\n' "$CMD_OUT" >"$RUN_DIR/restored.identity3"
head -c 54321 /dev/urandom >"$RUN_DIR/external2.bin"
sync -- "$RUN_DIR/external2.bin"
hook="mv -f -- '$RUN_DIR/external2.bin' '$WORK_SUBVOL/issued/restored.bin'"
run_file_publish publish \
	--source-vault "$STATE_SUBVOL" --source obj1.bin \
	--target-vault "$WORK_SUBVOL" --target-dir issued --target-name restored.bin \
	--expect-from "$RUN_DIR/restored.identity3" --race-hook "$hook"
expect_status "外部竞争（暂存后）：退出码 3（暂停）" 3
expect_decision "外部竞争（暂存后）：暂停并报告" paused target_changed
expect_field "外部竞争（暂存后）：注入的外部写入已执行" race_hook_exit 0
expect_line "外部竞争（暂存后）：列出变化字段（含 inode）" '^changed_fields=[a-z,]*inode'
candidate=$(field candidate)
if [ -n "$candidate" ] && [ "$candidate" != "none" ]; then
	pass "外部竞争（暂存后）：保留候选 $candidate"
else
	fail_case "外部竞争（暂存后）：没有保留候选副本"
fi
expect_field "外部竞争（暂存后）：候选内容哈希" candidate_sha256 "$src_sha"
if [ -f "$WORK_SUBVOL/issued/$candidate" ]; then
	pass "外部竞争（暂存后）：候选文件确实存在于目标目录"
else
	fail_case "外部竞争（暂存后）：候选文件不存在：$WORK_SUBVOL/issued/$candidate"
fi
external2_sha=$(sha_of "$WORK_SUBVOL/issued/restored.bin")
if [ "$external2_sha" != "$src_sha" ]; then
	pass "外部竞争（暂存后）：目标仍是注入写入的内容，未被服务覆盖"
else
	fail_case "外部竞争（暂存后）：目标被服务覆盖成源内容"
fi
if [ "$(stat -c %s -- "$WORK_SUBVOL/issued/restored.bin")" = "54321" ]; then
	pass "外部竞争（暂存后）：目标大小与注入内容一致（54321 字节）"
else
	fail_case "外部竞争（暂存后）：目标大小为 $(stat -c %s -- "$WORK_SUBVOL/issued/restored.bin")，期望 54321"
fi
if [ "$(sha_of "$WORK_SUBVOL/issued/$candidate")" = "$src_sha" ]; then
	pass "外部竞争（暂存后）：候选保留可恢复内容（哈希与源对象一致）"
else
	fail_case "外部竞争（暂存后）：候选内容与源对象不一致"
fi

step "8) --expect absent 遇到已存在目标：拒绝而不是覆盖"
pre_expect_absent_sha=$(sha_of "$WORK_SUBVOL/issued/restored.bin")
run_file_publish publish \
	--source-vault "$STATE_SUBVOL" --source obj1.bin \
	--target-vault "$WORK_SUBVOL" --target-dir issued --target-name restored.bin \
	--expect absent
expect_status "目标已存在：退出码 3" 3
expect_decision "目标已存在：拒绝创建" paused target_appeared
if [ "$(sha_of "$WORK_SUBVOL/issued/restored.bin")" = "$pre_expect_absent_sha" ]; then
	pass "目标已存在：原有内容未被替换"
else
	fail_case "目标已存在：原有内容被覆盖"
fi

step "9) 跨文件系统采集（loop btrfs 镜像）：EXDEV 走校验复制"
mkdir -p -- "$CROSS_MOUNT/objects"
run_file_publish capture \
	--source-vault "$WORK_SUBVOL" --source doc.bin \
	--object-vault "$CROSS_MOUNT" --object objects/obj-cross.bin
expect_status "跨文件系统采集退出码" 0
expect_decision "跨文件系统采集成功" published
expect_field "跨文件系统采集走校验复制" method verified_copy
expect_field "跨文件系统采集暴露 EXDEV" fallback exdev
if grep -qx 'reflink_error=none' <<<"$CMD_OUT"; then
	fail_case "跨文件系统采集没有保留 FICLONE 的原始错误"
else
	pass "跨文件系统采集保留 FICLONE 原始错误（$(field reflink_error)）"
fi
expect_field "跨文件系统对象内容哈希" sha256 "$work_sha_now"
if [ "$(field source_mount_id)" != "$(field target_mount_id)" ]; then
	pass "源与目标挂载 ID 不同（$(field source_mount_id) vs $(field target_mount_id)），确实是跨文件系统"
else
	fail_case "源与目标挂载 ID 相同，跨文件系统样本无效"
fi
if [ "$(field staging_mount_id)" = "$(field target_mount_id)" ]; then
	pass "跨文件系统时暂存仍在目标文件系统内（staging_mount_id=$(field staging_mount_id)）"
else
	fail_case "跨文件系统时暂存不在目标文件系统内：$(field staging_mount_id) vs $(field target_mount_id)"
fi
if [ "$(sha_of "$CROSS_MOUNT/objects/obj-cross.bin")" = "$work_sha_now" ]; then
	pass "校验复制落盘的字节与源一致"
else
	fail_case "校验复制落盘的字节与源不一致"
fi
cross_shared=$(filefrag -v -- "$CROSS_MOUNT/objects/obj-cross.bin" 2>/dev/null | grep -c 'shared' || true)
if [ "${cross_shared:-0}" -eq 0 ]; then
	pass "跨文件系统对象没有共享 extent（是完整复制而非伪造 reflink）"
else
	fail_case "跨文件系统对象报告了共享 extent（$cross_shared 行）"
fi

step "10) 跨文件系统发布（对象在 loop 卷 → 工作文件在 /data）"
run_file_publish publish \
	--source-vault "$CROSS_MOUNT" --source objects/obj-cross.bin \
	--target-vault "$WORK_SUBVOL" --target-dir issued --target-name cross.bin \
	--expect absent
expect_status "跨文件系统发布退出码" 0
expect_decision "跨文件系统发布成功" published
expect_field "跨文件系统发布走校验复制" method verified_copy
expect_field "跨文件系统发布暴露 EXDEV" fallback exdev
expect_field "跨文件系统发布内容哈希" sha256 "$work_sha_now"
if [ "$(sha_of "$WORK_SUBVOL/issued/cross.bin")" = "$work_sha_now" ]; then
	pass "跨文件系统发布后的工作文件内容正确"
else
	fail_case "跨文件系统发布后的工作文件内容不正确"
fi

step "11) 跨设备 rename 必须明确失败（不得在源文件系统里暂存再跨设备 rename）"
run_file_publish rename \
	--from-vault "$STATE_SUBVOL" --from obj1.bin \
	--to-vault "$CROSS_MOUNT" --to moved.bin
expect_status "跨设备 rename 退出码 3" 3
expect_decision "跨设备 rename 被拒绝" paused cross_device_rename
if [ ! -e "$CROSS_MOUNT/moved.bin" ]; then
	pass "跨设备 rename 未在目标留下文件"
else
	fail_case "跨设备 rename 留下了目标文件：$CROSS_MOUNT/moved.bin"
fi
if [ -f "$STATE_SUBVOL/obj1.bin" ]; then
	pass "跨设备 rename 后源文件仍在原文件系统"
else
	fail_case "跨设备 rename 后源文件消失"
fi

step "12) 受限访问 fail-closed：符号链接与逃逸路径"
ln -s /etc/passwd "$WORK_SUBVOL/escape.link"
run_file_publish hash "$WORK_SUBVOL" escape.link
expect_status "openat2 遇符号链接退出码 3" 3
expect_decision "openat2 遇符号链接拒绝" paused symlink_rejected

run_file_publish --resolve walk hash "$WORK_SUBVOL" escape.link
expect_status "逐段打开遇符号链接退出码 3" 3
expect_decision "逐段打开遇符号链接拒绝" paused symlink_rejected

ln -s "$STATE_SUBVOL" "$WORK_SUBVOL/state-link"
run_file_publish hash "$WORK_SUBVOL" state-link/obj1.bin
expect_status "中间段符号链接（openat2）退出码 3" 3
expect_decision "中间段符号链接（openat2）拒绝" paused symlink_rejected

run_file_publish --resolve walk hash "$WORK_SUBVOL" state-link/obj1.bin
expect_status "中间段符号链接（逐段打开）退出码 3" 3
expect_decision "中间段符号链接（逐段打开）拒绝" paused

run_file_publish hash "$WORK_SUBVOL" ../escape
expect_status ".. 逃逸退出码 3" 3
expect_decision ".. 逃逸拒绝" paused invalid_relative_path

run_file_publish hash "$WORK_SUBVOL" /etc/passwd
expect_status "绝对路径退出码 3" 3
expect_decision "绝对路径拒绝" paused invalid_relative_path

step "13) 已存在的对象不被覆盖（内容对象不可变）"
run_file_publish capture \
	--source-vault "$WORK_SUBVOL" --source doc.bin \
	--object-vault "$STATE_SUBVOL" --object obj1.bin
expect_status "重复采集同一对象退出码 3" 3
expect_decision "重复采集同一对象被拒绝" paused target_appeared
if [ "$(sha_of "$STATE_SUBVOL/obj1.bin")" = "$src_sha" ]; then
	pass "已存在对象的内容保持原样（仍是第一次采集的字节）"
else
	fail_case "已存在对象的内容被改写"
fi

summary || die "存在失败用例"
