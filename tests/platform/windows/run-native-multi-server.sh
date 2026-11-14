#!/usr/bin/env bash
# Windows native acceptance with two real, isolated Linux servers and a controlled outage.
set -euo pipefail
umask 077
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
vault=$(repo_root)
cd "$vault"
dev_root=/data/vaultsyncd-dev
test -d "$dev_root" && test -w "$dev_root"
lab_root=$(realpath -m "$vault/target/s052-lab")
[[ $lab_root == "$dev_root/"* ]] || { echo 'point the test worktree target at the development root first' >&2; exit 1; }
build_base="${CARGO_TARGET_DIR:-$dev_root/cache/cargo-target/windows}"
helper="${VAULTSYNCD_NATIVE_SERVER:-$dev_root/cache/cargo-target/debug/examples/e2e-native-server}"
test -x "$helper" || { echo 'build vaultsyncd-server-tests --example e2e-native-server in the default devShell' >&2; exit 1; }
run_id="S191-$(date +%s)-$$"
artifact_dir="$dev_root/$run_id"
guest_dir="C:/vaultsyncd-lab/$run_id"
mkdir "$artifact_dir"
echo "ARTIFACTS $artifact_dir"
pnpm --filter @vaultsyncd/desktop build > "$artifact_dir/frontend-build.log" 2>&1

export CARGO_TARGET_DIR="$build_base"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent -p vaultsyncd-cli \
  -p vaultsyncd-desktop --features vaultsyncd-desktop/native-acceptance > "$artifact_dir/build.log" 2>&1
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent --example s191_cleanup >> "$artifact_dir/build.log" 2>&1
cp "$build_base/$TARGET_TRIPLE/debug/examples/s191_cleanup.exe" "$artifact_dir/"
cargo xwin test --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent --lib --no-run --message-format=json \
  > "$artifact_dir/test-build.json" 2>> "$artifact_dir/build.log"
python3 - "$artifact_dir/test-build.json" "$artifact_dir/s191-agent-tests.exe" <<'PY'
import json, pathlib, shutil, sys

for line in pathlib.Path(sys.argv[1]).read_text().splitlines():
    message = json.loads(line)
    if message.get("reason") == "compiler-artifact" and message.get("profile", {}).get("test") and message.get("executable"):
        shutil.copy2(message["executable"], sys.argv[2])
        break
else:
    raise SystemExit("missing agent test executable")
PY
for name in agent cli desktop; do
  cp "$build_base/$TARGET_TRIPLE/debug/vaultsyncd-$name.exe" "$artifact_dir/"
done
if [[ -n ${VAULTSYNCD_LEGACY_CLIENT_DIR:-} ]]; then
  mkdir "$artifact_dir/legacy"
  for name in agent cli; do
    cp "$VAULTSYNCD_LEGACY_CLIENT_DIR/vaultsyncd-$name.exe" "$artifact_dir/legacy/"
  done
  cp "$VAULTSYNCD_LEGACY_CLIENT_DIR/build-info.txt" "$artifact_dir/legacy/"
fi

