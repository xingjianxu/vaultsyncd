#!/usr/bin/env bash
# S195: real agent + CLI + HTTPS server, with three CFAPI binding generations on one NTFS root.
set -euo pipefail
umask 077
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
vault=$(repo_root)
cd "$vault"
dev_root=/data/vaultsyncd-dev
test -d "$dev_root" && test -w "$dev_root"
artifact_base=$(realpath -m "${VAULTSYNCD_NATIVE_ARTIFACTS:-$dev_root/S195-detached-path-reuse-20261111/process}")
[[ $artifact_base == "$dev_root/"* ]] || { echo 'evidence must stay below /data/vaultsyncd-dev' >&2; exit 1; }
run_id="S195-$(date +%s)-$$"
artifact_dir="$artifact_base/$run_id"
guest_dir="C:/vaultsyncd-lab/$run_id"
mkdir -p "$artifact_dir"
echo "ARTIFACTS $artifact_dir"
target_link=''
# The existing server helper uses its compile-time worktree target/s052-lab. Keep that
# ignored runtime entry on the authorized development volume, never in the source tree.
if [[ ! -e $vault/target && ! -L $vault/target ]]; then
  mkdir "$artifact_dir/lab-target"
  ln -s "$artifact_dir/lab-target" "$vault/target"
fi
if [[ -L $vault/target && $(readlink "$vault/target") == "$artifact_base"/S195-*/lab-target ]]; then
  target_link=$(readlink "$vault/target")
