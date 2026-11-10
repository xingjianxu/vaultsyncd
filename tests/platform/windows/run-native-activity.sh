#!/usr/bin/env bash
# S172: actual Windows Tauri -> named pipe -> agent -> authenticated TLS -> btrfs server.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
vault=$(repo_root)
cd "$vault"
run_id="S172-native-$(date -u +%Y%m%dT%H%M%S)-$$"
artifact_dir="/data/vaultsyncd-dev/S172-sync-records-20261110/$run_id"
mkdir -p "$artifact_dir"
chmod 700 "$artifact_dir"
target_dir=${CARGO_TARGET_DIR:-$vault/target}
helper=${VAULTSYNCD_RELEASE_SERVER:-$target_dir/debug/examples/e2e-native-server}
test -x "$helper" || { echo 'build vaultsyncd-server-tests --example e2e-native-server first' >&2; exit 1; }
test -f apps/desktop/dist/index.html || { echo 'build desktop frontend first' >&2; exit 1; }
server_pid=''; tls_pid=''; tunnel_pid=''
cleanup() {
    touch "$artifact_dir/stop"
    for pid in "$tunnel_pid" "$tls_pid"; do
        if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    done
    if [ -n "$server_pid" ]; then
        for _ in $(seq 1 100); do kill -0 "$server_pid" 2>/dev/null || break; sleep .1; done
        if kill -0 "$server_pid" 2>/dev/null; then kill "$server_pid"; fi
        wait "$server_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT
python3 - "$artifact_dir" <<'PY'
import json,pathlib,sys
p=pathlib.Path(sys.argv[1]);(p/'scenario.json').write_text(json.dumps({'server_seed':[{'path':'server-seed.txt','content':'S172-native-server-seed'}]}))
PY
"$helper" "$artifact_dir/host-fixture.json" "$artifact_dir/stop" "$artifact_dir/scenario.json" > "$artifact_dir/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 600); do
    test -f "$artifact_dir/host-fixture.json" && break
    kill -0 "$server_pid" 2>/dev/null || { cat "$artifact_dir/server.log"; exit 1; }
    sleep .1
done
test -f "$artifact_dir/host-fixture.json"
host_port=$(python3 - "$artifact_dir" <<'PY'
import json,pathlib,sys,urllib.request,urllib.parse
p=pathlib.Path(sys.argv[1]);fixture=json.loads((p/'host-fixture.json').read_text());base=fixture['url'].rstrip('/');token=fixture['issued']['access_token'];vault=fixture['vault']
def request(method,path,body=None,raw=False):
    req=urllib.request.Request(base+path,data=json.dumps(body).encode() if body is not None else None,method=method,headers={'authorization':'Bearer '+token,'content-type':'application/json'})
    with urllib.request.urlopen(req) as response:
        content=response.read();return content if raw else json.loads(content)
identity=request('GET','/v1/capabilities')['server_id']
empty=pathlib.Path(fixture['work_dir']).parent/'empty-vault';empty.mkdir()
empty_vault=request('POST','/v1/admin/vaults',{'path':str(empty)})['vault_id']
snapshot=request('POST',f'/v1/vaults/{vault}/snapshots',{})['snapshot_id']
rows=request('GET',f'/v1/vaults/{vault}/snapshots/{snapshot}/entries?limit=100')['entries']
seed=next(row for row in rows if row['name']=='server-seed.txt')
path=f"/v1/vaults/{vault}/entries/{seed['entry_id']}/versions/{seed['version_id']}/content"
for _ in range(105):
    assert request('GET',path,raw=True)==b'S172-native-server-seed'
guest={'serverId':identity,'url':'https://127.0.0.1:18872','vault':vault,'emptyVault':empty_vault,'login':fixture['login'],'password':fixture['password']}
(p/'fixture.json').write_text(json.dumps(guest))
(p/'server-evidence.json').write_text(json.dumps({'source':'real btrfs server','seed':seed,'primed_downloads':105}))
print(urllib.parse.urlparse(base).port)
PY
)
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=S172-test-CA -keyout "$artifact_dir/ca.key" -out "$artifact_dir/ca.pem" > "$artifact_dir/certificate.log" 2>&1
openssl req -newkey rsa:2048 -nodes -subj /CN=127.0.0.1 -keyout "$artifact_dir/server.key" -out "$artifact_dir/server.csr" >> "$artifact_dir/certificate.log" 2>&1
printf 'subjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n' > "$artifact_dir/server.ext"
openssl x509 -req -in "$artifact_dir/server.csr" -CA "$artifact_dir/ca.pem" -CAkey "$artifact_dir/ca.key" -CAcreateserial -days 2 -extfile "$artifact_dir/server.ext" -out "$artifact_dir/server.pem" >> "$artifact_dir/certificate.log" 2>&1
python3 "$script_dir/s172-tls.py" "$artifact_dir" "$host_port" > "$artifact_dir/tls.log" 2>&1 &
tls_pid=$!
for _ in $(seq 1 100); do test -f "$artifact_dir/tls-port" && break; sleep .1; done
tls_port=$(cat "$artifact_dir/tls-port")
ssh -p "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR -o ExitOnForwardFailure=yes -N -R "127.0.0.1:18872:127.0.0.1:$tls_port" "$GUEST_USER@$GUEST_HOST" > "$artifact_dir/tunnel.log" 2>&1 &
tunnel_pid=$!
sleep .5
kill -0 "$tunnel_pid"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent -p vaultsyncd-desktop --features vaultsyncd-desktop/native-acceptance > "$artifact_dir/build.log" 2>&1
cp "$target_dir/$TARGET_TRIPLE/debug/vaultsyncd-agent.exe" "$target_dir/$TARGET_TRIPLE/debug/vaultsyncd-desktop.exe" "$script_dir/s172-activity.ps1" "$artifact_dir/"
python3 - "$artifact_dir" "$helper" <<'PY'
import hashlib,json,pathlib,subprocess,sys
p=pathlib.Path(sys.argv[1]);hashes={f.name:hashlib.sha256(f.read_bytes()).hexdigest() for f in p.glob('*.exe')};(p/'hashes.json').write_text(json.dumps(hashes))
files=subprocess.check_output(['git','ls-files','-co','--exclude-standard','-z']).decode().split('\0')
suffixes={'.rs','.toml','.json','.ts','.tsx','.js','.css','.html','.yaml','.sql','.lock'}
inputs={name:hashlib.sha256(pathlib.Path(name).read_bytes()).hexdigest() for name in files if name and pathlib.Path(name).is_file() and pathlib.Path(name).suffix in suffixes and not name.startswith(('docs/','agent/','.agents/'))}
(p/'source-inputs.json').write_text(json.dumps(inputs,sort_keys=True))
(p/'build-source.json').write_text(json.dumps({'commit':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'dirty':bool(subprocess.check_output(['git','status','--porcelain'],text=True)),'helper_sha256':hashlib.sha256(pathlib.Path(sys.argv[2]).read_bytes()).hexdigest(),'binaries':hashes,'source_inputs_sha256':hashlib.sha256((p/'source-inputs.json').read_bytes()).hexdigest()}))
PY
guest_dir="C:/vaultsyncd-lab/$run_id"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Path '$guest_dir' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/vaultsyncd-agent.exe" "$artifact_dir/vaultsyncd-desktop.exe" "$artifact_dir/s172-activity.ps1" "$artifact_dir/hashes.json" "$artifact_dir/fixture.json" "$artifact_dir/ca.pem"
set +e
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/s172-activity.ps1" > "$artifact_dir/native.log" 2>&1
result=$?
set -e
scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/*.json" "$GUEST_USER@$GUEST_HOST:$guest_dir/*.log" "$GUEST_USER@$GUEST_HOST:$guest_dir/*.txt" "$artifact_dir/" || true
cat "$artifact_dir/native.log"
echo "ARTIFACTS $artifact_dir"
echo "GUEST $guest_dir (isolated evidence retained; VM remains online)"
exit "$result"
