#!/usr/bin/env bash
# S071: real Windows named pipe ACL/token identity, framing and bounded lifecycle tests.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
cd "$(repo_root)"
artifact_dir="$(repo_root)/artifacts/S071/ipc-$(date +%s)-$$"
mkdir -p "$artifact_dir"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin test --locked --target "$TARGET_TRIPLE" -p "$PLATFORM_PACKAGE" -p vaultsyncd-agent --lib --no-run --message-format=json > "$artifact_dir/build.json"
for target in vaultsyncd_platform_windows vaultsyncd_agent; do
    built=$(python3 - "$artifact_dir/build.json" "$target" <<'PY'
import json,sys
for line in open(sys.argv[1]):
    entry=json.loads(line)
    if entry.get('reason')=='compiler-artifact' and entry.get('profile',{}).get('test') and entry.get('executable') and entry.get('target',{}).get('name')==sys.argv[2]:
        print(entry['executable']); break
else: raise SystemExit('missing native test binary: '+sys.argv[2])
PY
)
    install -m 0644 "$built" "$artifact_dir/$target.exe"
done
host_hash=$(sha256sum "$artifact_dir/vaultsyncd_platform_windows.exe" | awk '{print $1}')
agent_hash=$(sha256sum "$artifact_dir/vaultsyncd_agent.exe" | awk '{print $1}')
cat > "$artifact_dir/guest-ipc.ps1" <<'PS'
param([Parameter(Mandatory=$true)][string]$ExpectedHash,[Parameter(Mandatory=$true)][string]$AgentHash)
$ErrorActionPreference='Stop'
$exe=Join-Path $PSScriptRoot 'vaultsyncd_platform_windows.exe'
$agent=Join-Path $PSScriptRoot 'vaultsyncd_agent.exe'
if ((Get-FileHash $exe -Algorithm SHA256).Hash.ToLowerInvariant() -ne $ExpectedHash) { throw 'native test transport mismatch' }
if ((Get-FileHash $agent -Algorithm SHA256).Hash.ToLowerInvariant() -ne $AgentHash) { throw 'native agent test transport mismatch' }
Write-Output "guest_test_sha256 $ExpectedHash"
Write-Output "guest_agent_test_sha256 $AgentHash"
$user='s071-'+[Guid]::NewGuid().ToString('N').Substring(0,12)
$password='S071!'+[Guid]::NewGuid().ToString('N')+'aZ9'
try {
    New-LocalUser -Name $user -Password (ConvertTo-SecureString $password -AsPlainText -Force) -AccountNeverExpires | Out-Null
    $env:VAULTSYNCD_IPC_TEST_USER=$user
    $env:VAULTSYNCD_IPC_TEST_PASSWORD=$password
    & $exe ipc::tests --test-threads=1 --nocapture
    $result=$LASTEXITCODE
    & $agent ipc::tests --test-threads=1 --nocapture
    if ($LASTEXITCODE -ne 0) {$result=$LASTEXITCODE}
} finally {
    Remove-Item Env:\VAULTSYNCD_IPC_TEST_PASSWORD -ErrorAction SilentlyContinue
    Remove-Item Env:\VAULTSYNCD_IPC_TEST_USER -ErrorAction SilentlyContinue
    Remove-LocalUser -Name $user
}
[Environment]::OSVersion.VersionString
exit $result
PS
guest_dir="C:/vaultsyncd-lab/s071-ipc-$$"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/vaultsyncd_platform_windows.exe" "$artifact_dir/vaultsyncd_agent.exe" "$artifact_dir/guest-ipc.ps1"
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/guest-ipc.ps1 -ExpectedHash $host_hash -AgentHash $agent_hash" | tee "$artifact_dir/native-tests.log"
guest_ssh "powershell -NoProfile -Command \"Remove-Item -LiteralPath '$guest_dir' -Recurse -Force\""
echo "MATCH vaultsyncd_platform_windows.exe $host_hash"
echo "MATCH vaultsyncd_agent.exe $agent_hash"
echo "ARTIFACTS $artifact_dir"
