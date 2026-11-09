#!/usr/bin/env bash
# vaultsyncd 测试卷生命周期工具（S007）。
#
# 在源码仓库之外的 lab 根（默认 /data/vaultsyncd-dev/vaultsyncd-temp/volumes）内，按 run_id 建立
#   1) 现有合规 btrfs 卷上的专用子卷（run 目录下的 volume/）；
#   2) 独立容量的小型 btrfs loop 镜像（run 目录下的 loop.img），供满盘/ENOSPC 等
#      故障测试使用，避免污染主机大卷的容量语义。
#
# 安全约束（实现为断言，不是注释）：
#   - 镜像/挂载点/meta 路径由脚本按 run_id 计算，永不由调用者给出，且必须落在 lab 根内；
#   - lab 根与所有操作路径拒绝系统/用户路径（/, /home, /nix, /data, /data/llm 等）；
#   - mkfs/mount/umount/losetup/subvolume delete 只作用于本 run 记录并当场核对的设备；
#   - 卸载/删除前用 findmnt 与 losetup 交叉核对来源、回环设备与镜像文件；
#   - 已存在、UUID/设备不匹配或出现未知条目时拒绝操作并保持原状；
#   - 仅使用非交互 sudo -n，失败立即退出，不等待密码输入。
#
# 实测证据与路径规划见 docs/environment.md §11。
set -eu
set -o pipefail