fi
lab_root=$(realpath -m "$vault/target/s052-lab")
[[ $lab_root == "$dev_root/"* ]] || { echo 'target/s052-lab must resolve below /data/vaultsyncd-dev' >&2; exit 1; }
build_base="${CARGO_TARGET_DIR:-$dev_root/cache/cargo-target/windows}"
[[ $(realpath -m "$build_base") == "$dev_root/"* ]] || { echo 'build cache must stay below /data/vaultsyncd-dev' >&2; exit 1; }
helper="$build_base/debug/examples/e2e-native-server"
test -x "$helper" || { echo 'build e2e-native-server in this worktree with the default devShell first' >&2; exit 1; }
server_pid=''; tls_pid=''; tunnel_pid=''; guest_started=0
cleanup() {
  local status=$? server_exit=0
  trap - EXIT
  set +e
  if (( guest_started )); then
    guest_ssh "powershell -NoProfile -Command \"Remove-Item -LiteralPath '$guest_dir/fixture.json' -Force -ErrorAction SilentlyContinue\"" >/dev/null 2>&1
    for pattern in '*.log' '*.json' 'exit.txt' 'error.txt'; do
      scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
        -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/$pattern" "$artifact_dir/" \
        >> "$artifact_dir/evidence-copy.log" 2>&1
    done
  fi
  # Lab::drop removes its runtime roots. Retain this run's observed server files before stopping.
  if [[ -s $artifact_dir/host-fixture.json ]]; then
    python3 - "$artifact_dir" <<'PY'
import json, pathlib, shutil, sys
out = pathlib.Path(sys.argv[1])
work = pathlib.Path(json.loads((out / "host-fixture.json").read_text())["work_dir"])
assert work.resolve().is_relative_to("/data/vaultsyncd-dev")
shutil.copytree(work, out / "server-work", dirs_exist_ok=True)
PY
    if (( $? != 0 )); then status=1; fi
  fi
  touch "$artifact_dir/stop"
  if [[ -n $server_pid ]]; then
    for _ in $(seq 1 150); do kill -0 "$server_pid" 2>/dev/null || break; sleep .1; done
    if kill -0 "$server_pid" 2>/dev/null; then kill "$server_pid"; fi
    wait "$server_pid"; server_exit=$?
    if (( server_exit != 0 && status == 0 )); then status=$server_exit; fi
  fi
  for pid in "$tunnel_pid" "$tls_pid"; do
    if [[ -n $pid ]]; then kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; fi
  done
  rm -f "$artifact_dir/fixture.json" "$artifact_dir/host-fixture.json" "$artifact_dir/ca.key" "$artifact_dir/server.key"
  if [[ -n $target_link && -L $vault/target && $(readlink "$vault/target") == "$target_link" ]]; then
    unlink "$vault/target" # Only the owned runtime symlink; retain its evidence destination.
  fi
  printf '{"exit":%s,"server_exit":%s,"guest":"%s"}\n' "$status" "$server_exit" "$guest_dir" > "$artifact_dir/run-status.json"
  echo "RESULT exit=$status ARTIFACTS $artifact_dir GUEST $guest_dir"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export CARGO_TARGET_DIR="$build_base"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent -p vaultsyncd-cli > "$artifact_dir/build.log" 2>&1
for name in agent cli; do cp "$build_base/$TARGET_TRIPLE/debug/vaultsyncd-$name.exe" "$artifact_dir/"; done
printf '{"server_seed":[{"path":"server-seed.txt","content":"S195-server-original"}]}\n' > "$artifact_dir/scenario.json"
"$helper" "$artifact_dir/host-fixture.json" "$artifact_dir/stop" "$artifact_dir/scenario.json" > "$artifact_dir/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 600); do
  [[ -s $artifact_dir/host-fixture.json ]] && break
  kill -0 "$server_pid"
  sleep .1
done
backend_port=$(python3 -c 'import json,sys,urllib.parse; print(urllib.parse.urlparse(json.load(open(sys.argv[1]))["url"]).port)' "$artifact_dir/host-fixture.json")
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=vaultsyncd-S195-CA \
  -keyout "$artifact_dir/ca.key" -out "$artifact_dir/ca.pem" > "$artifact_dir/certificate.log" 2>&1
openssl req -newkey rsa:2048 -nodes -subj /CN=127.0.0.1 -keyout "$artifact_dir/server.key" \
  -out "$artifact_dir/server.csr" >> "$artifact_dir/certificate.log" 2>&1
printf 'subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n' > "$artifact_dir/server.ext"
openssl x509 -req -days 1 -in "$artifact_dir/server.csr" -CA "$artifact_dir/ca.pem" -CAkey "$artifact_dir/ca.key" \
  -CAcreateserial -extfile "$artifact_dir/server.ext" -out "$artifact_dir/server.pem" >> "$artifact_dir/certificate.log" 2>&1
python3 "$script_dir/s081-tls.py" "$artifact_dir" "$backend_port" > "$artifact_dir/tls.log" 2>&1 &
tls_pid=$!
for _ in $(seq 1 100); do [[ -s $artifact_dir/tls-port ]] && break; kill -0 "$tls_pid"; sleep .1; done
tls_port=$(< "$artifact_dir/tls-port")
ssh -p "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
  -o BatchMode=yes -o LogLevel=ERROR -o ExitOnForwardFailure=yes -N \
  -R "127.0.0.1:18795:127.0.0.1:$tls_port" "$GUEST_USER@$GUEST_HOST" > "$artifact_dir/tunnel.log" 2>&1 &
tunnel_pid=$!
sleep .5
kill -0 "$tunnel_pid"
python3 - "$artifact_dir" "$script_dir" <<'PY'
import hashlib, json, pathlib, sys, urllib.request
out, scripts = map(pathlib.Path, sys.argv[1:])
host = json.loads((out / "host-fixture.json").read_text())
assert pathlib.Path(host["work_dir"]).resolve().is_relative_to("/data/vaultsyncd-dev")
with urllib.request.urlopen(host["url"].rstrip("/") + "/v1/capabilities", timeout=5) as response:
    server_id = json.load(response)["server_id"]
(out / "fixture.json").write_text(json.dumps({"serverId": server_id, "vault": host["vault"],
    "url": "https://127.0.0.1:18795", "login": host["login"], "password": host["password"]}))
script = out / "s195-rebind.ps1"
script.write_text((scripts / script.name).read_text(), encoding="utf-8-sig")
paths = [*out.glob("*.exe"), out / "ca.pem", script]
(out / "hashes.json").write_text(json.dumps({p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in paths}))
PY
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Path '$guest_dir' | Out-Null; icacls '$guest_dir' /inheritance:r /grant:r '${GUEST_USER}:(OI)(CI)F' 'SYSTEM:(OI)(CI)F' | Out-Null; exit \$LASTEXITCODE\""
guest_started=1
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/"*.exe "$artifact_dir/ca.pem" \
  "$artifact_dir/fixture.json" "$artifact_dir/hashes.json" "$artifact_dir/s195-rebind.ps1"
worker_status=0
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/s195-rebind.ps1" > "$artifact_dir/native.log" 2>&1 || worker_status=$?
printf '%s\n' "$worker_status" > "$artifact_dir/ssh-exit.txt"
(( worker_status == 0 )) || exit "$worker_status"
scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
  -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/result.json" "$artifact_dir/result.json"
python3 - "$artifact_dir" <<'PY'
import hashlib, json, pathlib, sys, time
out = pathlib.Path(sys.argv[1])
receipt = json.loads((out / "result.json").read_text(encoding="utf-8-sig"))
assert receipt["ok"] and len(set(receipt["bindings"])) == 3
work = pathlib.Path(json.loads((out / "host-fixture.json").read_text())["work_dir"])
expected = {"server-seed.txt": b"S195-server-original", "local-before.txt": b"S195-local-original",
            "upload-generation2.txt": b"S195-second-generation", "after-restart.txt": b"S195-after-restart",
            "upload-generation3.txt": b"S195-third-generation"}
deadline = time.monotonic() + 60
while time.monotonic() < deadline:
    if all((work / name).is_file() and (work / name).read_bytes() == content for name, content in expected.items()):
        break
    time.sleep(.25)
else:
    raise SystemExit("server bytes did not converge across all three binding generations")
summary = {name: hashlib.sha256((work / name).read_bytes()).hexdigest() for name in expected}
(out / "server-verification.json").write_text(json.dumps(summary, indent=2))
print("PASS server bytes", json.dumps(summary))
PY
echo "PASS S195 native three-generation rebind, guest $guest_dir"
