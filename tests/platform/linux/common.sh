#!/usr/bin/env bash
# S013 Linux 存储准入验收脚本的共享辅助函数（被 run-*.sh source）。
#
# 约定：
#   - 全部命令在 `nix develop` 内执行（需要 cargo、btrfs-progs、e2fsprogs、util-linux）。
#   - 测试卷、loop、挂载、子卷只允许出现在 tools/test-volume/test-volume.sh 建立的 run 目录内。
#   - 只使用非交互 sudo -n；失败立即停止，不等待密码输入。
#   - 任何测试样本都在本脚本自己的 run_id 之下，结束时（含失败）必须清理并核对。

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../../.." && pwd)

TEST_VOLUME="$repo_root/tools/test-volume/test-volume.sh"
LAB_VAULT=${VAULTSYNCD_LAB_VAULT:-/data/vaultsyncd-temp/volumes}
ADMISSION_BIN="$repo_root/target/debug/examples/storage-admission"
FILE_PUBLISH_BIN="$repo_root/target/debug/examples/file-publish"
PATH_ACCESS_BIN="$repo_root/target/debug/examples/path-access"
OBJECT_CAPTURE_BIN="$repo_root/target/debug/examples/object-capture"

CASE_TOTAL=0
CASE_FAILED=0

log() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }

pass() {
	CASE_TOTAL=$((CASE_TOTAL + 1))
	printf 'PASS %s\n' "$*"
}

fail_case() {
	CASE_TOTAL=$((CASE_TOTAL + 1))
	CASE_FAILED=$((CASE_FAILED + 1))
	printf 'FAIL %s\n' "$*" >&2
}

die() {
	printf '%s: error: %s\n' "${PROG:-test}" "$*" >&2
	exit 1
}

summary() {
	log ""
	log "用例：$CASE_TOTAL 项，失败 $CASE_FAILED 项"
	[ "$CASE_FAILED" -eq 0 ] || return 1
	log "结论：全部通过"
}

require_dev_shell() {
	for tool in cargo btrfs mkfs.btrfs findmnt losetup mount umount truncate stat; do
		command -v "$tool" >/dev/null 2>&1 ||
			die "缺少 $tool；请在 nix develop 内运行：nix develop --command bash $0"
	done
}

require_sudo_noninteractive() {
	sudo -n true 2>/dev/null || die "需要非交互 sudo -n（不提示密码）"
}

require_tools() {
	for tool in "$@"; do
		command -v "$tool" >/dev/null 2>&1 || die "缺少工具 $tool（S013 需要 e2fsprogs 提供 mkfs.ext4/chattr/filefrag）"
	done
}

# 在 run 目录内以 sudo 执行（btrfs 子卷操作需要 CAP_SYS_ADMIN）。
sudo_run() { sudo -n "$@"; }

# 把当前用户设为目录属主，便于在其内创建测试样本。
own_run_path() {
	sudo_run chown "$(id -u):$(id -g)" -- "$1"
}

build_admission_bin() {
	step "构建 S013 原型命令（cargo build --locked --example storage-admission）"
	(
		cd "$repo_root"
		cargo build --locked -p vaultsyncd-platform-linux-server --example storage-admission
	)
	[ -x "$ADMISSION_BIN" ] || die "缺少产物 $ADMISSION_BIN"
	log "产物：$ADMISSION_BIN"
}

build_file_publish_bin() {
	step "构建 S014 原型命令（cargo build --locked --example file-publish）"
	(
		cd "$repo_root"
		cargo build --locked -p vaultsyncd-platform-linux-server --example file-publish
	)
	[ -x "$FILE_PUBLISH_BIN" ] || die "缺少产物 $FILE_PUBLISH_BIN"
	log "产物：$FILE_PUBLISH_BIN"
}

build_path_access_bin() {
	step "构建 S032 诊断命令（cargo build --locked --example path-access）"
	(
		cd "$repo_root"
		cargo build --locked -p vaultsyncd-platform-linux-server --example path-access
	)
	[ -x "$PATH_ACCESS_BIN" ] || die "缺少产物 $PATH_ACCESS_BIN"
	log "产物：$PATH_ACCESS_BIN"
}

build_object_capture_bin() {
	step "构建 S033 对象写入/读取命令（cargo build --locked --example object-capture）"
	(
		cd "$repo_root"
		cargo build --locked -p vaultsyncd-platform-linux-server --example object-capture
	)
	[ -x "$OBJECT_CAPTURE_BIN" ] || die "缺少产物 $OBJECT_CAPTURE_BIN"
	log "产物：$OBJECT_CAPTURE_BIN"
}

