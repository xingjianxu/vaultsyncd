#!/usr/bin/env bash
# S203: production desktop shutdown with a real binding and locked unconfirmed data.
set -euo pipefail
umask 077
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
vault=$(repo_root)
cd "$vault"
test "$#" -eq 1 || { echo "usage: run-native-agent-exit.sh <production-exes>" >&2; exit 2; }
program=$(realpath "$1")
build_base="${CARGO_TARGET_DIR:-$vault/target}"
helper="$build_base/debug/examples/e2e-native-server"
test -x "$helper" || { echo 'build e2e-native-server in default devShell first' >&2; exit 1; }
run_id="s203-$(date +%s)-$$"
artifact_dir="${VAULTSYNCD_NATIVE_ARTIFACTS:-/data/vaultsyncd-dev/S203-agent-exit-20261111/binding}/$run_id"
case "$artifact_dir" in /data/vaultsyncd-dev/*) ;; *) echo "artifacts must be under /data/vaultsyncd-dev" >&2; exit 2 ;; esac
mkdir -p "$artifact_dir"
server_pid=''; tls_pid=''; tunnel_pid=''
cleanup() {
  touch "$artifact_dir/stop"
  if [ -n "$tunnel_pid" ]; then kill "$tunnel_pid" 2>/dev/null || true; wait "$tunnel_pid" 2>/dev/null || true; fi
  if [ -n "$tls_pid" ]; then kill "$tls_pid" 2>/dev/null || true; wait "$tls_pid" 2>/dev/null || true; fi
  if [ -n "$server_pid" ]; then
    for _ in $(seq 1 100); do kill -0 "$server_pid" 2>/dev/null || break; sleep .1; done
    if kill -0 "$server_pid" 2>/dev/null; then kill "$server_pid"; fi
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -f "$artifact_dir/ca.key" "$artifact_dir/server.key" "$artifact_dir/fixture.json"
}
trap cleanup EXIT
python3 -c 'import json,pathlib,sys; (pathlib.Path(sys.argv[1])/"scenario.json").write_text(json.dumps({"server_seed":[{"path":"server-seed.txt","content":"S203-server-before-shutdown"}]}))' "$artifact_dir"
"$helper" "$artifact_dir/host-fixture.json" "$artifact_dir/stop" "$artifact_dir/scenario.json" > "$artifact_dir/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 600); do test -f "$artifact_dir/host-fixture.json" && break; kill -0 "$server_pid" || { cat "$artifact_dir/server.log"; exit 1; }; sleep .1; done
host_port=$(python3 -c 'import json,sys,urllib.parse;print(urllib.parse.urlparse(json.load(open(sys.argv[1]))["url"]).port)' "$artifact_dir/host-fixture.json")
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=vaultsyncd-S203-CA -keyout "$artifact_dir/ca.key" -out "$artifact_dir/ca.pem" > "$artifact_dir/certificate.log" 2>&1
openssl req -newkey rsa:2048 -nodes -subj /CN=127.0.0.1 -keyout "$artifact_dir/server.key" -out "$artifact_dir/server.csr" >> "$artifact_dir/certificate.log" 2>&1
python3 -c 'import pathlib,sys;(pathlib.Path(sys.argv[1])/"server.ext").write_text("subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")' "$artifact_dir"
openssl x509 -req -days 1 -in "$artifact_dir/server.csr" -CA "$artifact_dir/ca.pem" -CAkey "$artifact_dir/ca.key" -CAcreateserial -extfile "$artifact_dir/server.ext" -out "$artifact_dir/server.pem" >> "$artifact_dir/certificate.log" 2>&1
python3 "$script_dir/s081-tls.py" "$artifact_dir" "$host_port" > "$artifact_dir/tls.log" 2>&1 &
tls_pid=$!
for _ in $(seq 1 100); do test -f "$artifact_dir/tls-port" && break; sleep .1; done
tls_port=$(cat "$artifact_dir/tls-port")
ssh -p "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR -o ExitOnForwardFailure=yes -N -R "127.0.0.1:18771:127.0.0.1:$tls_port" "$GUEST_USER@$GUEST_HOST" > "$artifact_dir/tunnel.log" 2>&1 &
tunnel_pid=$!
sleep .5
kill -0 "$tunnel_pid"
python3 - "$artifact_dir" <<'PYFIXTURE'
import json, pathlib, sys, urllib.request
p=pathlib.Path(sys.argv[1]); v=json.loads((p/'host-fixture.json').read_text())
f={'serverId':json.load(urllib.request.urlopen(v['url'].rstrip('/')+'/v1/capabilities'))['server_id'], 'vault':v['vault'], 'url':'https://127.0.0.1:18771', 'login':v['login'], 'password':v['password']}
(p/'fixture.json').write_text(json.dumps(f))
PYFIXTURE
export VAULTSYNCD_S203_FIXTURE_DIR="$artifact_dir"
export VAULTSYNCD_NATIVE_ARTIFACTS="$artifact_dir/native"
bash "$script_dir/run-native-desktop-settings.sh" "$program"
echo "ARTIFACTS $artifact_dir"
