#!/usr/bin/env bash
# S105 actual CLI/TCP backup artifact authorization, with S088 crash/release regression.
set -euo pipefail
vault=$(git rev-parse --show-toplevel)
cd "$vault"
run_id="s105-backup-$(date +%s)-$$"
volume="/data/vaultsyncd-temp/volumes/runs/$run_id/mnt"
cleanup() {
  result=$?
  trap - EXIT
  bash tools/test-volume/test-volume.sh cleanup "$run_id" || { echo 'S105 test volume cleanup failed' >&2; exit 1; }
  exit "$result"
}
bash tools/test-volume/test-volume.sh create "$run_id" 1G
trap cleanup EXIT
sudo -n mount -o remount,user_subvol_rm_allowed "$volume"
sudo -n chown "$(id -u):$(id -g)" "$volume"
VAULTSYNCD_S088_VOLUME="$volume" VAULTSYNCD_ADMIN_BIN="$vault/target/debug/vaultsyncd-admin" \
  cargo test --locked -p vaultsyncd-server-tests --test backup_export_native -- --ignored --nocapture --test-threads=1