# run_admission <子命令与参数...>：结果写入 CMD_OUT / CMD_STATUS。
# 同时保留 ADMISSION_OUT / ADMISSION_STATUS 别名，供 S013 脚本里的直接引用继续工作。
run_admission() {
	set +e
	CMD_OUT=$("$ADMISSION_BIN" "$@" 2>&1)
	CMD_STATUS=$?
	set -e
	ADMISSION_OUT=$CMD_OUT
	ADMISSION_STATUS=$CMD_STATUS
}

# run_file_publish <子命令与参数...>：结果写入 CMD_OUT / CMD_STATUS。
run_file_publish() {
	set +e
	CMD_OUT=$("${FILE_PUBLISH_BIN:?先运行 build_file_publish_bin}" "$@" 2>&1)
	CMD_STATUS=$?
	set -e
}

# run_path_access <子命令与参数...>：结果写入 CMD_OUT / CMD_STATUS。
run_path_access() {
	set +e
	CMD_OUT=$("${PATH_ACCESS_BIN:?先运行 build_path_access_bin}" "$@" 2>&1)
	CMD_STATUS=$?
	set -e
}

# run_object_capture <子命令与参数...>：结果写入 CMD_OUT / CMD_STATUS。
run_object_capture() {
	set +e
	CMD_OUT=$("${OBJECT_CAPTURE_BIN:?先运行 build_object_capture_bin}" "$@" 2>&1)
	CMD_STATUS=$?
	set -e
}

# expect_decision <label> <accepted|rejected|inconclusive> [reason] [期望退出码]
expect_decision() {
	local label=$1 decision=$2 reason=${3:-} expected_status=${4:-}
	local status_ok=1

	[ -z "$expected_status" ] || [ "$CMD_STATUS" = "$expected_status" ] || status_ok=0
	if [ "$status_ok" = 0 ]; then
		fail_case "$label：期望退出码 $expected_status，实际 $CMD_STATUS"
		printf '%s\n' "$CMD_OUT" >&2
		return 0
	fi
	if ! grep -qx "decision=$decision" <<<"$CMD_OUT"; then
		fail_case "$label：期望 decision=$decision，实际输出："
		printf '%s\n' "$CMD_OUT" >&2
		return 0
	fi
	if [ -n "$reason" ] && ! grep -qx "reason=$reason" <<<"$CMD_OUT"; then
		fail_case "$label：期望 reason=$reason，实际输出："
		printf '%s\n' "$CMD_OUT" >&2
		return 0
	fi
	pass "$label（decision=$decision${reason:+, reason=$reason}）"
}

# expect_line <label> <pattern>：输出里必须有一行匹配（grep -E）。
expect_line() {
	local label=$1 pattern=$2
	if grep -Eq "$pattern" <<<"$CMD_OUT"; then
		pass "$label"
	else
		fail_case "$label：输出缺少匹配 /$pattern/："
		printf '%s\n' "$CMD_OUT" >&2
	fi
}

# field <key>：从最近一次输出里取 key=value 的 value（取第一行）。
field() {
	sed -n "s/^$1=//p" <<<"$CMD_OUT" | head -n 1
}

# expect_field <label> <key> <期望值>
expect_field() {
	local label=$1 key=$2 expected=$3 actual
	actual=$(field "$key")
	if [ "$actual" = "$expected" ]; then
		pass "$label（$key=$expected）"
	else
		fail_case "$label：期望 $key=$expected，实际 $key=$actual"
	fi
}

expect_status() {
	local label=$1 expected=$2
	if [ "$CMD_STATUS" = "$expected" ]; then
		pass "$label（exit=$expected）"
	else
		fail_case "$label：期望退出码 $expected，实际 $CMD_STATUS"
		printf '%s\n' "$CMD_OUT" >&2
	fi
}

# 在 run 目录内创建目录（父目录为当前用户所有时可省略 sudo）。
run_mkdir() {
	local path=$1
	mkdir -p -- "$path" || die "无法创建 $path"
}

mount_bind() {
	sudo_run mount --bind -- "$1" "$2"
}

mount_tmpfs() {
	sudo_run mount -t tmpfs -o size=4M tmpfs -- "$1"
}

umount_checked() {
	local target=$1
	if is_mountpoint "$target"; then
		sudo_run umount -- "$target" || die "卸载失败：$target"
	fi
}

# cleanup 专用：卸载失败只记录，不中断后续清理。
umount_tolerant() {
	local target=$1
	if is_mountpoint "$target"; then
		sudo_run umount -- "$target" || log "卸载失败（需人工核查）：$target"
	fi
}

mountpoint_of() {
	findmnt -n -o TARGET -T "$1" 2>/dev/null || true
}

is_mountpoint() {
	[ "$(mountpoint_of "$1")" = "$(readlink -m -- "$1")" ]
}

snapshot_mounts() {
	findmnt -l -n -o TARGET,SOURCE,FSTYPE | sort
}

snapshot_loops() {
	losetup -a 2>/dev/null | sort || true
}
