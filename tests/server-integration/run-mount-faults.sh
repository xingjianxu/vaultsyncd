#!/usr/bin/env bash
# S103 挂载故障集成验收驱动（design §21.1 T19/T35）。
#
# 在 tools/test-volume/test-volume.sh 建立的**独立 btrfs loop 卷**上运行
# `tests/server-integration/tests/mount_faults.rs`：
#   - 真实 umount/mount 卷根目录（卸载后露出空目录）；
#   - 用 -o subvol=decoy 把另一个子卷挂到同一 Root 路径（卷/子卷替换）；
#   - bind 别名重复注册、Root 内子挂载。
#
# 只有本脚本建立/核对的挂载会被卸载；失败时也先清理再退出。真实用户目录不参与。
#
# 运行：nix develop --command bash tests/server-integration/run-mount-faults.sh
set -euo pipefail
set -o pipefail
PROG=${0##*/}

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
TEST_VOLUME="$repo_root/tools/test-volume/test-volume.sh"
LAB_ROOT=${VAULTSYNCD_LAB_ROOT:-/data/vaultsyncd-lab}
run_id=${VAULTSYNCD_RUN_ID:-s103-mount-$$}
RUN_DIR=$LAB_ROOT/runs/$run_id
MNT=$RUN_DIR/mnt
IMAGE=$RUN_DIR/loop.img
META=$RUN_DIR/run.env
ALIAS_BASE=$repo_root/target/s103-mount-$run_id

log() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
die() {
	printf '%s: error: %s\n' "$PROG" "$*" >&2
	exit 1
}
sudo_run() { sudo -n "$@"; }

require_tools() {
	local tool
	for tool in cargo btrfs findmnt mountpoint mount umount losetup; do
		command -v "$tool" >/dev/null 2>&1 || die "缺少 $tool；请在 nix develop 内运行"
	done
	sudo -n true 2>/dev/null || die "需要非交互 sudo -n"
}

# 列出某个目录之下的全部挂载点（含目录本身）。只按路径边界匹配，绝不包含兄弟目录。
mounts_under() {
	findmnt -n -l -o TARGET 2>/dev/null |
		awk -v prefix="$1" '$0 == prefix || index($0, prefix "/") == 1'
}

umount_run_mounts() {
	local target list round
	for round in 1 2 3 4; do
		# 由深到浅：父挂载只有在子挂载卸载后才不 EBUSY。
		list=$(mounts_under "$RUN_DIR"; mounts_under "$ALIAS_BASE")
		[ -n "$list" ] || return 0
		while read -r target; do
			[ -n "$target" ] || continue
			case $target in
			"$RUN_DIR"/* | "$ALIAS_BASE"/*) ;;
			*)
				log "跳过非本 run 挂载：$target"
				continue
				;;
			esac
			mountpoint -q "$target" || continue
			log "   卸载本 run 挂载 $target"
			sudo_run umount -- "$target" ||
				sudo_run umount -l -- "$target" ||
				log "卸载失败（保留现场）：$target"
		done <<<"$(printf '%s\n' "$list" | tac)"
	done
	list=$(mounts_under "$RUN_DIR"; mounts_under "$ALIAS_BASE")
	[ -z "$list" ] || {
		log "仍有本 run 挂载未卸载：$list"
		return 1
	}
}

cleanup() {
	local status=$?
	set +e
	log ""
	step "清理本 run 的挂载、loop 与卷（run_id=$run_id）"
	umount_run_mounts || log "卸载本 run 挂载失败，保留现场"
	if [ -d "$ALIAS_BASE" ]; then
		if mounts_under "$ALIAS_BASE" | grep -q .; then
			log "别名基目录仍有挂载，保留现场：$ALIAS_BASE"
		else
			rm -rf -- "$ALIAS_BASE"
			log "   已删除别名基目录 $ALIAS_BASE"
		fi
	fi
	if [ -d "$RUN_DIR" ]; then
		"$TEST_VOLUME" cleanup "$run_id" || log "test-volume cleanup 失败，请人工核查 $RUN_DIR"
	fi
	if [ -e "$RUN_DIR" ]; then
		log "run 目录仍存在：$RUN_DIR"
	else
		log "run 目录已删除：$RUN_DIR"
	fi
	log "cleanup status=$status"
	exit "$status"
}
trap cleanup EXIT

require_tools
step "0) 测试卷基线"
"$TEST_VOLUME" status
[ -e "$RUN_DIR" ] && die "run 目录已存在（$RUN_DIR），换一个 VAULTSYNCD_RUN_ID"

step "1) 建立独立 btrfs loop 卷（run_id=$run_id）"
"$TEST_VOLUME" create "$run_id" 512M
[ -f "$META" ] || die "缺少 $META"
LOOP=""
while IFS='=' read -r key value; do
	case $key in
	loop_device) LOOP=$value ;;
	esac
done <"$META"
[ -n "$LOOP" ] || die "run.env 缺少 loop_device"
mountpoint -q "$MNT" || die "镜像未挂载：$MNT"

step "2) 准备 Root/替换子卷与卸载后的空目录"
# Root 是 <mnt>/rootmnt：它是一次真实子卷挂载；卸载后该路径露出主文件系统上的空目录（T19）。
# 内部状态也必须在同一 btrfs 文件系统（准入要求），由用例在 <mnt> 下建立。
sudo_run btrfs subvolume create "$MNT/root" >/dev/null
sudo_run btrfs subvolume create "$MNT/decoy" >/dev/null
sudo_run chown -R "$(id -u):$(id -g)" "$MNT"
printf 'the original subvolume content\n' >"$MNT/root/note.txt"
printf 'another subvolume mounted at the same path\n' >"$MNT/decoy/note.txt"
mkdir -p "$MNT/rootmnt"
sudo_run mount -o subvol=root -- "$LOOP" "$MNT/rootmnt"
mountpoint -q "$MNT/rootmnt" || die "Root 子卷挂载失败：$MNT/rootmnt"
[ -f "$MNT/rootmnt/note.txt" ] || die "Root 子卷内容不可见"
mkdir -p "$ALIAS_BASE"
ls -la "$MNT" "$MNT/rootmnt"

step "3) 运行真实挂载故障用例（server-integration mount_faults）"
status=0
(
	cd "$repo_root"
	VAULTSYNCD_S103_MOUNT="$MNT" \
		VAULTSYNCD_S103_ROOT="$MNT/rootmnt" \
		VAULTSYNCD_S103_ROOT_SUBVOL=root \
		VAULTSYNCD_S103_LOOP="$LOOP" \
		VAULTSYNCD_S103_DECOY_SUBVOL=decoy \
		VAULTSYNCD_S103_ALIAS_BASE="$ALIAS_BASE" \
		cargo test --locked -p vaultsyncd-server-tests --test mount_faults \
		-- --test-threads=1
) || status=$?

step "4) 用例后核对"
# 用例正常结束时卷应已挂回；无论成败都先卸载，再交给 test-volume cleanup。
umount_run_mounts
exit "$status"
