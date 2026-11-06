#!/usr/bin/env bash
# Real native namespace adapter consumed by S070 first-import assembly.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
cd "$(repo_root)"
mkdir -p artifacts/S070
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin test --locked --target "$TARGET_TRIPLE" -p "$PLATFORM_PACKAGE" --no-run --message-format=json > artifacts/S070/namespace-build.json
built=$(python3 - <<'PY'
import json
for line in open('artifacts/S070/namespace-build.json'):
    row=json.loads(line)
    if row.get('reason')=='compiler-artifact' and row.get('profile',{}).get('test') and row.get('executable'):
        print(row['executable']);break
else: raise SystemExit('missing native test binary')
PY
)
cp "$built" artifacts/S070/namespace-tests.exe
host_hash=$(sha256sum artifacts/S070/namespace-tests.exe | awk '{print $1}')
guest_dir='C:/vaultsyncd-lab/s070'
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir" guest_scp artifacts/S070/namespace-tests.exe
guest_ssh "powershell -NoProfile -Command \"\$h=(Get-FileHash -Algorithm SHA256 '$guest_dir/namespace-tests.exe').Hash.ToLowerInvariant(); if (\$h -ne '$host_hash') {throw 'transport hash mismatch'}; Write-Output ('guest_sha256 '+\$h); & '$guest_dir/namespace-tests.exe' native_application_namespace --test-threads=1 --nocapture; exit \$LASTEXITCODE\"" | tee artifacts/S070/namespace-native.log