declare -A backend_pid tls_pid tunnel_pid
guest_started=0
worker_pid=''
cleanup() {
  if [[ -n $worker_pid ]]; then kill "$worker_pid" 2>/dev/null || true; wait "$worker_pid" 2>/dev/null || true; fi
  if (( guest_started )); then
    guest_ssh "powershell -NoProfile -Command \"Remove-Item -LiteralPath '$guest_dir/fixture.json' -Force -ErrorAction SilentlyContinue\"" >/dev/null 2>&1 || true
  fi
  for side in a b; do
    dir="$artifact_dir/$side"
    [[ -d $dir ]] || continue
    touch "$dir/stop"
    if [[ -n ${tls_pid[$side]:-} ]]; then kill -CONT "${tls_pid[$side]}" 2>/dev/null || true; fi
    for pid in "${tunnel_pid[$side]:-}" "${tls_pid[$side]:-}"; do
      if [[ -n $pid ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    done
    if [[ -n ${backend_pid[$side]:-} ]]; then
      for _ in $(seq 1 150); do kill -0 "${backend_pid[$side]}" 2>/dev/null || break; sleep .2; done
      kill "${backend_pid[$side]}" 2>/dev/null || true
      wait "${backend_pid[$side]}" 2>/dev/null || true
    fi
    rm -f "$dir/host-fixture.json" "$dir/ca.key" "$dir/server.key"
  done
  rm -f "$artifact_dir/fixture.json"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

start_server() {
  local side=$1 guest_port=$2 dir="$artifact_dir/$1" backend_port tls_port
  mkdir "$dir"
  printf '{"server_seed":[{"path":"server-seed.txt","content":"S191-server-%s-original"}]}\n' "$side" > "$dir/scenario.json"
  "$helper" "$dir/host-fixture.json" "$dir/stop" "$dir/scenario.json" > "$dir/server.log" 2>&1 &
  backend_pid[$side]=$!
  for _ in $(seq 1 300); do
    [[ -s $dir/host-fixture.json ]] && break
    kill -0 "${backend_pid[$side]}"
    sleep .2
  done
  backend_port=$(python3 -c 'import json,sys,urllib.parse; print(urllib.parse.urlparse(json.load(open(sys.argv[1]))["url"]).port)' "$dir/host-fixture.json")
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=vaultsyncd-S191-$side" -keyout "$dir/ca.key" -out "$dir/ca.pem" > "$dir/certificate.log" 2>&1
  openssl req -newkey rsa:2048 -nodes -subj /CN=127.0.0.1 -keyout "$dir/server.key" -out "$dir/server.csr" >> "$dir/certificate.log" 2>&1
  printf 'subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n' > "$dir/server.ext"
  openssl x509 -req -days 1 -in "$dir/server.csr" -CA "$dir/ca.pem" -CAkey "$dir/ca.key" -CAcreateserial -extfile "$dir/server.ext" -out "$dir/server.pem" >> "$dir/certificate.log" 2>&1
  python3 "$script_dir/s081-tls.py" "$dir" "$backend_port" > "$dir/tls.log" 2>&1 &
  tls_pid[$side]=$!
  for _ in $(seq 1 100); do [[ -s $dir/tls-port ]] && break; kill -0 "${tls_pid[$side]}"; sleep .1; done
  tls_port=$(cat "$dir/tls-port")
  ssh -p "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
    -o BatchMode=yes -o LogLevel=ERROR -o ExitOnForwardFailure=yes -N \
    -R "127.0.0.1:$guest_port:127.0.0.1:$tls_port" "$GUEST_USER@$GUEST_HOST" > "$dir/tunnel.log" 2>&1 &
  tunnel_pid[$side]=$!
  sleep .5
  kill -0 "${tunnel_pid[$side]}"
  cp "$dir/ca.pem" "$artifact_dir/ca-$side.pem"
}

start_server a 18781
start_server b 18782
python3 - "$artifact_dir" "$script_dir" <<'PY'
import hashlib, json, pathlib, sys, urllib.request

out, scripts = map(pathlib.Path, sys.argv[1:])
fixture = {}
for side, port in (("a", 18781), ("b", 18782)):
    host = json.loads((out / side / "host-fixture.json").read_text())
    with urllib.request.urlopen(host["url"].rstrip("/") + "/v1/capabilities", timeout=5) as response:
        server_id = json.load(response)["server_id"]
    fixture[side] = {"serverId": server_id, "url": f"https://127.0.0.1:{port}",
                     "vault": host["vault"], "login": host["login"], "password": host["password"]}
assert fixture["a"]["serverId"] != fixture["b"]["serverId"], "fixture identities must differ"
assert fixture["a"]["vault"] != fixture["b"]["vault"], "fixture Vaults must differ"
(out / "fixture.json").write_text(json.dumps(fixture))
script = "s191-multi-server.ps1"
(out / script).write_text((scripts / script).read_text(), encoding="utf-8-sig")
paths = [*out.glob("*.exe"), *out.glob("ca-*.pem"), out / script]
paths.extend((out / "legacy").glob("*.exe"))
(out / "hashes.json").write_text(json.dumps({p.relative_to(out).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest() for p in paths}))
PY

guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Path '$guest_dir' | Out-Null; icacls '$guest_dir' /inheritance:r /grant:r '${GUEST_USER}:(OI)(CI)F' 'SYSTEM:(OI)(CI)F' | Out-Null; exit \$LASTEXITCODE\""
guest_started=1
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/"*.exe "$artifact_dir/"ca-*.pem \
  "$artifact_dir/fixture.json" "$artifact_dir/hashes.json" "$artifact_dir/s191-multi-server.ps1"
if [[ -d $artifact_dir/legacy ]]; then
  GUEST_TARGET_DIR="$guest_dir" guest_scp -r "$artifact_dir/legacy"
fi
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/s191-multi-server.ps1" > "$artifact_dir/native.log" 2>&1 &
worker_pid=$!

guest_marker() {
  guest_ssh "powershell -NoProfile -Command \"if (Test-Path -LiteralPath '$guest_dir/$1') { exit 0 }; exit 1\""
}
acknowledge() {
  guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType File -Path '$guest_dir/$1' | Out-Null\""
}

network_state=ready
while kill -0 "$worker_pid" 2>/dev/null; do
  case $network_state in
    ready)
      if guest_marker block-a; then
        # Leave the listener/tunnel open so an in-flight request to A really stalls.
        kill -STOP "${tls_pid[a]}"
        acknowledge a-offline
        network_state=offline
      fi ;;
    offline)
      if guest_marker unblock-a; then
        kill -CONT "${tls_pid[a]}"
        acknowledge a-online
        network_state=restored
      fi ;;
    restored) break ;;
  esac
  sleep .5
done
worker_status=0
wait "$worker_pid" || worker_status=$?
worker_pid=''
cat "$artifact_dir/native.log"
scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
  -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/*.log" "$artifact_dir/" || true
for pattern in 'cleanup-*.json' 'webview-*.json'; do
  scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
    -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/$pattern" "$artifact_dir/" || true
done
(( worker_status == 0 )) || exit "$worker_status"
[[ $network_state == restored ]]

scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
  -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/result.json" "$artifact_dir/result.json"
python3 - "$artifact_dir" <<'PY'
import hashlib, json, pathlib, sys

out = pathlib.Path(sys.argv[1])
receipt = json.loads((out / "result.json").read_text(encoding="utf-8-sig"))
assert receipt["ok"]
for side, other in (("a", "b"), ("b", "a")):
    host = json.loads((out / side / "host-fixture.json").read_text())
    work = pathlib.Path(host["work_dir"])
    own_name = f"upload-{side}.txt"
    assert (work / own_name).read_bytes() == f"S191-upload-{side}".encode()
    assert (work / "server-seed.txt").read_bytes() == f"S191-server-{side}-original".encode()
    assert not (work / f"upload-{other}.txt").exists(), "cross-Vault upload"
    for filename, content in {"while-a-offline.txt": b"S191-B-while-A-offline",
                              "after-restart.txt": b"S191-B-after-restart"}.items():
        assert (work / filename).exists() == (side == "b")
        if side == "b":
            assert (work / filename).read_bytes() == content
    print(f"PASS server {side}: exact upload {hashlib.sha256((work / own_name).read_bytes()).hexdigest()}")
PY
echo "PASS Windows multi-server acceptance, guest $guest_dir"
