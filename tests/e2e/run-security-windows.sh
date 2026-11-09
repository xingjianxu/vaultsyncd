#!/usr/bin/env bash
# S105: sequential, real Windows entry points. Host helpers and frontend must be built first.
set -euo pipefail
vault=$(git rev-parse --show-toplevel)
cd "$vault"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
mkdir -p artifacts/S105
if [ "$#" = 0 ]; then set -- ipc cli rules files history hydration release capacity; fi
for stage in "$@"; do
  case "$stage" in
    ipc) script=run-native-ipc.sh ;;
    cli) script=run-native-process.sh ;;
    rules) script=run-native-process.sh ;;
    files) script=run-native-files-ui.sh ;;
    history) script=run-native-history.sh ;;
    hydration) script=run-native-hydration.sh ;;
    release|capacity) script=run-native-release.sh ;;
    *) echo "unknown security stage: $stage" >&2; exit 2 ;;
  esac
  case "$stage" in
    cli) VAULTSYNCD_PROCESS_TASK=S073 bash "tests/platform/windows/$script" > "artifacts/S105/final-$stage.log" 2>&1 ;;
    rules) VAULTSYNCD_PROCESS_TASK=S105 bash "tests/platform/windows/$script" > "artifacts/S105/final-$stage.log" 2>&1 ;;
    capacity) VAULTSYNCD_RELEASE_NTFS_CAPACITY=1 bash "tests/platform/windows/$script" > "artifacts/S105/final-$stage.log" 2>&1 ;;
    *) bash "tests/platform/windows/$script" > "artifacts/S105/final-$stage.log" 2>&1 ;;
  esac
  echo "PASS native security $stage (artifacts/S105/final-$stage.log)"
done
