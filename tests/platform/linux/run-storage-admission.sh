#!/usr/bin/env bash
# S013 验收 1/2：Linux 存储准入矩阵（真实后端样本）。
#
# 覆盖：btrfs 子卷与合规 bind mount 接受；tmpfs/proc/overlay/ext4 真实样本拒绝；嵌套子卷、
# 子挂载、NOCOW 目录/文件拒绝；内部状态目录重叠/跨中立文件系统拒绝；已注册 Vault 的别名与
# 重叠拒绝；稀疏文件不是占位证据。
#
# 全部样本只在 tools/test-volume/test-volume.sh 建立的 run 目录内，结束时清理并核对。
# 运行：nix develop --command bash tests/platform/linux/run-storage-admission.sh
set -euo pipefail
PROG=${0##*/}
# shellcheck source=tests/platform/linux/common.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

require_dev_shell
require_sudo_noninteractive
require_tools mkfs.ext4 chattr truncate

run_id=${VAULTSYNCD_RUN_ID:-s013-adm-$$}
RUN_DIR=$LAB_VAULT/runs/$run_id
VOLUME=$RUN_DIR/volume
MNT=$RUN_DIR/mnt

declare -a TRACK_MOUNTS=() TRACK_SUBVOLS=() TRACK_PATHS=() TRACK_LOOPS=()
MOUNTS_BEFORE=""
LOOPS_BEFORE=""

track_mount() { TRACK_MOUNTS+=("$1"); }
track_subvol() { TRACK_SUBVOLS+=("$1"); }
track_path() { TRACK_PATHS+=("$1"); }

cleanup() {
	local status=$?
	set +e
	log ""
	step "清理本 run 的挂载、loop、子卷与样本文件"
	local reversed index
	if [ "${#TRACK_MOUNTS[@]}" -gt 0 ]; then
		for ((index = ${#TRACK_MOUNTS[@]} - 1; index >= 0; index--)); do
			umount_tolerant "${TRACK_MOUNTS[index]}"
		done
	fi
	if [ "${#TRACK_LOOPS[@]}" -gt 0 ]; then
		for ((index = ${#TRACK_LOOPS[@]} - 1; index >= 0; index--)); do
			sudo_run losetup -d -- "${TRACK_LOOPS[index]}"
		done
	fi
	if [ "${#TRACK_SUBVOLS[@]}" -gt 0 ]; then
		for ((index = ${#TRACK_SUBVOLS[@]} - 1; index >= 0; index--)); do
			local subvol=${TRACK_SUBVOLS[index]}
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
			local path=${TRACK_PATHS[index]}
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
	if [ -d "$VOLUME" ] && [ -z "$(find "$VOLUME" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
		:
	else
		log "volume 子卷残留内容（test-volume.sh cleanup 会拒绝删除）："
		find "$VOLUME" -mindepth 1 -maxdepth 1 2>/dev/null || true
	fi
	if [ -d "$RUN_DIR" ]; then
		"$TEST_VOLUME" cleanup "$run_id" || log "test-volume.sh cleanup 失败，请人工核查 $RUN_DIR"
	fi
	if [ -e "$LAB_VAULT/runs/$run_id" ]; then
		log "run 目录仍存在：$LAB_VAULT/runs/$run_id"
	else
		log "run 目录已删除：$LAB_VAULT/runs/$run_id"
	fi
	if [ "$(snapshot_loops)" != "$LOOPS_BEFORE" ]; then
		log "注意：losetup 与开始时不一致，请核对："
		diff <(printf '%s\n' "$LOOPS_BEFORE") <(snapshot_loops) || true
	else
		log "losetup -a 与开始时一致"
	fi
	if [ "$(snapshot_mounts)" != "$MOUNTS_BEFORE" ]; then
		log "注意：挂载表与开始时不一致，请核对："
		diff <(printf '%s\n' "$MOUNTS_BEFORE") <(snapshot_mounts) || true
	else
		log "findmnt 与开始时一致"
	fi
	log "cleanup status=$status"
}
trap cleanup EXIT

step "0) 记录基线并建立 run 目录（run_id=$run_id）"
MOUNTS_BEFORE=$(snapshot_mounts)
LOOPS_BEFORE=$(snapshot_loops)
"$TEST_VOLUME" create "$run_id"
VOL_SUBVOL=""
IMG_FSID=""
while IFS='=' read -r key value; do
	case $key in
	volume_subvol_id) VOL_SUBVOL=$value ;;
	image_fs_uuid) IMG_FSID=$value ;;
	esac
done <"$RUN_DIR/run.env"
[ -n "$VOL_SUBVOL" ] || die "run.env 缺少 volume_subvol_id"
[ -n "$IMG_FSID" ] || die "run.env 缺少 image_fs_uuid"
own_run_path "$VOLUME"
own_run_path "$MNT"
build_admission_bin

step "1) btrfs 子卷接受，FSID/子卷 ID 与 btrfs 工具一致"
run_admission inspect "$VOLUME"
expect_decision "专用子卷接受" accepted "" 0
expect_field "后端为 btrfs" backend btrfs
expect_field "子卷 ID 与 test-volume 记录一致" subvol_id "$VOL_SUBVOL"
expect_field "子卷 ID 来源无需特权" subvol_id_source statx_subvol
expect_field "未命中 nocow" nocow "false"
cpu_fsid=$(field fsid)
tool_fsid=$(sudo_run btrfs filesystem show -- /data |
	awk '/uuid:/ { for (i = 1; i <= NF; i++) if ($i == "uuid:") { print $(i + 1); exit } }')
if [ -n "$cpu_fsid" ] && [ "$cpu_fsid" = "$tool_fsid" ]; then
	pass "FSID 与 btrfs filesystem show 一致（$cpu_fsid）"
else
	fail_case "FSID 不一致：探针=$cpu_fsid，btrfs=$tool_fsid"
fi

step "2) 独立 btrfs loop 卷接受，且 FSID 与主卷不同"
run_admission inspect "$MNT"
expect_decision "loop 镜像根接受" accepted "" 0
expect_field "loop 卷 FSID 与运行记录一致" fsid "$IMG_FSID"
if [ "$(field fsid)" != "$cpu_fsid" ]; then
	pass "loop 卷与 /data 卷 FSID 不同（容量/身份均独立）"
else
	fail_case "loop 卷 FSID 与 /data 相同：$(field fsid)"
fi

step "3) 合规 bind mount 接受，且与源是同一存储（别名不重复计数）"
mkdir -p -- "$RUN_DIR/bind-vault"
track_path "$RUN_DIR/bind-vault"
mount_bind "$VOLUME" "$RUN_DIR/bind-vault"
track_mount "$RUN_DIR/bind-vault"
run_admission inspect "$RUN_DIR/bind-vault"
expect_decision "bind mount 接受" accepted "" 0
expect_field "bind mount 子卷 ID 与源相同" subvol_id "$VOL_SUBVOL"
expect_field "bind mount FSID 与源相同" fsid "$cpu_fsid"
bind_mount_id=$(field mount_id)
run_admission same-storage "$VOLUME" "$RUN_DIR/bind-vault"
expect_line "bind mount 与源同存储" '^same_storage=true$'
expect_line "bind mount 与源同目录" '^same_directory=true$'
expect_status "same-storage 退出码" 0
src_mount_id=$(sed -n 's/^left_mount_id=//p' <<<"$ADMISSION_OUT")
if [ -n "$src_mount_id" ] && [ "$src_mount_id" != "$bind_mount_id" ]; then
	pass "bind mount 的 mount id 与源不同（$bind_mount_id vs $src_mount_id），身份比较没有只看路径"
else
	fail_case "bind mount 与源 mount id 相同或缺失：$bind_mount_id / $src_mount_id"
fi
umount_checked "$RUN_DIR/bind-vault"

step "4) 符号链接与 .. 别名解析为同一存储"
ln -s -- "$VOLUME" "$RUN_DIR/alias-link"
track_path "$RUN_DIR/alias-link"
run_admission same-storage "$RUN_DIR/alias-link" "$VOLUME"
expect_line "符号链接别名同存储" '^same_storage=true$'
expect_status "符号链接别名退出码" 0
run_admission same-storage "$VOLUME/../volume" "$VOLUME"
expect_line ".. 别名同存储" '^same_storage=true$'
expect_status ".. 别名退出码" 0

step "5) 子卷内普通目录接受（Vault 可以是子卷内任意目录）"
mkdir -p -- "$VOLUME/plain"
track_path "$VOLUME/plain"
run_admission inspect "$VOLUME/plain"
expect_decision "子卷内普通目录接受" accepted "" 0

step "6) tmpfs 真实样本拒绝（/tmp、/dev/shm、/run）"
for sample in /tmp /dev/shm /run; do
	run_admission inspect "$sample"
	expect_decision "拒绝 $sample" rejected unsupported_backend 3
	expect_field "$sample 后端分类" backend tmpfs
	expect_line "$sample 错误码为 UNSUPPORTED_STORAGE_BACKEND" '^error_code=UNSUPPORTED_STORAGE_BACKEND$'
done

step "7) proc 真实样本拒绝"
run_admission inspect /proc
expect_decision "拒绝 /proc" rejected unsupported_backend 3
expect_field "/proc 后端分类" backend other

step "8) overlay 真实样本拒绝（run 目录内自建，不触碰主机挂载）"
mkdir -p -- "$RUN_DIR/ovl/lower" "$RUN_DIR/ovl/upper" "$RUN_DIR/ovl/work" "$RUN_DIR/ovl/mnt"
track_path "$RUN_DIR/ovl"
sudo_run mount -t overlay overlay \
	-o "lowerdir=$RUN_DIR/ovl/lower,upperdir=$RUN_DIR/ovl/upper,workdir=$RUN_DIR/ovl/work" \
	-- "$RUN_DIR/ovl/mnt"
track_mount "$RUN_DIR/ovl/mnt"
run_admission inspect "$RUN_DIR/ovl/mnt"
expect_decision "拒绝 overlay" rejected unsupported_backend 3
expect_field "overlay 后端分类" backend overlay
umount_checked "$RUN_DIR/ovl/mnt"

step "9) 小 ext4 loop 样本拒绝（run 目录内 64 MiB 稀疏镜像，不格式化任何既有设备）"
ext4_img=$RUN_DIR/ext4.img
truncate -s 64M -- "$ext4_img"
track_path "$ext4_img"
ext4_loop=$(sudo_run losetup --find --show -- "$ext4_img")
TRACK_LOOPS+=("$ext4_loop")
sudo_run mkfs.ext4 -q -F -L "vaultsyncd-s013" -- "$ext4_loop"
mkdir -p -- "$RUN_DIR/ext4-mnt"
track_path "$RUN_DIR/ext4-mnt"
sudo_run mount -t ext4 -- "$ext4_loop" "$RUN_DIR/ext4-mnt"
track_mount "$RUN_DIR/ext4-mnt"
run_admission inspect "$RUN_DIR/ext4-mnt"
expect_decision "拒绝 ext4" rejected unsupported_backend 3
expect_field "ext4 后端分类" backend ext4
umount_checked "$RUN_DIR/ext4-mnt"
sudo_run losetup -d -- "$ext4_loop"
unset 'TRACK_LOOPS[-1]'

step "10) 嵌套子卷拒绝，子卷自身仍可接受"
sudo_run btrfs subvolume create -- "$VOLUME/inner" >/dev/null
track_subvol "$VOLUME/inner"
own_run_path "$VOLUME/inner"
run_admission inspect "$VOLUME"
expect_decision "拒绝含嵌套子卷的 Vault" rejected nested_subvolume 3
expect_line "报告嵌套子卷相对路径" '^nested_subvolume=inner subvol_id=[0-9]+$'
run_admission inspect "$VOLUME/inner"
expect_decision "嵌套子卷本身作为 Vault 可接受" accepted "" 0
sudo_run btrfs subvolume delete -- "$VOLUME/inner" >/dev/null

step "11) 子挂载拒绝（Vault 内 tmpfs 挂载点）"
mkdir -p -- "$VOLUME/submount"
track_path "$VOLUME/submount"
mount_tmpfs "$VOLUME/submount"
track_mount "$VOLUME/submount"
run_admission inspect "$VOLUME"
expect_decision "拒绝含子挂载的 Vault" rejected nested_mount 3
expect_line "报告子挂载 fstype" '^submount=.* fstype=tmpfs'
umount_checked "$VOLUME/submount"

step "12) NOCOW 目录与文件拒绝（只读检查，不清理标志）"
mkdir -p -- "$VOLUME/nocow"
track_path "$VOLUME/nocow"
chattr +C -- "$VOLUME/nocow"
run_admission inspect "$VOLUME/nocow"
expect_decision "NOCOW 目录作为 Vault 拒绝" rejected nocow_inode_flags 3
expect_line "报告 NOCOW 标志位" '^inode_flags=0x00800000$'
run_admission inspect "$VOLUME"
expect_decision "扫描发现 NOCOW 子目录后拒绝 Vault" rejected nocow_inode_flags 3
expect_line "报告 NOCOW 子目录" '^nocow_directory=nocow flags=0x00800000$'
nocow_file=$VOLUME/nocow-file.bin
: >"$nocow_file"
track_path "$nocow_file"
chattr +C -- "$nocow_file"
dd if=/dev/urandom of="$nocow_file" bs=4096 count=2 status=none
run_admission content "$nocow_file"
expect_field "NOCOW 文件的 content 探针识别标志" nocow "true"
rm -rf -- "$VOLUME/nocow" "$nocow_file"

step "13) 内部状态目录重叠 / 跨文件系统拒绝，同卷另一子卷记录备注"
mkdir -p -- "$VOLUME/db"
track_path "$VOLUME/db"
run_admission inspect "$VOLUME" --internal-state-dir "$VOLUME/db"
expect_decision "内部状态目录与 Vault 重叠拒绝" rejected internal_state_overlap 3
sudo_run btrfs subvolume create -- "$RUN_DIR/state" >/dev/null
track_subvol "$RUN_DIR/state"
own_run_path "$RUN_DIR/state"
run_admission inspect "$VOLUME" --internal-state-dir "$RUN_DIR/state"
expect_decision "同文件系统另一子卷作为内部状态目录接受" accepted "" 0
expect_line "记录内部状态目录的独立子卷（reflink 仍可用）" '^note=internal_state_separate_subvolume='
mkdir -p -- "$MNT/state"
track_path "$MNT/state"
run_admission inspect "$VOLUME" --internal-state-dir "$MNT/state"
expect_decision "内部状态目录在另一文件系统拒绝" rejected internal_state_other_filesystem 3

step "14) 已注册 Vault 的别名与重叠拒绝"
mkdir -p -- "$RUN_DIR/bind-vault"
track_path "$RUN_DIR/bind-vault"
mount_bind "$VOLUME" "$RUN_DIR/bind-vault"
track_mount "$RUN_DIR/bind-vault"
run_admission inspect "$RUN_DIR/bind-vault" --registered-vault "$VOLUME"
expect_decision "同一目录的 bind 别名不能重复注册" rejected duplicate_vault 3
umount_checked "$RUN_DIR/bind-vault"
mkdir -p -- "$VOLUME/sub"
track_path "$VOLUME/sub"
run_admission inspect "$VOLUME/sub" --registered-vault "$VOLUME"
expect_decision "与已注册 Vault 路径重叠拒绝" rejected overlapping_vault 3

step "15) 普通稀疏文件不是占位证据"
sparse=$RUN_DIR/sparse.bin
truncate -s 8M -- "$sparse"
track_path "$sparse"
run_admission content "$sparse"
expect_field "8 MiB 稀疏文件形态" content_shape sparse
expect_field "稀疏不构成占位证据" placeholder_evidence none
dd if=/dev/urandom of="$sparse" bs=4096 count=1 conv=notrunc status=none
run_admission content "$sparse"
expect_field "写 4 KiB 后仍是稀疏文件" content_shape sparse
expect_field "稀疏（有数据）仍不是占位证据" placeholder_evidence none
full=$RUN_DIR/full.bin
dd if=/dev/urandom of="$full" bs=1M count=1 status=none
track_path "$full"
run_admission content "$full"
expect_field "1 MiB 随机数据文件完全分配" content_shape fully_allocated
expect_field "完全分配也不推断占位" placeholder_evidence none

step "16) 用法错误与探测错误不静默成功"
run_admission inspect
expect_status "缺少路径时退出码 2" 2
run_admission inspect /nonexistent-vaultsyncd-s013
expect_status "不存在的路径退出码 1" 1

summary || die "存在失败用例"
