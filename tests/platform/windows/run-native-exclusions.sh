#!/usr/bin/env bash
# S196: real agent + CLI/IPC + verified HTTPS + native CFAPI, in an interactive logon.
# Build the host helper once in this worktree with the default devShell:
# cargo build --locked -p vaultsyncd-server-tests --example e2e-native-server
# Then run in nix develop .#windows; VAULTSYNCD_NATIVE_SKIP_BUILD=1 reuses the
# caller's freshly built agent/CLI (never claims a reused build was rebuilt here).
set -euo pipefail
umask 077
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/common.sh"
test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1 || { echo 'use nix develop .#windows' >&2; exit 1; }
vault=$(repo_root)
cd "$vault"
dev_root=/data/vaultsyncd-dev
test -d "$dev_root" && test -w "$dev_root"
artifact_base=$(realpath -m "${VAULTSYNCD_NATIVE_ARTIFACTS:-$dev_root/S196-default-exclusions-20261111/evidence/native}")
[[ $artifact_base == "$dev_root/"* ]] || { echo 'evidence must stay below /data/vaultsyncd-dev' >&2; exit 1; }
export TMPDIR="${TMPDIR:-$dev_root/S196-default-exclusions-20261111/tmp}"
[[ $(realpath -m "$TMPDIR") == "$dev_root/"* ]] || { echo 'tmp must stay below /data/vaultsyncd-dev' >&2; exit 1; }
mkdir -p "$TMPDIR"
run_id="S196-$(date +%s)-$$"
artifact_dir="$artifact_base/$run_id"
guest_dir="C:/vaultsyncd-lab/$run_id"
mkdir -p "$artifact_dir"
echo "ARTIFACTS $artifact_dir"
# Fail without touching the user's product or desktop session. The worker repeats
# this check inside its real interactive logon before claiming the single writer.
guest_ssh 'powershell -NoProfile -Command "if (@(Get-Process vaultsyncd-agent,vaultsyncd-desktop -ErrorAction SilentlyContinue).Count) { throw '\''preserve existing product processes'\'' }; quser; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }"' > "$artifact_dir/preflight.log" 2>&1
build_base="${CARGO_TARGET_DIR:-$dev_root/cache/cargo-target/windows}"
[[ $(realpath -m "$build_base") == "$dev_root/"* ]] || { echo 'build cache must stay below /data/vaultsyncd-dev' >&2; exit 1; }
helper="$build_base/debug/examples/e2e-native-server"
test -x "$helper" || { echo 'build e2e-native-server in this worktree with the default devShell first' >&2; exit 1; }
# A helper from a removed worktree would create its compile-time Lab outside this
# run's root. Require local provenance, not just a coincidentally cached binary.
grep -Fq "$vault/tests/server-integration/examples/e2e-native-server.rs" "$helper.d" || {
  echo 'e2e-native-server must be built in the current worktree' >&2; exit 1;
}
target_link=''; server_pid=''; tls_pid=''; tunnel_pid=''; guest_started=0
cleanup() {
  local status=$? server_exit=0
  trap - EXIT
  set +e
  if (( guest_started )); then
    guest_ssh "powershell -NoProfile -Command \"Remove-Item -LiteralPath '$guest_dir/fixture.json' -Force -ErrorAction SilentlyContinue\"" >/dev/null 2>&1
    for pattern in '*.log' '*.json' 'exit.txt' 'error.txt' 'rules.txt'; do
      scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
        -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/$pattern" "$artifact_dir/" \
        >> "$artifact_dir/evidence-copy.log" 2>&1
    done
  fi
  # The host Lab removes its roots on drop; archive bytes while it still runs.
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
  if [[ -n $target_link && -L $vault/target && $(readlink "$vault/target") == "$target_link" ]]; then unlink "$vault/target"; fi
  printf '{"exit":%s,"server_exit":%s,"guest":"%s"}\n' "$status" "$server_exit" "$guest_dir" > "$artifact_dir/run-status.json"
  echo "RESULT exit=$status ARTIFACTS $artifact_dir GUEST $guest_dir"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# Only create/remove our own ignored runtime link, never another task's target.
if [[ ! -e $vault/target && ! -L $vault/target ]]; then
  mkdir "$artifact_dir/lab-target"
  target_link="$artifact_dir/lab-target"
  ln -s "$target_link" "$vault/target"
fi
[[ $(realpath -m "$vault/target/s052-lab") == "$dev_root/"* ]] || { echo 'target/s052-lab must resolve below /data/vaultsyncd-dev' >&2; exit 1; }
export CARGO_TARGET_DIR="$build_base"
if [[ ${VAULTSYNCD_NATIVE_SKIP_BUILD:-0} == 1 ]]; then
  printf 'Reused caller-built agent/CLI from %s\n' "$build_base" > "$artifact_dir/build.log"
else
  export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
  cargo xwin build --locked --target "$TARGET_TRIPLE" -p vaultsyncd-agent -p vaultsyncd-cli > "$artifact_dir/build.log" 2>&1
fi
for name in agent cli; do cp "$build_base/$TARGET_TRIPLE/debug/vaultsyncd-$name.exe" "$artifact_dir/"; done
python3 - "$artifact_dir" <<'PY'
import json, pathlib, sys
out = pathlib.Path(sys.argv[1])
seeds = {"server.xlsx": "S196-server-workbook", "~$remote-default.xlsx": "S196-remote-lock",
         "~$reconcile.xlsx": "S196-remote-original"}
(out / "scenario.json").write_text(json.dumps({"server_seed": [{"path": k, "content": v} for k, v in seeds.items()]}))
PY
"$helper" "$artifact_dir/host-fixture.json" "$artifact_dir/stop" "$artifact_dir/scenario.json" > "$artifact_dir/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 600); do [[ -s $artifact_dir/host-fixture.json ]] && break; kill -0 "$server_pid"; sleep .1; done
backend_port=$(python3 -c 'import json,sys,urllib.parse; print(urllib.parse.urlparse(json.load(open(sys.argv[1]))["url"]).port)' "$artifact_dir/host-fixture.json")
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=vaultsyncd-S196-CA \
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
# Two run-private reverse tunnels: HTTPS through the TLS proxy for the product channel, and
# the loopback backend port for pinned-free test observation inside the guest.
ssh -p "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
  -o BatchMode=yes -o LogLevel=ERROR -o ExitOnForwardFailure=yes -N \
  -R "127.0.0.1:18796:127.0.0.1:$tls_port" -R "127.0.0.1:18797:127.0.0.1:$backend_port" \
  "$GUEST_USER@$GUEST_HOST" > "$artifact_dir/tunnel.log" 2>&1 &
tunnel_pid=$!
sleep .5
kill -0 "$tunnel_pid"
python3 - "$artifact_dir" "$script_dir" <<'PY'
import base64, hashlib, json, pathlib, subprocess, sys, urllib.request
out, scripts = map(pathlib.Path, sys.argv[1:])
host = json.loads((out / "host-fixture.json").read_text())
assert pathlib.Path(host["work_dir"]).resolve().is_relative_to("/data/vaultsyncd-dev")
with urllib.request.urlopen(host["url"].rstrip("/") + "/v1/capabilities", timeout=5) as response:
    server_id = json.load(response)["server_id"]
(out / "fixture.json").write_text(json.dumps({"serverId": server_id, "vault": host["vault"],
    "url": "https://127.0.0.1:18796", "login": host["login"], "password": host["password"],
    "token": host["issued"]["access_token"], "apiUrl": "http://127.0.0.1:18797"}))
script = out / "s196-exclusions.ps1"
script.write_text((scripts / script.name).read_text(), encoding="utf-8-sig")
paths = [*out.glob("*.exe"), out / "ca.pem", script]
(out / "hashes.json").write_text(json.dumps({p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in paths}))
PY
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Path '$guest_dir' | Out-Null; icacls '$guest_dir' /inheritance:r /grant:r '${GUEST_USER}:(OI)(CI)F' 'SYSTEM:(OI)(CI)F' | Out-Null; exit \$LASTEXITCODE\""
guest_started=1
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/"*.exe "$artifact_dir/ca.pem" \
  "$artifact_dir/fixture.json" "$artifact_dir/hashes.json" "$artifact_dir/s196-exclusions.ps1"
worker_status=0
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/s196-exclusions.ps1" > "$artifact_dir/native.log" 2>&1 || worker_status=$?
printf '%s\n' "$worker_status" > "$artifact_dir/ssh-exit.txt"
(( worker_status == 0 )) || exit "$worker_status"
scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
  -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/result.json" "$artifact_dir/result.json"
python3 - "$artifact_dir" <<'PY'
import hashlib, json, pathlib, sys
out = pathlib.Path(sys.argv[1])
receipt = json.loads((out / "result.json").read_text(encoding="utf-8-sig"))
for key in ("ok", "defaults", "additional", "cleared", "negation", "restart", "reconciliation", "retained", "cfapi", "fullMode",
            "initialRemoteOnly", "deletedTrackedStaysFenced", "bothEditsPreserved"):
    assert receipt[key] is True, key
work = pathlib.Path(json.loads((out / "host-fixture.json").read_text())["work_dir"])
expected = {"server.xlsx": b"S196-server-workbook", "local.xlsx": b"S196-local-workbook",
    "~$remote-default.xlsx": b"S196-remote-lock", "~$include-local.xlsx": b"S196-included-local-lock",
    "local.extra.xlsx": b"S196-extra-workbook"}
for phase in ("default", "additional", "cleared", "included", "retained", "excluded-local-deleted", "remote-edited",
              "restart", "reconciled", "before-both-edits", "both-edited", "both-preserved"):
    expected[f"barrier-{phase}.txt"] = f"S196-{phase}".encode()
for name, content in expected.items():
    assert (work / name).read_bytes() == content, name
assert not (work / "~$local-default.xlsx").exists(), "default local lock uploaded"
summary = {name: hashlib.sha256((work / name).read_bytes()).hexdigest() for name in expected}
# Conflict naming is an implementation detail; prove exact bytes from both edits
# exist in real server files, in addition to the guest's committed snapshot proof.
for content in (b"S196-unique-local-edit-while-excluded", b"S196-concurrent-remote-edit-while-excluded"):
    matches = [p for p in work.rglob("*") if p.is_file() and p.read_bytes() == content]
    assert matches, f"lost exclusion-period bytes: {content!r}"
    summary.update({str(p.relative_to(work)): hashlib.sha256(content).hexdigest() for p in matches})
(out / "server-verification.json").write_text(json.dumps(summary, indent=2))
print("PASS exact server bytes", json.dumps(summary))
PY
echo "PASS S196 native defaults/additional/clear/negation/restart/reconcile/deletion/conflict-preservation, guest $guest_dir"
