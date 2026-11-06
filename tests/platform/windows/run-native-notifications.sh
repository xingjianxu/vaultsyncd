#!/usr/bin/env bash
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1
root=$(repo_root);cd "$root"
run_id="s083-notifications-$(date +%s)-$$"
artifact_dir="$root/artifacts/S083/$run_id";mkdir -p "$artifact_dir"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent -p vaultsyncd-desktop --features vaultsyncd-desktop/native-acceptance
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent --example s083_notice_fixture
build="${CARGO_TARGET_DIR:-$root/target}/$TARGET_TRIPLE/debug"
cp "$build/vaultsyncd-agent.exe" "$build/vaultsyncd-desktop.exe" "$build/examples/s083_notice_fixture.exe" "$artifact_dir/"
cp "$script_dir/s083-notifications.ps1" "$artifact_dir/"
python3 -c 'import pathlib,hashlib,json,sys;p=pathlib.Path(sys.argv[1]);f=p/"s083-notifications.ps1";f.write_text(f.read_text(),encoding="utf-8-sig");(p/"hashes.json").write_text(json.dumps({f.name:hashlib.sha256(f.read_bytes()).hexdigest() for f in p.glob("*.exe")}))' "$artifact_dir"
guest_dir="C:/vaultsyncd-lab/$run_id"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory '$guest_dir' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/vaultsyncd-agent.exe" "$artifact_dir/vaultsyncd-desktop.exe" "$artifact_dir/s083_notice_fixture.exe" "$artifact_dir/s083-notifications.ps1" "$artifact_dir/hashes.json"
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/s083-notifications.ps1" | tee "$artifact_dir/native.log"
echo "ARTIFACTS $artifact_dir"
echo "GUEST $guest_dir (VM stays online)"
