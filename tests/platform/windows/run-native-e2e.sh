#!/usr/bin/env bash
# S074: three isolated Windows client identities against a real Linux server.
#
# Topology: one Windows 11 guest (the only guest in this repository; a second one is S107),
# three client environments with distinct DeviceId/state/client.sqlite/vault/cache and one
# agent process at a time (product invariant: one agent per user session). The file
# docs/operations/windows-lab.md records the guest facts; the run log repeats the topology.
#
# Usage (after `nix develop --command cargo build --locked -p vaultsyncd-server-tests \
#   --example e2e-native-server`):
#   VAULTSYNCD_WINDOWS_CACHE=/wrd/prj/vaultsyncd/.cache/windows \
#     nix develop .#windows --command bash tests/platform/windows/run-native-e2e.sh
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
vault=$(repo_root)
cd "$vault"
task=${VAULTSYNCD_E2E_TASK:-S074}
case "$task" in S074|S102) ;; *) echo 'unsupported e2e task' >&2; exit 2;; esac
target_dir=${CARGO_TARGET_DIR:-$vault/target}
scenario="$vault/tests/scenarios/three-clients.json"
test -f "$scenario" || { echo "missing scenario $scenario" >&2; exit 1; }
helper="${VAULTSYNCD_RELEASE_SERVER:-$target_dir/debug/examples/e2e-native-server}"
test -x "$helper" || {
    echo 'build: nix develop --command cargo build --locked -p vaultsyncd-server-tests --example e2e-native-server' >&2
    exit 1
}
artifact_dir="$vault/artifacts/$task/e2e-$(date +%s)-$$"
mkdir -p "$artifact_dir"
server_pid=''
tunnel_pid=''
cleanup() {
    touch "$artifact_dir/stop"
    if [ -n "$tunnel_pid" ]; then
        kill "$tunnel_pid" 2>/dev/null || true
        wait "$tunnel_pid" 2>/dev/null || true
    fi
    if [ -n "$server_pid" ]; then
        for _ in $(seq 1 100); do kill -0 "$server_pid" 2>/dev/null || break; sleep .1; done
        if kill -0 "$server_pid" 2>/dev/null; then kill "$server_pid"; fi
        wait "$server_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT

commit=$(git rev-parse HEAD)
if git diff --quiet && git diff --cached --quiet; then dirty=0; else dirty=1; fi
echo "commit=$commit dirty=$dirty"
echo "helper_sha256=$(sha256sum "$helper" | awk '{print $1}')"

# 1. Real Linux server: isolated btrfs Vault under target/, seeded from the scenario's server_seed.
"$helper" "$artifact_dir/host-fixture.json" "$artifact_dir/stop" "$scenario" > "$artifact_dir/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 600); do
    test -f "$artifact_dir/host-fixture.json" && break
    kill -0 "$server_pid" 2>/dev/null || { cat "$artifact_dir/server.log"; exit 1; }
    sleep .1
done
test -f "$artifact_dir/host-fixture.json" || { cat "$artifact_dir/server.log"; exit 1; }
# Guest loopback 18770 is this run's private reverse-tunnel port; nothing else uses it.
host_port=$(python3 - "$artifact_dir" <<'PY'
import json,sys,pathlib,urllib.parse,os
p=pathlib.Path(sys.argv[1]);v=json.loads((p/'host-fixture.json').read_text())
port=urllib.parse.urlparse(v['url']).port
v['url']='http://127.0.0.1:18770'
fd=os.open(p/'guest-fixture.json',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
with os.fdopen(fd,'w') as f: json.dump(v,f)
print(port)
PY
)

# 2. Authenticated reverse tunnel: guest 127.0.0.1:18770 -> host real server.
ssh -p "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
    -o BatchMode=yes -o LogLevel=ERROR -o ExitOnForwardFailure=yes -N \
    -R "127.0.0.1:18770:127.0.0.1:$host_port" "$GUEST_USER@$GUEST_HOST" > "$artifact_dir/tunnel.log" 2>&1 &
tunnel_pid=$!
sleep .5
kill -0 "$tunnel_pid" 2>/dev/null || { cat "$artifact_dir/tunnel.log"; exit 1; }

# 3. Cross-build the real product agent and the orchestrator for Windows.
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent --bin vaultsyncd-agent
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-e2e --bin vaultsyncd-e2e
agent_hash=$(sha256sum "$target_dir/$TARGET_TRIPLE/debug/vaultsyncd-agent.exe" | awk '{print $1}')
e2e_hash=$(sha256sum "$target_dir/$TARGET_TRIPLE/debug/vaultsyncd-e2e.exe" | awk '{print $1}')
fixture_hash=$(sha256sum "$artifact_dir/guest-fixture.json" | awk '{print $1}')
scenario_hash=$(sha256sum "$scenario" | awk '{print $1}')
echo "agent_sha256=$agent_hash"
echo "e2e_sha256=$e2e_hash"

guest_dir="C:/vaultsyncd-lab/${task,,}-driver-$$"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir' | Out-Null\""
cat > "$artifact_dir/guest-e2e.ps1" <<'PS'
param(
    [Parameter(Mandatory=$true)][string]$Driver,
    [Parameter(Mandatory=$true)][string]$E2eHash,
    [Parameter(Mandatory=$true)][string]$AgentHash,
    [Parameter(Mandatory=$true)][string]$FixtureHash,
    [Parameter(Mandatory=$true)][string]$ScenarioHash,
    [string]$Commit='unknown',
    [string]$Dirty='1',
    [string]$Scenario='three-clients.json',
    [string]$Suite='S074',
    [switch]$Worker
)
$ErrorActionPreference = 'Stop'
$exe = Join-Path $Driver 'vaultsyncd-e2e.exe'
$agent = Join-Path $Driver 'vaultsyncd-agent.exe'
$fixture = Join-Path $Driver 'guest-fixture.json'
$scenarioFile = Join-Path $Driver $Scenario
$report = Join-Path $Driver 'report.json'
$log = Join-Path $Driver 'e2e.log'
$exitFile = Join-Path $Driver 'e2e.exit'

if (-not $Worker) {
    foreach ($check in @(
        @($exe, $E2eHash), @($agent, $AgentHash),
        @($fixture, $FixtureHash), @($scenarioFile, $ScenarioHash)
    )) {
        if ((Get-FileHash $check[0] -Algorithm SHA256).Hash.ToLowerInvariant() -ne $check[1]) {
            throw "transport hash mismatch: $($check[0])"
        }
    }
    $os = Get-CimInstance -ClassName Win32_OperatingSystem
    Write-Output "guest_os: $($os.Caption)"
    Write-Output "guest_version: $($os.Version) build $($os.BuildNumber)"
    Write-Output "guest_e2e_sha256 $E2eHash"
    Write-Output "guest_agent_sha256 $AgentHash"
    $task = 'vaultsyncd-' + $Suite.ToLowerInvariant() + '-e2e-' + [Guid]::NewGuid().ToString('N')
    $arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker -Driver `"$Driver`" -E2eHash $E2eHash -AgentHash $AgentHash -FixtureHash $FixtureHash -ScenarioHash $ScenarioHash -Commit $Commit -Dirty $Dirty -Scenario `"$Scenario`" -Suite $Suite"
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument $arguments
    $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited
    try {
        Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
        Start-ScheduledTask -TaskName $task
        $deadline = (Get-Date).AddSeconds(2400)
        while (-not (Test-Path $exitFile)) {
            if ((Get-Date) -ge $deadline) { throw 'e2e native worker timeout' }
            Start-Sleep -Milliseconds 500
        }
        if (Test-Path $log) { Get-Content $log -Encoding UTF8 }
        $code = [int](Get-Content $exitFile)
    } finally {
        Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
    }
    exit $code
}

# Worker: runs in the interactive desktop session so CFAPI/WinCred/named pipe are real.
$env:VAULTSYNCD_E2E_COMMIT = $Commit
$env:VAULTSYNCD_E2E_DIRTY = $Dirty
$arguments = @(
    '--fixture', "`"$fixture`"",
    '--scenario', "`"$scenarioFile`"",
    '--agent', "`"$agent`"",
    '--driver', "`"$Driver`"",
    '--report', "`"$report`""
) -join ' '
if ($Suite -eq 'S102') { $arguments += ' --faults' }
$process = New-Object System.Diagnostics.Process
$process.StartInfo.FileName = $exe
$process.StartInfo.Arguments = $arguments
$process.StartInfo.UseShellExecute = $false
$process.StartInfo.RedirectStandardOutput = $true
$process.StartInfo.RedirectStandardError = $true
$process.StartInfo.WorkingDirectory = $Driver
$process.Start()
$stdout = $process.StandardOutput.ReadToEndAsync()
$stderr = $process.StandardError.ReadToEndAsync()
$process.WaitForExit()
[IO.File]::WriteAllText($log, $stdout.Result + $stderr.Result, (New-Object Text.UTF8Encoding($false)))
$process.ExitCode | Set-Content $exitFile
exit $process.ExitCode
PS

# guest_scp targets the default GUEST_DIR; stage explicitly into this run's driver.
GUEST_TARGET_DIR="$guest_dir" guest_scp "$target_dir/$TARGET_TRIPLE/debug/vaultsyncd-agent.exe" \
    "$target_dir/$TARGET_TRIPLE/debug/vaultsyncd-e2e.exe" \
    "$artifact_dir/guest-fixture.json" "$scenario" "$artifact_dir/guest-e2e.ps1"

set +e
timeout 2700 ssh -p "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes \
    -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR \
    "$GUEST_USER@$GUEST_HOST" \
    "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/guest-e2e.ps1 -Driver $guest_dir -E2eHash $e2e_hash -AgentHash $agent_hash -FixtureHash $fixture_hash -ScenarioHash $scenario_hash -Commit $commit -Dirty $dirty -Scenario three-clients.json -Suite $task" \
    2>&1 | tee "$artifact_dir/native-e2e.log"
result=${PIPESTATUS[0]}
set -e

# Fetch the machine-readable report and agent logs before removing the run's guest directory.
scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
    -o BatchMode=yes -o LogLevel=ERROR \
    "$GUEST_USER@$GUEST_HOST:$guest_dir/report.json" "$artifact_dir/report.json" 2>/dev/null || true
scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
    -o BatchMode=yes -o LogLevel=ERROR \
    "$GUEST_USER@$GUEST_HOST:$guest_dir/run-e2e-*/failure.json" "$artifact_dir/failure.json" 2>/dev/null || true
mkdir -p "$artifact_dir/logs"
scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
    -o BatchMode=yes -o LogLevel=ERROR -r \
    "$GUEST_USER@$GUEST_HOST:$guest_dir/run-e2e-*/logs/*.log" "$artifact_dir/logs/" 2>/dev/null || true

if [ "$result" -ne 0 ]; then
    echo "check=fail native_e2e exit=$result (guest driver kept at $guest_dir for diagnosis)" >&2
    echo "ARTIFACTS $artifact_dir"
    exit "$result"
fi

if [ ! -f "$artifact_dir/report.json" ]; then
    echo 'check=fail report.json was not transferred back' >&2
    exit 1
fi

if [ "$task" = S102 ]; then
    python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); assert r["passed"] and len(r["scenes"])==6; print("check=pass S102 scenarios=6")' "$artifact_dir/report.json"
    guest_ssh "powershell -NoProfile -Command \"Remove-Item -LiteralPath '$guest_dir' -Recurse -Force\""
    echo "ARTIFACTS $artifact_dir"
    exit 0
fi

# Server-side convergence: every committed entry (including conflict copies) must exist on the
# btrfs Vault work directory with the same bytes, and no extra file may linger.
python3 - "$artifact_dir/report.json" <<'PY'
import hashlib, json, pathlib, sys
report = json.loads(pathlib.Path(sys.argv[1]).read_text())
if not report.get('passed'):
    raise SystemExit('check=fail report.passed is false')
work = pathlib.Path(report['work_dir'])
expected = {entry['path']: entry['hash'] for entry in report['final_entries']}
actual = {}
for path in work.rglob('*'):
    if path.is_file():
        actual[path.relative_to(work).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
missing = sorted(set(expected) - set(actual))
extra = sorted(set(actual) - set(expected))
mismatch = sorted(path for path, digest in expected.items() if actual.get(path) not in (None, digest))
if missing or extra or mismatch:
    raise SystemExit(f'check=fail server_work_namespace missing={missing} extra={extra} mismatch={mismatch}')
coverage = report['coverage']
if coverage['missing']:
    raise SystemExit(f"check=fail missing input content {coverage['missing']}")
if not report['convergence']['matched'] or report['convergence']['server_paths'] != len(expected):
    raise SystemExit('check=fail convergence report inconsistent')
if len(report['blocks']) < 5:
    raise SystemExit('check=fail scenario blocks incomplete')
topology = report['topology']
if topology['physical_windows_guests'] != 1 or topology['concurrent_agent_processes'] != 1:
    raise SystemExit(f"check=fail topology not recorded as one guest/sequential: {topology}")
if report['proxy']['dropped'] < 2:
    raise SystemExit('check=fail lost-response injections did not both fire')
print(f"check=pass server_work_matches_committed_namespace entries={len(expected)}")
print(f"check=pass coverage_inputs={len(coverage['inputs'])} missing=0")
print(f"check=pass guest={report['guest']} server_version={report['server_version']} commit={report['commit']} dirty={report['dirty']}")
print(f"check=pass clients={len(report['clients'])} blocks={len(report['blocks'])} conflict_copies={len(report['final_conflict_copies'])}")
print(f"check=pass topology={json.dumps(topology, ensure_ascii=False)}")
print(f"check=pass lost_responses={report['proxy']['dropped']} operation_events={len(report['proxy']['events'])}")
PY

# Remove only this run's guest driver; VM stays online for the next card.
guest_ssh "powershell -NoProfile -Command \"Remove-Item -LiteralPath '$guest_dir' -Recurse -Force\"" || \
    echo "check=warn guest_driver_cleanup_failed $guest_dir" >&2
echo "MATCH vaultsyncd-agent.exe $agent_hash"
echo "MATCH vaultsyncd-e2e.exe $e2e_hash"
echo "check=pass s074_native_e2e"
echo "ARTIFACTS $artifact_dir"
