#!/usr/bin/env bash
# Invoke in nix develop .#windows; the real VM remains online.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/../platform/windows/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1
vault=$(repo_root);cd "$vault"
build_base="${CARGO_TARGET_DIR:-$vault/target}"
helper="$build_base/debug/examples/e2e-native-server"
test -x "$helper" || { echo 'build e2e-native-server in the default devShell first' >&2;exit 1; }
run_id="s084-$(date +%s)-$$"
artifact_dir="$vault/artifacts/S084/$run_id";mkdir -p "$artifact_dir"
server_pid='';proxy_pid='';tunnel_pid=''
cleanup() {
    touch "$artifact_dir/stop"
    if [ -n "$tunnel_pid" ];then kill "$tunnel_pid" 2>/dev/null || true;wait "$tunnel_pid" 2>/dev/null || true;fi
    if [ -n "$proxy_pid" ];then kill "$proxy_pid" 2>/dev/null || true;wait "$proxy_pid" 2>/dev/null || true;fi
    if [ -n "$server_pid" ];then
        for _ in $(seq 1 100);do kill -0 "$server_pid" 2>/dev/null || break;sleep .1;done
        if kill -0 "$server_pid" 2>/dev/null;then kill "$server_pid";fi
        wait "$server_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT
python3 -c 'import json,pathlib,sys;p=pathlib.Path(sys.argv[1]);(p/"scenario.json").write_text(json.dumps({"server_seed":[{"path":"dirty.txt","content":"S084 acknowledged baseline"},{"path":"intent.txt","content":"S084 original intent content"}]}))' "$artifact_dir"
"$helper" "$artifact_dir/host-fixture.json" "$artifact_dir/stop" "$artifact_dir/scenario.json" > "$artifact_dir/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 600);do test -f "$artifact_dir/host-fixture.json" && break;kill -0 "$server_pid" || { cat "$artifact_dir/server.log";exit 1; };sleep .1;done
# Moved target caches can contain a helper whose embedded lab base points at an old tree.
python3 -c 'import json,pathlib,sys;vault=pathlib.Path.cwd().resolve();work=pathlib.Path(json.load(open(sys.argv[1]))["work_dir"]).resolve();assert work.is_relative_to(vault/"target"),"stale helper source path: rebuild e2e-native-server in this worktree before native execution"' "$artifact_dir/host-fixture.json"
host_port=$(python3 -c 'import json,sys,urllib.parse;print(urllib.parse.urlparse(json.load(open(sys.argv[1]))["url"]).port)' "$artifact_dir/host-fixture.json")
python3 "$script_dir/s084-upload-fault-proxy.py" "$artifact_dir" "$host_port" > "$artifact_dir/proxy.log" 2>&1 &
proxy_pid=$!
for _ in $(seq 1 100);do test -f "$artifact_dir/proxy-port" && break;sleep .1;done
proxy_port=$(cat "$artifact_dir/proxy-port")
ssh -p "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR -o ExitOnForwardFailure=yes -N -R "127.0.0.1:18774:127.0.0.1:$proxy_port" "$GUEST_USER@$GUEST_HOST" > "$artifact_dir/tunnel.log" 2>&1 &
tunnel_pid=$!;sleep .5;kill -0 "$tunnel_pid"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent
cargo xwin test --locked --target "$TARGET_TRIPLE" -p vaultsyncd-windows-tests --test unuploaded_recovery --no-run --message-format=json > "$artifact_dir/build.json"
python3 - "$artifact_dir" <<'PY'
import hashlib,json,pathlib,shutil,sys
p=pathlib.Path(sys.argv[1]);vault=pathlib.Path.cwd();build=pathlib.Path(__import__('os').environ.get('CARGO_TARGET_DIR',vault/'target'))
shutil.copyfile(build/'x86_64-pc-windows-msvc/debug/vaultsyncd-agent.exe',p/'vaultsyncd-agent.exe')
for line in (p/'build.json').read_text().splitlines():
    r=json.loads(line)
    if r.get('reason')=='compiler-artifact' and r.get('executable') and r.get('profile',{}).get('test'):
        shutil.copyfile(r['executable'],p/'s084-native-tests.exe');break
else:raise RuntimeError('no native test executable')
fixture=json.loads((p/'host-fixture.json').read_text());fixture['url']='http://127.0.0.1:18774'
(p/'fixture.json').write_text(json.dumps(fixture))
(p/'hashes.json').write_text(json.dumps({f.name:hashlib.sha256(f.read_bytes()).hexdigest() for f in p.glob('*.exe')}))
PY
cp "$script_dir/s084-run.ps1" "$artifact_dir/"
guest_dir="C:/vaultsyncd-lab/$run_id"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory '$guest_dir' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/vaultsyncd-agent.exe" "$artifact_dir/s084-native-tests.exe" "$artifact_dir/s084-run.ps1" "$artifact_dir/fixture.json" "$artifact_dir/hashes.json"
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/s084-run.ps1" | tee "$artifact_dir/native.log"
scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/result.json" "$artifact_dir/result.json"
guest_run=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["run"].replace("\\","/"))' "$artifact_dir/result.json")
for directory in recovery-package manual-recovered;do
    scp -r -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_run/$directory" "$artifact_dir/$directory"
done
for log in first-agent.log restarted-agent.log;do
    scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_run/$log" "$artifact_dir/$log"
done
python3 "$script_dir/verify-recovery-evidence.py" "$artifact_dir"
python3 -c 'import json,pathlib,sys;p=pathlib.Path(sys.argv[1]);f=json.loads((p/"host-fixture.json").read_text());w=pathlib.Path(f["work_dir"]);assert (w/"dirty.txt").read_bytes()==b"S084 acknowledged baseline";assert (w/"remote-renamed.txt").read_bytes()==b"S084 original intent content";assert not (w/"new-未上传.txt").exists();assert (p/"dropped-parts.log").stat().st_size>0;print("PASS server baseline unchanged; actual part disconnect and Move intent scenario")' "$artifact_dir"
echo "ARTIFACTS $artifact_dir"
echo "GUEST $guest_dir (native NTFS data and recovery packages retained; VM stays online)"
