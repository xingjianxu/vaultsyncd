#!/usr/bin/env bash
# S104：独占btrfs loop上的真实提交/下载/GC/备份崩溃与恢复，不使用Windows替身。
# 从仓库根运行：nix develop --command bash tests/faults/run-maintenance-crash.sh
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root"
run_id=${VAULTSYNCD_RUN_ID:-s104-maintenance-$$}
case "$run_id" in s104-*) ;; *) printf 'run_id must start with s104-\n' >&2; exit 2;; esac
lab_root=/data/vaultsyncd-lab
volume=$lab_root/runs/$run_id/mnt
report=${VAULTSYNCD_S104_REPORT:-$repo_root/artifacts/S104/$run_id}
mkdir -p "$report"
report=$(realpath "$report")
volume_tool=$repo_root/tools/test-volume/test-volume.sh
[ ! -e "$lab_root/runs/$run_id" ] || { printf 'run already exists: %s\n' "$run_id" >&2; exit 2; }
cleanup() {
    local result=$?
    trap - EXIT
    if [ -e "$lab_root/runs/$run_id/run.env" ]; then
        if ! bash "$volume_tool" cleanup "$run_id" >"$report/cleanup.log" 2>&1; then
            cat "$report/cleanup.log" >&2
            [ "$result" -ne 0 ] || result=1
        fi
    fi
    printf '%s\n' "$result" >"$report/exit"
    exit "$result"
}
trap cleanup EXIT
bash "$volume_tool" create "$run_id" 2G >"$report/volume-create.log" 2>&1
sudo -n mount -o remount,user_subvol_rm_allowed "$volume"
sudo -n chown "$(id -u):$(id -g)" "$volume"
bash "$volume_tool" verify "$run_id" >"$report/volume-verify.log" 2>&1
cargo build --locked -p vaultsyncd-admin --bin vaultsyncd-admin >"$report/admin-build.log" 2>&1
target_dir=${CARGO_TARGET_DIR:-$repo_root/target}
target_dir=$(realpath "$target_dir")
export VAULTSYNCD_ADMIN_BIN=$target_dir/debug/vaultsyncd-admin
export VAULTSYNCD_S104_VOLUME=$volume
export VAULTSYNCD_S104_TRACE=$report/trace.jsonl
cargo test --locked -p vaultsyncd-server-tests --features test-faults --test maintenance_crash \
    -- --ignored --nocapture --test-threads=1 >"$report/maintenance.log" 2>&1

# T33：复用S091真实旧程序拒写/完整归档回退装置；actor与admin均从本树现编。
cargo test --locked -p vaultsyncd-server-store --lib --no-run --message-format=json \
    >"$report/store-build.jsonl" 2>"$report/store-build.log"
actor=$(python3 - "$report/store-build.jsonl" <<'PY'
import json, sys
paths = []
for line in open(sys.argv[1]):
    item = json.loads(line)
    if (item.get("reason") == "compiler-artifact" and item.get("profile", {}).get("test")
        and item.get("target", {}).get("name") == "vaultsyncd_server_store" and item.get("executable")):
        paths.append(item["executable"])
assert len(paths) == 1, paths
print(paths[0])
PY
)
mkdir "$volume/s091-t33"
VAULTSYNCD_S091_VOLUME=$volume/s091-t33 \
VAULTSYNCD_S091_ADMIN_BIN=$VAULTSYNCD_ADMIN_BIN \
VAULTSYNCD_S091_SERVER_TEST_BIN=$actor \
cargo test --locked -p vaultsyncd-server-tests --test upgrade_rollback_native \
    -- --ignored --nocapture --test-threads=1 >"$report/upgrade-rollback.log" 2>&1
printf 'S104 real maintenance/crash and upgrade recovery passed; evidence: %s\n' "$report"
