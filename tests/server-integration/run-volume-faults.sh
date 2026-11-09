#!/usr/bin/env bash
# S103 容量故障集成验收驱动（design §21.1 T21、§11.5）。
#
# 用 tools/test-volume/test-volume.sh 建立**两块互不相同**的小型 btrfs loop 卷，分别作为：
#   - 工作盘：Vault 与内部状态同一 btrfs（准入要求），填满数据块；
#   - 状态盘：另一块独立卷，用大量小文件耗尽元数据。
# 然后运行 tests/server-integration/tests/volume_exhaustion.rs（含 reflink 后 CoW 分配失败）。
#
# 满盘注入只作用于本 run 的 loop 镜像；结束后 delete 镜像即恢复容量，不触碰主机卷。
#
# 运行：nix develop --command bash tests/server-integration/run-volume-faults.sh
set -euo pipefail
set -o pipefail
PROG=${0##*/}

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
TEST_VOLUME="$repo_root/tools/test-volume/test-volume.sh"
LAB_VAULT=${VAULTSYNCD_LAB_VAULT:-/data/vaultsyncd-lab}
base_id=${VAULTSYNCD_RUN_ID:-s103-vol-$$}
WORK_ID="$base_id-work"
STATE_ID="$base_id-state"
WORK_MNT=$LAB_VAULT/runs/$WORK_ID/mnt
STATE_MNT=$LAB_VAULT/runs/$STATE_ID/mnt

log() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
die() {
	printf '%s: error: %s\n' "$PROG" "$*" >&2
	exit 1
}
sudo_run() { sudo -n "$@"; }

require_tools() {
	local tool
	for tool in cargo btrfs findmnt mount umount losetup sync; do
		command -v "$tool" >/dev/null 2>&1 || die "缺少 $tool；请在 nix develop 内运行"
	done
	sudo -n true 2>/dev/null || die "需要非交互 sudo -n"
}

cleanup() {
	local status=$?
	set +e
	log ""
	step "清理本 run 的测试卷（$WORK_ID / $STATE_ID）"
	sync
	local id
	for id in "$WORK_ID" "$STATE_ID"; do
		if [ -d "$LAB_VAULT/runs/$id" ]; then
			"$TEST_VOLUME" cleanup "$id" ||
				log "test-volume cleanup 失败，请人工核查 $LAB_VAULT/runs/$id"
		fi
	done
	log "cleanup status=$status"
	exit "$status"
}
trap cleanup EXIT

require_tools
step "0) 测试卷基线"
"$TEST_VOLUME" status
for id in "$WORK_ID" "$STATE_ID"; do
	[ -e "$LAB_VAULT/runs/$id" ] && die "run 目录已存在（$LAB_VAULT/runs/$id），换一个 VAULTSYNCD_RUN_ID"
done

step "1) 建立工作盘与状态盘（各 256M 独立 loop 镜像）"
"$TEST_VOLUME" create "$WORK_ID" 256M
"$TEST_VOLUME" create "$STATE_ID" 256M
for mnt in "$WORK_MNT" "$STATE_MNT"; do
	mountpoint -q "$mnt" || die "镜像未挂载：$mnt"
	sudo_run chown -R "$(id -u):$(id -g)" "$mnt"
done
log "工作盘 $WORK_MNT"
log "状态盘 $STATE_MNT"
df -h "$WORK_MNT" "$STATE_MNT"

step "2) 运行容量故障用例（server-integration volume_exhaustion）"
status=0
(
	cd "$repo_root"
	VAULTSYNCD_S103_WORK_VOLUME="$WORK_MNT" \
		VAULTSYNCD_S103_STATE_VOLUME="$STATE_MNT" \
		cargo test --locked -p vaultsyncd-server-tests --test volume_exhaustion \
		-- --test-threads=1
) || status=$?

step "3) 用例后核对容量恢复"
# 用例会释放自己创建的填充文件；这里只核对并同步，真正的容量恢复由 cleanup 删除镜像完成。
df -h "$WORK_MNT" "$STATE_MNT" || true
"$TEST_VOLUME" verify "$WORK_ID" || log "工作盘 verify 未通过（满盘样本已被用例释放，仅记录）"
"$TEST_VOLUME" verify "$STATE_ID" || log "状态盘 verify 未通过（满盘样本已被用例释放，仅记录）"
exit "$status"
