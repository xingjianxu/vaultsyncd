#!/usr/bin/env bash
# S067: build the real Windows test harness, verify transport hash, run on native NTFS.
# Invoke inside nix develop .#windows. The guest remains online after validation.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
if [ "${VAULTSYNCD_WINDOWS_SHELL:-}" != 1 ]; then
    echo 'run-native-hydration: run inside nix develop .#windows' >&2
    exit 1
fi
vault=$(repo_root)
cd "$vault"
run_id="s067-$(date +%s)-$$"
artifact_dir="$vault/artifacts/S067/$run_id"
mkdir -p "$artifact_dir"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin test --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent \
    --lib --no-run --message-format=json > "$artifact_dir/build.json"
built=$(python3 - "$artifact_dir/build.json" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    entry = json.loads(line)
    if entry.get('reason') == 'compiler-artifact' and entry.get('profile', {}).get('test') and entry.get('executable'):
        print(entry['executable'])
        break
else:
    raise SystemExit('missing Windows test harness')
PY
)
install -m 0644 "$built" "$artifact_dir/s067-platform-tests.exe"
host_hash=$(sha256sum "$artifact_dir/s067-platform-tests.exe" | awk '{print $1}')
cat > "$artifact_dir/guest-native-files.ps1" <<'PS'
param([Parameter(Mandatory=$true)][string]$ExpectedHash)
$ErrorActionPreference = 'Stop'
$exe = Join-Path $PSScriptRoot 's067-platform-tests.exe'
$actual = (Get-FileHash -Algorithm SHA256 $exe).Hash.ToLowerInvariant()
if ($actual -ne $ExpectedHash) { throw 'test binary transport hash mismatch' }
Write-Output "guest_exe_sha256 $actual"
$env:VAULTSYNCD_SKIP_ABANDONED_HYDRATION_CLEANUP='1'
& $exe hydration_tests:: --test-threads=1 --nocapture
exit $LASTEXITCODE
PS
guest_dir="C:/vaultsyncd-lab/$run_id"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/s067-platform-tests.exe" "$artifact_dir/guest-native-files.ps1"
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/guest-native-files.ps1 -ExpectedHash $host_hash" \
    | tee "$artifact_dir/native-tests.log"
echo "MATCH s067-platform-tests.exe $host_hash"
guest_ssh 'powershell -NoProfile -Command "[Environment]::OSVersion.VersionString"'
