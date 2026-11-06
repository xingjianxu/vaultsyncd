#!/usr/bin/env bash
# S070: native headless agent process against a real isolated Linux server over an SSH loopback tunnel.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
root=$(repo_root)
cd "$root"
# S071 reuses the real-process regression but never runs S070's old-sample cleanup test.
task="${VAULTSYNCD_PROCESS_TASK:-S070}"
case "$task" in
    S070) native_test=all ;;
    S071) native_test=native_headless_import_real_sync_queue_restart_and_single_instance ;;
    S072) native_test=management_native::native_management_every_command ;;
    *) echo 'unsupported native process task' >&2; exit 1 ;;
esac
artifact_dir="$root/artifacts/$task/native-$(date +%s)-$$"
mkdir -p "$artifact_dir"
helper="${VAULTSYNCD_RELEASE_SERVER:-$root/target/debug/examples/release-native-server}"
test -x "$helper" || { echo 'build: nix develop --command cargo build --locked -p vaultsyncd-server-tests --example release-native-server' >&2; exit 1; }
server_pid=''
tunnel_pid=''
cleanup() {
    touch "$artifact_dir/stop"
    if [ -n "$tunnel_pid" ]; then kill "$tunnel_pid" 2>/dev/null || true; wait "$tunnel_pid" 2>/dev/null || true; fi
    if [ -n "$server_pid" ]; then
        for _ in $(seq 1 100); do kill -0 "$server_pid" 2>/dev/null || break; sleep .1; done
        if kill -0 "$server_pid" 2>/dev/null; then kill "$server_pid"; fi
        wait "$server_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT
"$helper" "$artifact_dir/host-fixture.json" "$artifact_dir/stop" > "$artifact_dir/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 200); do test -f "$artifact_dir/host-fixture.json" && break; kill -0 "$server_pid" || { cat "$artifact_dir/server.log"; exit 1; }; sleep .1; done
host_port=$(python3 - "$artifact_dir" <<'PY'
import json,sys,pathlib,urllib.parse,os
p=pathlib.Path(sys.argv[1]);v=json.loads((p/'host-fixture.json').read_text());port=urllib.parse.urlparse(v['url']).port
v['url']='http://127.0.0.1:18770'
fd=os.open(p/'guest-fixture.json',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
with os.fdopen(fd,'w') as f:json.dump(v,f)
print(port)
PY
)
ssh -p "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR -o ExitOnForwardFailure=yes \
    -N -R "127.0.0.1:18770:127.0.0.1:$host_port" "$GUEST_USER@$GUEST_HOST" > "$artifact_dir/tunnel.log" 2>&1 &
tunnel_pid=$!
sleep .5
kill -0 "$tunnel_pid" || { cat "$artifact_dir/tunnel.log"; exit 1; }
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent --bin vaultsyncd-agent
cargo xwin test --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent --test native_process --no-run --message-format=json > "$artifact_dir/build.json"
built=$(python3 - "$artifact_dir/build.json" <<'PY'
import json,sys
for line in open(sys.argv[1]):
 v=json.loads(line)
 if v.get('reason')=='compiler-artifact' and v.get('executable') and v.get('profile',{}).get('test'):
  print(v['executable']);break
else:raise SystemExit('missing native test binary')
PY
)
install -m 0644 "$built" "$artifact_dir/s070-process-tests.exe"
host_hash=$(sha256sum "$artifact_dir/s070-process-tests.exe" | awk '{print $1}')
fixture_hash=$(sha256sum "$artifact_dir/guest-fixture.json" | awk '{print $1}')
cleanup_allowlist=$(printf '%s' "${VAULTSYNCD_PROCESS_CLEANUP_ALLOWLIST:-[]}" | base64 -w0)
cat > "$artifact_dir/guest-process.ps1" <<'PS'
param([string]$ExpectedHash,[string]$FixtureHash,[string]$CleanupAllowlist,[string]$TestName='all',[switch]$Worker)
$ErrorActionPreference='Stop'
$exe=Join-Path $PSScriptRoot 's070-process-tests.exe'
$fixture=Join-Path $PSScriptRoot 'guest-fixture.json'
if ($Worker) {
    $env:VAULTSYNCD_PROCESS_CLEANUP_ALLOWLIST=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($CleanupAllowlist))
    $env:VAULTSYNCD_PROCESS_FIXTURE=$fixture
    $env:VAULTSYNCD_AGENT_EXE=Join-Path $PSScriptRoot 'vaultsyncd-agent.exe'
    try {
        $p=New-Object System.Diagnostics.Process
        $p.StartInfo.FileName=$exe
        $p.StartInfo.Arguments='--nocapture --test-threads=1'
        if ($TestName -ne 'all') {$p.StartInfo.Arguments="--exact $TestName --nocapture --test-threads=1"}
        $p.StartInfo.UseShellExecute=$false
        $p.StartInfo.RedirectStandardOutput=$true
        $p.StartInfo.RedirectStandardError=$true
        $p.Start() | Out-Null
        $stdout=$p.StandardOutput.ReadToEndAsync(); $stderr=$p.StandardError.ReadToEndAsync()
        $p.WaitForExit()
        ($stdout.Result+$stderr.Result) | Set-Content (Join-Path $PSScriptRoot 'process.log')
        $p.ExitCode | Set-Content (Join-Path $PSScriptRoot 'process.exit')
    }
    catch {$_ | Out-String | Add-Content (Join-Path $PSScriptRoot 'process.log'); '99' | Set-Content (Join-Path $PSScriptRoot 'process.exit')}
    exit
}
if ((Get-FileHash $exe -Algorithm SHA256).Hash.ToLowerInvariant() -ne $ExpectedHash) {throw 'native test transport mismatch'}
if ((Get-FileHash $fixture -Algorithm SHA256).Hash.ToLowerInvariant() -ne $FixtureHash) {throw 'fixture transport mismatch'}
Write-Output "guest_test_sha256 $ExpectedHash"
Write-Output ('guest_agent_sha256 '+(Get-FileHash (Join-Path $PSScriptRoot 'vaultsyncd-agent.exe') -Algorithm SHA256).Hash.ToLowerInvariant())
$task='vaultsyncd-s070-process-'+[Guid]::NewGuid().ToString('N')
$action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker -CleanupAllowlist $CleanupAllowlist -TestName $TestName"
$principal=New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try {
    Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
    Start-ScheduledTask -TaskName $task
    $end=(Get-Date).AddSeconds(240)
    while (!(Test-Path (Join-Path $PSScriptRoot 'process.exit'))) {if ((Get-Date) -ge $end) {throw 'native process timeout'};Start-Sleep -Milliseconds 200}
    Get-Content (Join-Path $PSScriptRoot 'process.log')
    $result=[int](Get-Content (Join-Path $PSScriptRoot 'process.exit'))
} finally {Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue;Remove-Item -LiteralPath $fixture -Force}
exit $result
PS
guest_dir="C:/vaultsyncd-lab/${task,,}-driver-$$"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir" guest_scp "target/$TARGET_TRIPLE/debug/vaultsyncd-agent.exe" "$artifact_dir/s070-process-tests.exe" "$artifact_dir/guest-fixture.json" "$artifact_dir/guest-process.ps1"
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/guest-process.ps1 -ExpectedHash $host_hash -FixtureHash $fixture_hash -CleanupAllowlist $cleanup_allowlist -TestName $native_test" | tee "$artifact_dir/native-tests.log"
guest_ssh "powershell -NoProfile -Command \"Remove-Item -LiteralPath '$guest_dir' -Recurse -Force\""
echo "MATCH s070-process-tests.exe $host_hash"
echo "ARTIFACTS $artifact_dir"
