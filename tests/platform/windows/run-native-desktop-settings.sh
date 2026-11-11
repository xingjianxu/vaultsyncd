#!/usr/bin/env bash
# Run the production desktop in an isolated interactive Windows profile. No compilation here.
set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$script_dir/common.sh"
if [ "$#" -ne 1 ]; then
  echo 'usage: run-native-desktop-settings.sh <directory-with-four-production-exes>' >&2
  exit 2
fi
program=$(realpath "$1")
run_id="s200-$(date +%s)-$$"
artifact_dir="${VAULTSYNCD_NATIVE_ARTIFACTS:-/data/vaultsyncd-dev/S200-desktop-settings-20261111/native}/$run_id"
case "$artifact_dir" in /data/vaultsyncd-dev/*) ;; *) echo 'native artifacts must be under /data/vaultsyncd-dev' >&2; exit 2 ;; esac
mkdir -p "$artifact_dir/program"
for name in vaultsyncd-agent vaultsyncd-cli vaultsyncd-desktop vaultsyncd-session; do
  cp "$program/$name.exe" "$artifact_dir/program/"
done
# Windows PowerShell 5 must recognize the Chinese UIA names as UTF-8.
python3 - "$script_dir/s200-desktop-settings.ps1" "$artifact_dir" <<'PY'
import hashlib, json, pathlib, sys
source, destination = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
(destination/source.name).write_text(source.read_text(), encoding='utf-8-sig')
(destination/'hashes.json').write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in (destination/'program').glob('*.exe')}))
PY
guest_dir="C:/vaultsyncd-lab/$run_id"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Path '$guest_dir/program' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir/program" guest_scp "$artifact_dir/program/"*
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/s200-desktop-settings.ps1" "$artifact_dir/hashes.json"
if [ -n "${VAULTSYNCD_S203_FIXTURE_DIR:-}" ]; then
  GUEST_TARGET_DIR="$guest_dir" guest_scp "$VAULTSYNCD_S203_FIXTURE_DIR/fixture.json" "$VAULTSYNCD_S203_FIXTURE_DIR/ca.pem"
fi
status=0
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/s200-desktop-settings.ps1" | tee "$artifact_dir/native.log" || status=$?
# Preserve the failed run as well as successful evidence.
guest_ssh "powershell -NoProfile -Command \"Compress-Archive -Path '$guest_dir/*.json','$guest_dir/*.log','$guest_dir/*.txt' -DestinationPath '$guest_dir/guest-evidence.zip' -Force\"" || true
scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/guest-evidence.zip" "$artifact_dir/" || true
if [ "$status" -eq 0 ]; then
  scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/result.json" "$artifact_dir/"
fi
echo "ARTIFACTS $artifact_dir"
echo "GUEST $guest_dir (isolated state and recovery exports preserved; VM stays online)"
exit "$status"
