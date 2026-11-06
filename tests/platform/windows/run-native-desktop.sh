#!/usr/bin/env bash
# S080: same real Windows session, independent agent and Tauri WebView.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
root=$(repo_root)
cd "$root"
test -f apps/desktop/dist/index.html || { echo 'build desktop frontend in default devShell first' >&2; exit 1; }
run_id="s080-$(date +%s)-$$"
artifact_dir="$root/artifacts/S080/$run_id"
mkdir -p "$artifact_dir"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent -p vaultsyncd-desktop --features vaultsyncd-desktop/native-acceptance
build_dir="${CARGO_TARGET_DIR:-$root/target}/$TARGET_TRIPLE/debug"
cp "$build_dir/vaultsyncd-agent.exe" "$build_dir/vaultsyncd-desktop.exe" "$artifact_dir/"
cp "$script_dir/s080-desktop.ps1" "$artifact_dir/"
python3 -c 'import hashlib,json,pathlib,sys; p=pathlib.Path(sys.argv[1]); (p/"hashes.json").write_text(json.dumps({f.name:hashlib.sha256(f.read_bytes()).hexdigest() for f in p.glob("*.exe")}))' "$artifact_dir"
guest_dir="C:/vaultsyncd-lab/$run_id"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Path '$guest_dir' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/vaultsyncd-agent.exe" "$artifact_dir/vaultsyncd-desktop.exe" "$artifact_dir/s080-desktop.ps1" "$artifact_dir/hashes.json"
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/s080-desktop.ps1" | tee "$artifact_dir/native.log"
echo "ARTIFACTS $artifact_dir"
echo "GUEST $guest_dir (test state preserved; VM stays online)"