PROG=${0##*/}

LAB_VAULT=${VAULTSYNCD_LAB_VAULT:-/data/vaultsyncd-dev/vaultsyncd-temp/volumes}
LOOP_SIZE=${VAULTSYNCD_LOOP_SIZE:-512M}

RUNS_NAME=runs
VOLUME_NAME=volume
IMAGE_NAME=loop.img
MOUNT_NAME=mnt
META_NAME=run.env

BTRFS=$(command -v btrfs || true)
MKFS_BTRFS=$(command -v mkfs.btrfs || true)
MOUNT=$(command -v mount || true)
UMOUNT=$(command -v umount || true)
LOSETUP=$(command -v losetup || true)
FINDMNT=$(command -v findmnt || true)
STAT=$(command -v stat || true)
READLINK=$(command -v readlink || true)
TRUNCATE=$(command -v truncate || true)
# 非交互提权：-n 保证不会等待密码输入。
sudo_run() { sudo -n "$@"; }

# 临时测试卷只能位于用户指定的数据根之下，禁止触碰既有 win-lab。
LAB_PARENT_WHITELIST="/data/vaultsyncd-dev/vaultsyncd-temp"

# 永远不允许作为 lab 根或其操作路径的目录（精确匹配）。
FORBIDDEN_EXACT="/ /boot /home /nix /nix/store /var /var/lib /var/log /srv /data /wrd /tmp /run /proc /sys /dev /etc /usr /opt /vault /.snapshots /data/llm /data/.snapshots /data/snapshots"

# 永远不允许在其内部创建 lab 目录的父路径（按完整路径分量匹配）。
FORBIDDEN_INSIDE="/boot /home /nix /var/log /.snapshots /data/llm /data/.snapshots /data/snapshots /wrd /tmp /run /proc /sys /dev /etc /vault"

log() { printf '%s\n' "$*"; }
step() { printf '==> %s\n' "$*"; }
warn() { printf '%s: warning: %s\n' "$PROG" "$*" >&2; }

die() {
	printf '%s: error: %s\n' "$PROG" "$*" >&2
	if [ "${CREATE_IN_PROGRESS:-0}" = 1 ]; then
		CREATE_IN_PROGRESS=0
		create_rollback || true
	fi
	exit 1
}

usage() {
	cat <<'EOF'
用法：test-volume.sh <命令> [参数]

命令：
  create <run_id> [大小]  在 lab 根内为该 run 创建专用 btrfs 子卷与 btrfs loop 镜像，
                          并挂载镜像；重复 create 同一 run 会明确拒绝（先 cleanup）。
                          大小默认 512M（也可用 VAULTSYNCD_LOOP_SIZE）。
  verify <run_id>         只用只读命令核对后端、设备归属、子卷身份、路径不重叠与目录为空。
                          run 不存在或已清理时以非零退出并说明原因。
  cleanup <run_id>        卸载（仅当来源是本 run 的 loop 设备）、detach 本 run 的 loop、
                          删除本 run 的子卷与目录；拒绝触碰其他挂载、其他文件或未知条目。
  status                  列出 lab 根与已有 run 的状态（只读）。

环境变量：
  VAULTSYNCD_LAB_VAULT   lab 根，默认 /data/vaultsyncd-dev/vaultsyncd-temp/volumes；必须绝对、已规范化，
                        必须位于 /data/vaultsyncd-dev/vaultsyncd-temp 之下，且不得是系统/用户路径。
  VAULTSYNCD_LOOP_SIZE  loop 镜像大小（truncate 语法），默认 512M。

run_id 规则：^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$；不得包含 '/'、'.'、'..' 或前导 '-'。

示例：
  tools/test-volume/test-volume.sh create s007-run1
  tools/test-volume/test-volume.sh verify s007-run1
  tools/test-volume/test-volume.sh cleanup s007-run1
  tools/test-volume/test-volume.sh status

losetup/mkfs/mount/umount/btrfs subvolume 通过非交互 sudo -n 执行；sudo 不可用时
脚本立即报错退出，不会提示输入密码。
EOF
}

canon() { "$READLINK" -m -- "$1"; }

preflight() {
	local missing="" tool value
	for tool in BTRFS MKFS_BTRFS MOUNT UMOUNT LOSETUP FINDMNT STAT READLINK TRUNCATE; do
		eval "value=\${$tool}"
		[ -n "$value" ] || missing="$missing $tool"
	done
	[ -z "$missing" ] || die "缺少必需工具:$missing"
	sudo_run true 2>/dev/null ||
		die "需要非交互 sudo（sudo -n 失败）；脚本不会提示输入密码，请先配置免密 sudo 或使用具备 vault 能力的环境"
}

# ---------------------------------------------------------------------------
# 路径安全断言
# ---------------------------------------------------------------------------

assert_run_id() {
	local run_id=$1
	[ -n "$run_id" ] || die "缺少 run_id"
	case $run_id in
	[A-Za-z0-9]*) ;;
	*) die "非法 run_id '$run_id'：必须以字母或数字开头（不允许 '.', '..', '-', '/'）" ;;
	esac
	case $run_id in
	*[!A-Za-z0-9._-]*) die "非法 run_id '$run_id'：只允许 A-Z a-z 0-9 . _ -" ;;
	esac
	[ ${#run_id} -le 64 ] || die "非法 run_id '$run_id'：长度超过 64"
}

assert_safe_path() {
	local path=$1 label=$2 require_whitelist=$3 listed matcher canonical
	[ -n "$path" ] || die "$label：路径为空"
	case $path in
	/*) ;;
	*) die "$label：必须是绝对路径，实际为 '$path'" ;;
	esac
	canonical=$(canon "$path")
	[ "$path" = "$canonical" ] || die "$label：路径未规范化（含 .. 或符号链接）：'$path' -> '$canonical'"

	for listed in $FORBIDDEN_EXACT; do
		[ "$path" = "$listed" ] && die "$label：拒绝受保护路径 '$path'"
	done
	# 按完整路径分量判断包含关系，避免 /data 与 /datax 之类的字符串误判。
	for listed in $FORBIDDEN_INSIDE; do
		case $path in
		"$listed"/*) die "$label：拒绝位于受保护路径 '$listed' 之内：'$path'" ;;
		esac
	done

	if [ "$require_whitelist" = yes ]; then
		matcher=""
		for listed in $LAB_PARENT_WHITELIST; do
			case $path in
			"$listed"/*) matcher=$listed ;;
			esac
		done
		[ -n "$matcher" ] || die "$label：'$path' 必须位于白名单父目录（$LAB_PARENT_WHITELIST）之下，实际不在任何允许位置"
	fi
}

# 拒绝把 lab 放进 Git 仓库或 .worktrees。
assert_not_in_repo() {
	local path=$1 cur
	cur=$(canon "$path")
	while [ "$cur" != "/" ]; do
		[ -e "$cur/.git" ] && die "拒绝在 Git 工作区内创建测试卷（发现 '$cur/.git'）：'$path'"
		case $cur in
		*/.worktrees | */.worktrees/*) die "拒绝在 .worktrees 内创建测试卷：'$path'" ;;
		esac
		cur=$(dirname -- "$cur")
	done
}

assert_descendant() {
	local path=$1 parent=$2 label=$3 cpath cparent
	cpath=$(canon "$path")
	cparent=$(canon "$parent")
	[ "$cpath" != "$cparent" ] || die "$label：'$cpath' 不能等于父目录 '$cparent'"
	case $cpath in
	"$cparent"/*) ;;
	*) die "$label：'$cpath' 不在 '$cparent' 之内" ;;
	esac
}

lab_vault_resolve() {
	case $LAB_VAULT in
	/*) ;;
	*) die "lab 根必须是绝对路径：'$LAB_VAULT'" ;;
	esac
	LAB=$(canon "$LAB_VAULT")
	assert_safe_path "$LAB" "lab 根($LAB_VAULT)" yes
	assert_not_in_repo "$LAB"
	RUNS_DIR=$LAB/$RUNS_NAME
}

run_paths() {
	local run_id=$1 p
	assert_run_id "$run_id"
	lab_vault_resolve
	RUN_DIR=$RUNS_DIR/$run_id
	VOLUME_DIR=$RUN_DIR/$VOLUME_NAME
	IMAGE=$RUN_DIR/$IMAGE_NAME
	MOUNT_POINT=$RUN_DIR/$MOUNT_NAME
	META=$RUN_DIR/$META_NAME
	assert_descendant "$RUN_DIR" "$RUNS_DIR" "run 目录"
	assert_descendant "$VOLUME_DIR" "$RUN_DIR" "volume 子卷路径"
	assert_descendant "$IMAGE" "$RUN_DIR" "loop 镜像路径"
	assert_descendant "$MOUNT_POINT" "$RUN_DIR" "挂载点路径"
	for p in "$VOLUME_DIR" "$IMAGE" "$MOUNT_POINT" "$META"; do
		assert_safe_path "$p" "run 内部路径" no
	done
}

# ---------------------------------------------------------------------------
# 只读探针
# ---------------------------------------------------------------------------

mount_point_of() { "$FINDMNT" -n -o TARGET -T "$1" 2>/dev/null || true; }
is_mountpoint() { [ "$(mount_point_of "$1")" = "$(canon "$1")" ]; }
mount_source_of() { "$FINDMNT" -n -o SOURCE -T "$1" 2>/dev/null || true; }
mount_fstype_of() { "$FINDMNT" -n -o FSTYPE -T "$1" 2>/dev/null || true; }
mount_options_of() { "$FINDMNT" -n -o OPTIONS -T "$1" 2>/dev/null || true; }
stat_fstype_of() { "$STAT" -f -c '%T' "$1" 2>/dev/null || true; }
stat_fsid_of() { "$STAT" -f -c '%i' "$1" 2>/dev/null || true; }

loop_backing_file() {
	[ -n "${1:-}" ] || return 0
	"$LOSETUP" -n -O BACK-FILE -- "$1" 2>/dev/null | tr -d ' ' || true
}

fs_uuid_of() {
	sudo_run "$BTRFS" filesystem show -- "$1" 2>/dev/null |
		awk '/uuid:/ { for (i = 1; i <= NF; i++) if ($i == "uuid:") { print $(i + 1); exit } }'
}

# btrfs filesystem show 只接受挂载点或设备；目录需先解析到最近挂载点。
fs_uuid_at() {
	local mount_point
	mount_point=$(mount_point_of "$1")
	[ -n "$mount_point" ] || return 0
	fs_uuid_of "$mount_point"
}

# 列出 $1 之下（不含自身）的挂载点。findmnt -R 对非挂载点路径返回空，因此直接
# 过滤挂载表（-l 关闭树形前缀）并按完整路径分量比较。
mounts_under() {
	local vault target
	vault=$(canon "$1")
	"$FINDMNT" -l -n -o TARGET 2>/dev/null | while IFS= read -r target; do
		case $target in
		"$vault"/*) printf '%s\n' "$target" ;;
		esac
	done
}

subvol_id_of() {
	sudo_run "$BTRFS" subvolume show -- "$1" 2>/dev/null |
		awk -F: '/Subvolume ID/ { gsub(/[ \t]/, "", $2); print $2; exit }'
}

is_subvolume() { [ -n "$(subvol_id_of "$1")" ]; }

first_entry() { find "$1" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null || true; }

assert_no_nocow_mount() {
	local opts=$1 label=$2
	case ",$opts," in
	*,nodatacow,* | *,nodatasum,*) die "$label：挂载选项含 nodatacow/nodatasum，违反 design §11.5 的 CoW/校验要求：$opts" ;;
	esac
}

meta_get() {
	local key=$1 file=$2 line
	[ -r "$file" ] || return 1
	while IFS= read -r line; do
		case $line in
		"$key"=*) printf '%s' "${line#*=}"; return 0 ;;
		esac
	done <"$file"
	return 1
}

meta_require() {
	local value
	value=$(meta_get "$1" "$2" || true)
	[ -n "$value" ] || die "run 元数据缺少 '$1'：$2"
	printf '%s' "$value"
}

# ---------------------------------------------------------------------------
# create
# ---------------------------------------------------------------------------

CREATE_IN_PROGRESS=0

create_rollback() {
	trap - ERR
	if [ -n "${MOUNT_POINT:-}" ] && [ -d "$MOUNT_POINT" ] && is_mountpoint "$MOUNT_POINT"; then
		if [ -n "${LOOP_DEV:-}" ] && [ "$(mount_source_of "$MOUNT_POINT")" = "$LOOP_DEV" ]; then
			sudo_run "$UMOUNT" -- "$MOUNT_POINT" || warn "回滚卸载失败：$MOUNT_POINT"
		else
			warn "挂载来源不是本 run 的 loop 设备，回滚不卸载：$MOUNT_POINT"
		fi
	fi
	if [ -n "${LOOP_DEV:-}" ] && [ -n "${IMAGE:-}" ] &&
		[ "$(loop_backing_file "$LOOP_DEV")" = "$(canon "$IMAGE")" ]; then
		sudo_run "$LOSETUP" -d -- "$LOOP_DEV" || warn "回滚 detach 失败：$LOOP_DEV"
	fi
	if [ -n "${VOLUME_DIR:-}" ] && [ -d "$VOLUME_DIR" ] && is_subvolume "$VOLUME_DIR"; then
		sudo_run "$BTRFS" subvolume delete -- "$VOLUME_DIR" || warn "回滚删除子卷失败：$VOLUME_DIR"
	fi
	if [ -n "${IMAGE:-}" ] && [ -f "$IMAGE" ]; then rm -f -- "$IMAGE"; fi
	if [ -n "${META:-}" ] && [ -f "$META" ]; then rm -f -- "$META"; fi
	if [ -n "${MOUNT_POINT:-}" ] && [ -d "$MOUNT_POINT" ]; then rmdir -- "$MOUNT_POINT" 2>/dev/null || true; fi
	if [ -n "${RUN_DIR:-}" ] && [ -d "$RUN_DIR" ]; then rmdir -- "$RUN_DIR" 2>/dev/null || true; fi
	return 0
}

create_on_err() {
	local status=$?
	CREATE_IN_PROGRESS=0
	warn "create 失败（状态 $status），回滚本进程刚创建的资源"
	create_rollback
	exit "$status"
}

cmd_create() {
	local run_id=$1 size=${2:-$LOOP_SIZE} probe subvol_id fs_uuid entry

	preflight
	run_paths "$run_id"
	case $size in
	'' | *[!0-9KMGkmg]*) die "非法镜像大小 '$size'（示例 512M、1G）" ;;
	esac
	size=$(printf '%s' "$size" | tr 'kmg' 'KMG')

	[ ! -e "$RUN_DIR" ] ||
		die "run '$run_id' 已存在：$RUN_DIR（重复 create 被拒绝；先执行 cleanup $run_id）"

	probe=$LAB
	[ -d "$probe" ] || probe=$(dirname -- "$LAB")
	case $(stat_fstype_of "$probe") in
	btrfs) ;;
	'') die "无法读取 lab 根所在文件系统（缺少父目录？）：$LAB" ;;
	*) die "lab 根所在文件系统为 '$(stat_fstype_of "$probe")'，S007 要求本地 btrfs（C39）" ;;
	esac

	CREATE_IN_PROGRESS=1
	trap create_on_err ERR

	mkdir -p -- "$RUNS_DIR"
	chmod 0750 -- "$LAB" "$RUNS_DIR" 2>/dev/null || true
	mkdir -m 0700 -- "$RUN_DIR"
	step "创建 run 目录 $RUN_DIR"

	sudo_run "$BTRFS" subvolume create -- "$VOLUME_DIR" >/dev/null
	subvol_id=$(subvol_id_of "$VOLUME_DIR")
	[ -n "$subvol_id" ] || die "无法确认子卷身份：$VOLUME_DIR"
	[ -n "$(fs_uuid_at "$VOLUME_DIR")" ] || die "无法确认专用子卷所在 btrfs 文件系统：$VOLUME_DIR"
	step "创建专用 btrfs 子卷 $VOLUME_DIR（subvolid=$subvol_id）"

	"$TRUNCATE" -s "$size" -- "$IMAGE"
	[ -f "$IMAGE" ] || die "镜像创建失败：$IMAGE"
	step "创建 $size 稀疏镜像 $IMAGE（$(stat -c %s -- "$IMAGE") 字节）"

	LOOP_DEV=$(sudo_run "$LOSETUP" --find --show -- "$IMAGE")
	case $LOOP_DEV in
	/dev/loop[0-9]*) ;;
	*) die "losetup 返回非预期设备：'$LOOP_DEV'" ;;
	esac
	[ "$(loop_backing_file "$LOOP_DEV")" = "$(canon "$IMAGE")" ] ||
		die "$LOOP_DEV 的回环后端不是本 run 镜像（实际：$(loop_backing_file "$LOOP_DEV")），拒绝继续"
	step "attached $LOOP_DEV -> $IMAGE"

	sudo_run "$MKFS_BTRFS" -f -q -L "vaultsyncd-$run_id" -- "$LOOP_DEV" >/dev/null
	fs_uuid=$(fs_uuid_of "$LOOP_DEV")
	[ -n "$fs_uuid" ] || die "无法读回 loop 镜像的 btrfs UUID：$LOOP_DEV"
	step "mkfs.btrfs 完成，UUID=$fs_uuid"

	mkdir -m 0700 -- "$MOUNT_POINT"
	sudo_run "$MOUNT" -t btrfs -- "$LOOP_DEV" "$MOUNT_POINT"
	is_mountpoint "$MOUNT_POINT" || die "挂载后 $MOUNT_POINT 仍不是挂载点"
	[ "$(mount_source_of "$MOUNT_POINT")" = "$LOOP_DEV" ] ||
		die "挂载来源校验失败：期望 $LOOP_DEV，实际 $(mount_source_of "$MOUNT_POINT")"
	entry=$(first_entry "$MOUNT_POINT")
	[ -z "$entry" ] || die "新挂载的镜像文件系统非空：$entry"
	step "挂载 $LOOP_DEV -> $MOUNT_POINT"

	cat >"$META" <<EOF
run_id=$run_id
created_at=$(date -Ins)
lab_vault=$LAB
run_dir=$RUN_DIR
volume_dir=$VOLUME_DIR
volume_subvol_id=$subvol_id
volume_fs_uuid=$(fs_uuid_at "$VOLUME_DIR")
image=$IMAGE
image_size=$size
image_size_bytes=$(stat -c %s -- "$IMAGE")
image_fs_uuid=$fs_uuid
loop_device=$LOOP_DEV
mount_point=$MOUNT_POINT
EOF
	chmod 0600 -- "$META"

	CREATE_IN_PROGRESS=0
	trap - ERR
	step "create 完成"
	log ""
	log "lab 根     : $LAB"
	log "run 目录   : $RUN_DIR"
	log "专用子卷   : $VOLUME_DIR（subvolid=$subvol_id）"
	log "loop 镜像  : $IMAGE（$size，UUID=$fs_uuid）"
	log "镜像挂载点 : $MOUNT_POINT（$LOOP_DEV）"
	log "元数据     : $META"
	log ""
	log "下一步：$PROG verify $run_id"
}

# ---------------------------------------------------------------------------
# verify
# ---------------------------------------------------------------------------

cmd_verify() {
	local run_id=$1 rec_run_dir rec_volume rec_image rec_loop rec_mount rec_subvol
	local rec_img_uuid rec_vol_fs rec_bytes
	local nested target extra vol_fstype vol_mp lab_mp vol_source vol_fs_uuid vol_subvol_id
	local lab_fs_uuid vol_fsid vol_entry real_bytes mount_src mount_type mount_opts img_uuid img_fsid img_entry

	preflight
	run_paths "$run_id"
	[ -d "$RUN_DIR" ] || die "run '$run_id' 不存在或已清理：$RUN_DIR（无残留目录）"
	[ -f "$META" ] || die "run '$run_id' 缺少元数据 $META；无法证明归属，拒绝推断"

	rec_run_dir=$(meta_require run_dir "$META")
	rec_volume=$(meta_require volume_dir "$META")
	rec_image=$(meta_require image "$META")
	rec_loop=$(meta_require loop_device "$META")
	rec_mount=$(meta_require mount_point "$META")
	rec_subvol=$(meta_require volume_subvol_id "$META")
	rec_img_uuid=$(meta_require image_fs_uuid "$META")
	rec_vol_fs=$(meta_require volume_fs_uuid "$META")
	rec_bytes=$(meta_require image_size_bytes "$META")

	[ "$(canon "$rec_run_dir")" = "$(canon "$RUN_DIR")" ] || die "元数据 run_dir 与当前路径不符：$rec_run_dir"
	[ "$(canon "$rec_volume")" = "$(canon "$VOLUME_DIR")" ] || die "元数据 volume_dir 越界：$rec_volume"
	[ "$(canon "$rec_image")" = "$(canon "$IMAGE")" ] || die "元数据 image 越界：$rec_image"
	[ "$(canon "$rec_mount")" = "$(canon "$MOUNT_POINT")" ] || die "元数据 mount_point 越界：$rec_mount"
	case $rec_loop in
	/dev/loop[0-9]*) ;;
	*) die "元数据 loop_device 非法：$rec_loop" ;;
	esac

	step "1) 路径与重叠"
	assert_not_in_repo "$LAB"
	assert_descendant "$RUN_DIR" "$LAB" "run 目录"
	assert_descendant "$VOLUME_DIR" "$LAB" "专用子卷"
	assert_descendant "$IMAGE" "$LAB" "loop 镜像"
	assert_descendant "$MOUNT_POINT" "$LAB" "挂载点"
	log "   lab 根 $LAB 在源码仓库之外；全部 run 路径均在其内"
	nested=$(mounts_under "$RUN_DIR")
	extra=""
	for target in $nested; do
		[ "$target" = "$(canon "$MOUNT_POINT")" ] || extra="$extra $target"
	done
	[ -z "$extra" ] || die "run 目录下存在非本 run 的挂载点：$extra"
	log "   run 目录内挂载：${nested:-无}"

	step "2) 专用子卷后端"
	[ -d "$VOLUME_DIR" ] || die "专用子卷不存在：$VOLUME_DIR"
	is_subvolume "$VOLUME_DIR" || die "路径不是 btrfs 子卷：$VOLUME_DIR"
	vol_fstype=$(stat_fstype_of "$VOLUME_DIR")
	[ "$vol_fstype" = "btrfs" ] || die "专用子卷后端为 '$vol_fstype'，要求 btrfs"
	vol_mp=$(mount_point_of "$VOLUME_DIR")
	lab_mp=$(mount_point_of "$LAB")
	[ "$vol_mp" = "$lab_mp" ] || die "专用子卷位于额外挂载点 $vol_mp（lab 根在 $lab_mp），路径存在嵌套挂载"
	vol_source=$(mount_source_of "$VOLUME_DIR")
	lab_fs_uuid=$(fs_uuid_at "$LAB")
	vol_fs_uuid=$(fs_uuid_at "$VOLUME_DIR")
	[ "$vol_fs_uuid" = "$rec_vol_fs" ] || die "专用子卷文件系统 UUID 变化：记录 $rec_vol_fs，实际 $vol_fs_uuid"
	[ "$vol_fs_uuid" = "$lab_fs_uuid" ] || die "专用子卷与 lab 根不在同一 btrfs 文件系统（$vol_fs_uuid vs $lab_fs_uuid）"
	vol_subvol_id=$(subvol_id_of "$VOLUME_DIR")
	[ "$vol_subvol_id" = "$rec_subvol" ] || die "子卷 ID 变化：记录 $rec_subvol，实际 $vol_subvol_id"
	vol_fsid=$(stat_fsid_of "$VOLUME_DIR")
	assert_no_nocow_mount "$(mount_options_of "$VOLUME_DIR")" "专用子卷"
	vol_entry=$(first_entry "$VOLUME_DIR")
	[ -z "$vol_entry" ] || die "专用子卷非空：$vol_entry"
	log "   $VOLUME_DIR（空）"
	log "   fstype=$vol_fstype source=$vol_source fs_uuid=$vol_fs_uuid subvolid=$vol_subvol_id"
	log "   statfs 身份=$vol_fsid（btrfs 的 statfs f_fsid 随子卷变化，只作辅助证据）"
	log "   挂载选项无 nodatacow/nodatasum"

	step "3) loop 镜像与独立容量"
	[ -f "$IMAGE" ] || die "镜像文件不存在：$IMAGE"
	real_bytes=$(stat -c %s -- "$IMAGE")
	[ "$real_bytes" = "$rec_bytes" ] || die "镜像大小变化：记录 $rec_bytes 字节，实际 $real_bytes 字节"
	[ "$(loop_backing_file "$rec_loop")" = "$(canon "$IMAGE")" ] ||
		die "$rec_loop 未指向本 run 镜像（实际：$(loop_backing_file "$rec_loop")）"
	is_mountpoint "$MOUNT_POINT" || die "镜像未挂载：$MOUNT_POINT"
	mount_src=$(mount_source_of "$MOUNT_POINT")
	mount_type=$(mount_fstype_of "$MOUNT_POINT")
	mount_opts=$(mount_options_of "$MOUNT_POINT")
	[ "$mount_src" = "$rec_loop" ] || die "挂载来源不是本 run loop：期望 $rec_loop，实际 $mount_src"
	[ "$mount_type" = "btrfs" ] || die "镜像挂载类型为 '$mount_type'，要求 btrfs"
	assert_no_nocow_mount "$mount_opts" "loop 镜像"
	img_uuid=$(fs_uuid_of "$MOUNT_POINT")
	[ "$img_uuid" = "$rec_img_uuid" ] || die "loop 文件系统 UUID 变化：记录 $rec_img_uuid，实际 $img_uuid"
	[ "$img_uuid" != "$lab_fs_uuid" ] || die "loop 镜像与 lab 根同属一个 btrfs 文件系统（$img_uuid），未构成独立容量"
	img_fsid=$(stat_fsid_of "$MOUNT_POINT")
	[ "$img_fsid" != "$vol_fsid" ] || die "loop 镜像与专用子卷 statfs 身份相同（$img_fsid），未构成独立文件系统"
	img_entry=$(first_entry "$MOUNT_POINT")
	[ -z "$img_entry" ] || die "镜像文件系统非空：$img_entry"
	log "   $IMAGE（$rec_bytes 字节）"
	log "   $rec_loop fstype=$mount_type source=$mount_src fs_uuid=$img_uuid statfs 身份=$img_fsid"
	log "   lab 根 fs_uuid=$lab_fs_uuid；镜像 fs_uuid 与之不同，容量独立"
	log "   镜像文件系统为空；挂载选项无 nodatacow/nodatasum"

	step "4) 结论"
	log "   PASS run=$run_id：后端 btrfs，子卷与 loop 属于本 run，路径不与源码/用户目录重叠，目录为空"
}

# ---------------------------------------------------------------------------
# cleanup
# ---------------------------------------------------------------------------

cmd_cleanup() {
	local run_id=$1 rec_run_dir rec_volume rec_image rec_loop rec_mount rec_subvol
	local target nested name backing entry
	local image_canon

	preflight
	run_paths "$run_id"
	[ -d "$RUN_DIR" ] || die "run '$run_id' 不存在或已清理：$RUN_DIR"
	[ -f "$META" ] ||
		die "run '$run_id' 缺少元数据 $META；无法证明挂载/设备归属，拒绝删除（请人工核查 $RUN_DIR）"

	rec_run_dir=$(meta_require run_dir "$META")
	rec_volume=$(meta_require volume_dir "$META")
	rec_image=$(meta_require image "$META")
	rec_loop=$(meta_require loop_device "$META")
	rec_mount=$(meta_require mount_point "$META")
	rec_subvol=$(meta_require volume_subvol_id "$META")

	[ "$(canon "$rec_run_dir")" = "$(canon "$RUN_DIR")" ] || die "元数据 run_dir 越界：$rec_run_dir"
	[ "$(canon "$rec_volume")" = "$(canon "$VOLUME_DIR")" ] || die "元数据 volume_dir 越界：$rec_volume"
	[ "$(canon "$rec_image")" = "$(canon "$IMAGE")" ] || die "元数据 image 越界：$rec_image"
	[ "$(canon "$rec_mount")" = "$(canon "$MOUNT_POINT")" ] || die "元数据 mount_point 越界：$rec_mount"
	case $rec_loop in
	/dev/loop[0-9]*) ;;
	*) die "元数据 loop_device 非法：$rec_loop" ;;
	esac
	image_canon=$(canon "$IMAGE")

	step "0) 条目校验（未识别条目立即停止，保持原状）"
	for entry in "$RUN_DIR"/* "$RUN_DIR"/.[!.]*; do
		[ -e "$entry" ] || [ -L "$entry" ] || continue
		case $entry in
		"$VOLUME_DIR" | "$IMAGE" | "$MOUNT_POINT" | "$META") ;;
		*) die "run 目录出现未知条目 '$entry'，拒绝清理（请人工核查 $RUN_DIR）" ;;
		esac
	done
	step "   run 目录条目均在预期集合内（volume/loop.img/mnt/run.env）"

	step "1) 挂载归属核对"
	nested=$(mounts_under "$RUN_DIR")
	for target in $nested; do
		[ "$target" = "$(canon "$MOUNT_POINT")" ] ||
			die "run 目录下存在非本 run 的挂载点 $target，拒绝清理"
	done
	if is_mountpoint "$MOUNT_POINT"; then
		[ "$(mount_source_of "$MOUNT_POINT")" = "$rec_loop" ] ||
			die "挂载点 $MOUNT_POINT 的来源为 $(mount_source_of "$MOUNT_POINT")，不是本 run 的 $rec_loop，拒绝卸载"
		[ "$(loop_backing_file "$rec_loop")" = "$image_canon" ] ||
			die "$rec_loop 未指向本 run 镜像（实际：$(loop_backing_file "$rec_loop")），拒绝卸载"
		sudo_run "$UMOUNT" -- "$MOUNT_POINT"
		is_mountpoint "$MOUNT_POINT" && die "卸载失败，$MOUNT_POINT 仍是挂载点"
		step "   已卸载 $MOUNT_POINT（来源 $rec_loop）"
	else
		step "   $MOUNT_POINT 未挂载，跳过 umount"
	fi

	step "2) detach 本 run 的 loop"
	while read -r name backing; do
		[ -n "$name" ] || continue
		[ "$backing" = "$image_canon" ] ||
			die "发现 $name 的后端 '$backing' 与本 run 镜像路径不同，拒绝 detach"
		sudo_run "$LOSETUP" -d -- "$name"
		step "   detached $name（$backing）"
	done <<EOF
$("$LOSETUP" -a -n -O NAME,BACK-FILE 2>/dev/null | awk -v img="$image_canon" '$2 == img' || true)
EOF

	step "3) 删除专用子卷"
	if [ -e "$VOLUME_DIR" ]; then
		is_subvolume "$VOLUME_DIR" || die "$VOLUME_DIR 存在但不是 btrfs 子卷，拒绝自动删除"
		[ "$(subvol_id_of "$VOLUME_DIR")" = "$rec_subvol" ] ||
			die "子卷 ID 变化（记录 $rec_subvol，实际 $(subvol_id_of "$VOLUME_DIR")），拒绝删除"
		[ -z "$(first_entry "$VOLUME_DIR")" ] || die "$VOLUME_DIR 非空，拒绝删除"
		sudo_run "$BTRFS" subvolume delete -- "$VOLUME_DIR" >/dev/null
		step "   已删除子卷 $VOLUME_DIR（subvolid=$rec_subvol）"
	else
		step "   子卷 $VOLUME_DIR 不存在，跳过"
	fi

	step "4) 删除本 run 文件"
	for entry in "$RUN_DIR"/* "$RUN_DIR"/.[!.]*; do
		[ -e "$entry" ] || [ -L "$entry" ] || continue
		case $entry in
		"$IMAGE" | "$META") rm -f -- "$entry" ;;
		"$MOUNT_POINT")
			[ -L "$MOUNT_POINT" ] && die "挂载点是符号链接，拒绝删除：$MOUNT_POINT"
			rmdir -- "$MOUNT_POINT"
			;;
		*) die "清理期间出现新条目 '$entry'，已停止（请人工核查 $RUN_DIR）" ;;
		esac
	done
	rmdir -- "$RUN_DIR"
	rmdir -- "$RUNS_DIR" 2>/dev/null || true
	rmdir -- "$LAB" 2>/dev/null || true
	step "   已删除 run 目录 $RUN_DIR"

	step "5) 清理后核验"
	[ ! -e "$RUN_DIR" ] || die "清理后 $RUN_DIR 仍存在"
	[ -z "$(loop_backing_file "$rec_loop")" ] || die "清理后 $rec_loop 仍指向 $(loop_backing_file "$rec_loop")"
	log "   $RUN_DIR 不存在；$rec_loop 已 detach"
	log "   lab 根之下剩余挂载：$(mounts_under "$LAB" | tr '\n' ' ')"
	log "   PASS run=$run_id 已清理"
}

# ---------------------------------------------------------------------------
# status
# ---------------------------------------------------------------------------

cmd_status() {
	local run_dir run_id rec_loop rec_mount state

	preflight
	lab_vault_resolve
	log "lab 根       : $LAB"
	if [ ! -d "$LAB" ]; then
		log "状态         : 未创建（$LAB 不存在）"
		return 0
	fi
	log "lab 后端     : $(mount_source_of "$LAB") / $(mount_fstype_of "$LAB") / $(stat_fstype_of "$LAB")"
	log "lab 挂载点   : $(mount_point_of "$LAB")"
	log "lab 可用空间 : $(df -B1 --output=avail "$LAB" | tail -n 1) 字节"
	[ -d "$RUNS_DIR" ] || return 0
	log "run 目录     : $RUNS_DIR"
	for run_dir in "$RUNS_DIR"/*; do
		[ -d "$run_dir" ] || continue
		run_id=${run_dir##*/}
		rec_loop=$(meta_get loop_device "$run_dir/$META_NAME" || true)
		rec_mount=$(meta_get mount_point "$run_dir/$META_NAME" || true)
		state="未挂载"
		if [ -n "$rec_mount" ] && is_mountpoint "$rec_mount"; then
			state="已挂载 $(mount_source_of "$rec_mount")"
		fi
		printf '  %-24s loop=%-12s %s\n' "$run_id" "${rec_loop:-?}" "$state"
	done
}

# ---------------------------------------------------------------------------

case ${1:-} in
-h | --help | help) usage ;;
create)
	[ $# -ge 2 ] || { usage >&2; die "create 需要 run_id"; }
	[ $# -le 3 ] || die "create 参数过多"
	cmd_create "$2" "${3:-$LOOP_SIZE}"
	;;
verify)
	[ $# -eq 2 ] || { usage >&2; die "verify 需要且只需要 run_id"; }
	cmd_verify "$2"
	;;
cleanup)
	[ $# -eq 2 ] || { usage >&2; die "cleanup 需要且只需要 run_id"; }
	cmd_cleanup "$2"
	;;
status)
	[ $# -eq 1 ] || { usage >&2; die "status 不接受参数"; }
	cmd_status
	;;
'')
	usage >&2
	exit 2
	;;
*)
	usage >&2
	die "未知命令 '$1'"
	;;
esac
