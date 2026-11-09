#!/usr/bin/env bash
# Real React forms -> Tauri -> agent IPC -> HTTPS -> real isolated Linux server.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
vault=$(repo_root)
cd "$vault"
test -f apps/desktop/dist/index.html
build_base="${CARGO_TARGET_DIR:-$vault/target}"
helper="$build_base/debug/examples/e2e-native-server"
test -x "$helper" || { echo 'build e2e-native-server in default devShell first' >&2; exit 1; }
run_id="s082-$(date +%s)-$$"
artifact_dir="${VAULTSYNCD_NATIVE_ARTIFACTS:-/data/vaultsyncd-temp/S082-native/evidence}/$run_id"
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
  rm -f "$artifact_dir/ca.key" "$artifact_dir/server.key"
}
trap cleanup EXIT
python3 -c 'import json,pathlib,sys; (pathlib.Path(sys.argv[1])/"scenario.json").write_text(json.dumps({"server_seed":[{"path":"server-seed.txt","content":"S081-server-before-import"}]}))' "$artifact_dir"
"$helper" "$artifact_dir/host-fixture.json" "$artifact_dir/stop" "$artifact_dir/scenario.json" > "$artifact_dir/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 600); do test -f "$artifact_dir/host-fixture.json" && break; kill -0 "$server_pid" || { cat "$artifact_dir/server.log"; exit 1; }; sleep .1; done
host_port=$(python3 -c 'import json,sys,urllib.parse;print(urllib.parse.urlparse(json.load(open(sys.argv[1]))["url"]).port)' "$artifact_dir/host-fixture.json")
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=vaultsyncd-S081-CA -keyout "$artifact_dir/ca.key" -out "$artifact_dir/ca.pem" > "$artifact_dir/certificate.log" 2>&1
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
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent -p vaultsyncd-desktop --features vaultsyncd-desktop/native-acceptance
cp "$build_base/$TARGET_TRIPLE/debug/vaultsyncd-agent.exe" "$build_base/$TARGET_TRIPLE/debug/vaultsyncd-desktop.exe" "$artifact_dir/"
cp "$script_dir/s082-desktop.ps1" "$artifact_dir/"
python3 -c 'import pathlib,sys; p=pathlib.Path(sys.argv[1])/"s082-desktop.ps1"; p.write_text(p.read_text(),encoding="utf-8-sig")' "$artifact_dir"
python3 -c 'import hashlib,json,pathlib,sys,uuid; p=pathlib.Path(sys.argv[1]); v=json.loads((p/"host-fixture.json").read_text()); (p/"fixture.json").write_text(json.dumps({"serverId":uuid.uuid4().hex,"vault":v["vault"],"url":"https://127.0.0.1:18771","login":v["login"],"password":v["password"]})); (p/"hashes.json").write_text(json.dumps({f.name:hashlib.sha256(f.read_bytes()).hexdigest() for f in p.glob("*.exe")}))' "$artifact_dir"
guest_dir="C:/vaultsyncd-lab/$run_id"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Path '$guest_dir' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/vaultsyncd-agent.exe" "$artifact_dir/vaultsyncd-desktop.exe" "$artifact_dir/s082-desktop.ps1" "$artifact_dir/fixture.json" "$artifact_dir/hashes.json" "$artifact_dir/ca.pem"
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/s082-desktop.ps1" | tee "$artifact_dir/native.log"
# Detach is local-only: server retains the first confirmed bytes and receives no post-detach file.
python3 -c 'import json,pathlib,sys; p=pathlib.Path(sys.argv[1]);v=json.loads((p/"host-fixture.json").read_text());w=pathlib.Path(v["work_dir"]); assert (w/"server-seed.txt").read_bytes()==b"S081-server-before-import"; assert (w/"local-only.txt").read_bytes()==b"S081-local-before-import";assert not (w/"after-detach.txt").exists();print("PASS detach retains server bytes and stops local management")' "$artifact_dir"
echo "ARTIFACTS $artifact_dir"
echo "GUEST $guest_dir (VM remains online)"
