#!/usr/bin/env bash
# S069: native CFAPI release against a real isolated Linux server over an SSH loopback tunnel.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
root=$(repo_root)
cd "$root"
artifact_dir="$root/artifacts/S069/native-$(date +%s)-$$"
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
v['url']='http://127.0.0.1:18769'
fd=os.open(p/'guest-fixture.json',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
with os.fdopen(fd,'w') as f:json.dump(v,f)
print(port)
PY
)
ssh -p "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR -o ExitOnForwardFailure=yes \
    -N -R "127.0.0.1:18769:127.0.0.1:$host_port" "$GUEST_USER@$GUEST_HOST" > "$artifact_dir/tunnel.log" 2>&1 &
tunnel_pid=$!
sleep .5
kill -0 "$tunnel_pid" || { cat "$artifact_dir/tunnel.log"; exit 1; }
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin test --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent --test native_release --no-run --message-format=json > "$artifact_dir/build.json"
built=$(python3 - "$artifact_dir/build.json" <<'PY'
import json,sys
for line in open(sys.argv[1]):
 v=json.loads(line)
 if v.get('reason')=='compiler-artifact' and v.get('executable') and v.get('profile',{}).get('test'):
  print(v['executable']);break
else:raise SystemExit('missing native test binary')
PY
)
install -m 0644 "$built" "$artifact_dir/s069-native-tests.exe"
host_hash=$(sha256sum "$artifact_dir/s069-native-tests.exe" | awk '{print $1}')
fixture_hash=$(sha256sum "$artifact_dir/guest-fixture.json" | awk '{print $1}')
cat > "$artifact_dir/guest-release.ps1" <<'PS'
param([string]$ExpectedHash,[string]$FixtureHash)
$ErrorActionPreference='Stop'
$exe=Join-Path $PSScriptRoot 's069-native-tests.exe'
$fixture=Join-Path $PSScriptRoot 'guest-fixture.json'
if ((Get-FileHash $exe -Algorithm SHA256).Hash.ToLowerInvariant() -ne $ExpectedHash) { throw 'exe transport mismatch' }
if ((Get-FileHash $fixture -Algorithm SHA256).Hash.ToLowerInvariant() -ne $FixtureHash) { throw 'fixture transport mismatch' }
Write-Output "guest_exe_sha256 $ExpectedHash"
$env:VAULTSYNCD_RELEASE_FIXTURE=$fixture
try { & $exe --nocapture --test-threads=1; $result=$LASTEXITCODE }
finally { Remove-Item -LiteralPath $fixture -Force }
exit $result
PS
capacity_script=()
if [ "${VAULTSYNCD_RELEASE_NTFS_CAPACITY:-0}" = 1 ]; then
    cp "$script_dir/guest-native-capacity.ps1" "$artifact_dir/"
    capacity_script=("$artifact_dir/guest-native-capacity.ps1")
fi
guest_dir="C:/vaultsyncd-lab/s069-driver-$$"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/s069-native-tests.exe" "$artifact_dir/guest-fixture.json" "$artifact_dir/guest-release.ps1" "${capacity_script[@]}"
guest_runner=guest-release.ps1
if [ "${VAULTSYNCD_RELEASE_NTFS_CAPACITY:-0}" = 1 ]; then guest_runner=guest-native-capacity.ps1; fi
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/$guest_runner -ExpectedHash $host_hash -FixtureHash $fixture_hash" | tee "$artifact_dir/native-tests.log"
guest_ssh "powershell -NoProfile -Command \"Remove-Item -LiteralPath '$guest_dir' -Recurse -Force\""
echo "MATCH s069-native-tests.exe $host_hash"
echo "ARTIFACTS $artifact_dir"
