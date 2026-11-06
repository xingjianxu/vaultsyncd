#!/usr/bin/env bash
# Run S070 process-ownership and Windows credential tests in the native guest.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
cd "$(repo_root)"
mkdir -p artifacts/S070
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin build --locked --target "$TARGET_TRIPLE" -p "$TEST_PACKAGE" --bin agent_lifecycle_smoke
binary="target/$TARGET_TRIPLE/debug/agent_lifecycle_smoke.exe"
cp "$binary" artifacts/S070/agent_lifecycle_smoke.exe
host_hash=$(sha256sum "$binary" | awk '{print $1}')
guest_dir='C:/vaultsyncd-lab/s070'
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir" guest_scp "$binary"
cat > artifacts/S070/lifecycle-interactive.ps1 <<'PS'
param([string]$ExpectedHash, [switch]$Worker)
$ErrorActionPreference='Stop'
$base=$PSScriptRoot
$exe=Join-Path $base 'agent_lifecycle_smoke.exe'
if ($Worker) {
    try {
        & $exe *> (Join-Path $base 'interactive.log')
        $LASTEXITCODE | Set-Content (Join-Path $base 'interactive.exit')
    } catch { $_ | Out-String | Add-Content (Join-Path $base 'interactive.log'); '99' | Set-Content (Join-Path $base 'interactive.exit') }
    exit
}
$actual=(Get-FileHash -Algorithm SHA256 $exe).Hash.ToLowerInvariant()
if ($actual -ne $ExpectedHash) {throw 'transport hash mismatch'}
Write-Output "guest_sha256 $actual"
Remove-Item (Join-Path $base 'interactive.exit') -Force -ErrorAction SilentlyContinue
$task='vaultsyncd-s070-life-'+[Guid]::NewGuid().ToString('N')
$action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal=New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try {
    Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
    Start-ScheduledTask -TaskName $task
    $end=(Get-Date).AddSeconds(45)
    while (!(Test-Path (Join-Path $base 'interactive.exit'))) {
        if ((Get-Date) -ge $end) {throw 'interactive lifecycle test timeout'}
        Start-Sleep -Milliseconds 200
    }
    Get-Content (Join-Path $base 'interactive.log')
    $code=[int](Get-Content (Join-Path $base 'interactive.exit'))
} finally {Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue}
exit $code
PS
GUEST_TARGET_DIR="$guest_dir" guest_scp artifacts/S070/lifecycle-interactive.ps1
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/lifecycle-interactive.ps1 -ExpectedHash $host_hash" | tee artifacts/S070/lifecycle-native.log
