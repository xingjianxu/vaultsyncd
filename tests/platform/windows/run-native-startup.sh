#!/usr/bin/env bash
set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
vault=$(repo_root)
cd "$vault"
test -f apps/desktop/dist/index.html || { echo 'build desktop frontend first' >&2; exit 1; }
run_id="s185-$(date +%s)-$$"
artifact_dir="${VAULTSYNCD_NATIVE_ARTIFACTS:-/data/vaultsyncd-dev/vaultsyncd-temp/S167-native/evidence}/$run_id"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/vaultsyncd-dev/cache/S167-cargo-target}"
export TMPDIR="$artifact_dir/tmp"
mkdir -p "$artifact_dir/program" "$TMPDIR"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent -p vaultsyncd-cli -p vaultsyncd-desktop
build_dir="${CARGO_TARGET_DIR:-$vault/target}/$TARGET_TRIPLE/debug"
for name in vaultsyncd-agent vaultsyncd-cli vaultsyncd-desktop vaultsyncd-session; do
  cp "$build_dir/$name.exe" "$artifact_dir/program/"
done
cp "$script_dir/s185-direct-startup.ps1" "$artifact_dir/"
cp packaging/windows/zip/*.cmd packaging/windows/zip/README-ZIP.txt "$artifact_dir/program/"
printf '%s\n' 'S167 native test only; unsigned development binaries.' > "$artifact_dir/program/UNSIGNED-TEST.txt"
python3 - "$artifact_dir" <<'PY'
import hashlib,json,pathlib,sys
p=pathlib.Path(sys.argv[1]); (p/'hashes.json').write_text(json.dumps({f.name:hashlib.sha256(f.read_bytes()).hexdigest() for f in (p/'program').glob('*.exe')}))
PY
guest_dir="C:/vaultsyncd-lab/$run_id"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Path '$guest_dir/program' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir/program" guest_scp "$artifact_dir/program/"*
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/s185-direct-startup.ps1" "$artifact_dir/hashes.json"
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/s185-direct-startup.ps1" | tee "$artifact_dir/native.log"
scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/result.json" "$artifact_dir/"
guest_ssh "powershell -NoProfile -Command \"Compress-Archive -Path '$guest_dir/*.json','$guest_dir/*.log','$guest_dir/*-dialog.txt' -DestinationPath '$guest_dir/guest-evidence.zip'\""
scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/guest-evidence.zip" "$artifact_dir/"
echo "ARTIFACTS $artifact_dir"
echo "GUEST $guest_dir (isolated startup state preserved; VM stays online)"
