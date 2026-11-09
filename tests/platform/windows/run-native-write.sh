#!/usr/bin/env bash
# S068: build the real Windows test harness, verify transport hash, run on native NTFS.
# Invoke inside nix develop .#windows. The guest remains online after validation.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
if [ "${VAULTSYNCD_WINDOWS_SHELL:-}" != 1 ]; then
    echo 'run-native-files: run inside nix develop .#windows' >&2
    exit 1
fi
vault=$(repo_root)
cd "$vault"
artifact_dir="$vault/artifacts/S068"
mkdir -p "$artifact_dir"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin test --locked --target "$TARGET_TRIPLE" -p "$PLATFORM_PACKAGE" \
    --no-run --message-format=json > "$artifact_dir/build.json"
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
install -m 0644 "$built" "$artifact_dir/s068-platform-tests.exe"
host_hash=$(sha256sum "$artifact_dir/s068-platform-tests.exe" | awk '{print $1}')
cat > "$artifact_dir/guest-native-files.ps1" <<'PS'
param([Parameter(Mandatory=$true)][string]$ExpectedHash)
$ErrorActionPreference = 'Stop'
$exe = Join-Path $PSScriptRoot 's068-platform-tests.exe'
$actual = (Get-FileHash -Algorithm SHA256 $exe).Hash.ToLowerInvariant()
if ($actual -ne $ExpectedHash) { throw 'test binary transport hash mismatch' }
Write-Output "guest_exe_sha256 $actual"
& $exe --test-threads=1 --nocapture
$testExit=$LASTEXITCODE
if ($testExit -eq 0) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class S068Cleanup {
    [DllImport("cldapi.dll", CharSet=CharSet.Unicode)]
    public static extern int CfUnregisterSyncRoot(string path);
}
'@
    # Only this card's synthesized run directories. Never enumerate another card's vaults.
    Get-ChildItem -LiteralPath $PSScriptRoot -Directory -Filter 'run-*' | ForEach-Object {
        $run=$_.FullName
        $vault=Join-Path $run 'vault'
        if (Test-Path -LiteralPath $vault) {
            Get-ChildItem -LiteralPath $vault -Force | Remove-Item -Recurse -Force
            $unregister=[S068Cleanup]::CfUnregisterSyncRoot($vault)
            Write-Output "s068_cleanup_vault $vault unregister=$unregister"
        }
        Remove-Item -LiteralPath $run -Recurse -Force
    }
}
exit $testExit
PS
guest_dir='C:/vaultsyncd-lab/s068'
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/s068-platform-tests.exe" "$artifact_dir/guest-native-files.ps1"
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/guest-native-files.ps1 -ExpectedHash $host_hash" \
    | tee "$artifact_dir/native-tests.log"
echo "MATCH s068-platform-tests.exe $host_hash"
guest_ssh 'powershell -NoProfile -Command "[Environment]::OSVersion.VersionString"'
