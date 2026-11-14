#!/usr/bin/env bash
# Persistent Windows lab setup against the public development origin; uses existing production EXEs.
set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$script_dir/common.sh"
if [ "$#" -ne 3 ]; then
  echo 'usage: run-dev-server.sh <complete-client-directory> <fixture-directory-with-fixture.json-and-password> <evidence-directory>' >&2
  exit 2
fi
dev_root=$(realpath -e /data/vaultsyncd-dev)
test "$dev_root" = /data/vaultsyncd-dev && test ! -L /data/vaultsyncd-dev || { echo 'development root must be the real authorized directory' >&2; exit 2; }
program=$(realpath -e "$1")
fixture_dir=$(realpath -e "$2")
artifact_dir=$(realpath -m "$3")
test "$fixture_dir" = "$(realpath -ms "$2")" && test "$artifact_dir" = "$(realpath -ms "$3")" || { echo 'fixture/evidence paths must not traverse symbolic links' >&2; exit 2; }
case "$fixture_dir/" in /data/vaultsyncd-dev/*/) ;; *) echo 'fixture must be under /data/vaultsyncd-dev' >&2; exit 2 ;; esac
case "$artifact_dir/" in /data/vaultsyncd-dev/*/) ;; *) echo 'evidence must be under /data/vaultsyncd-dev' >&2; exit 2 ;; esac
test -f "$fixture_dir/password" && test ! -L "$fixture_dir/password" && test ! -L "$fixture_dir/fixture.json" || { echo 'fixture and password must be ordinary files' >&2; exit 2; }
test "$(stat -c %u "$fixture_dir/password")" = "$(id -u)" || { echo 'password file must belong to the invoking user' >&2; exit 2; }
test "$(stat -c %a "$fixture_dir/password")" = 600 || { echo 'password file must have mode 600' >&2; exit 2; }
mkdir -p "$artifact_dir"
python3 - "$program" "$fixture_dir" "$artifact_dir" "$script_dir/dev-server.ps1" <<'PY'
import hashlib, json, pathlib, sys
program, fixture_dir, destination, script = map(pathlib.Path, sys.argv[1:])
fixture = json.loads((fixture_dir/'fixture.json').read_text())
assert fixture['url'] == 'https://wlab.132140.xyz:15401'
assert fixture['runId'].isalnum(), 'runId must be alphanumeric'
required = {'vaultsyncd-agent.exe','vaultsyncd-cli.exe','vaultsyncd-desktop.exe','vaultsyncd-session.exe','installed-files.json','zip-files.json','Start vaultsyncd.cmd','Stop vaultsyncd.cmd','README-ZIP.txt'}
files = [f for f in program.iterdir() if f.is_file()]
assert required <= {f.name for f in files}, 'complete client directory required'
(destination/'hashes.json').write_text(json.dumps({f.name:hashlib.sha256(f.read_bytes()).hexdigest() for f in files}))
(destination/'dev-server.ps1').write_text(script.read_text(), encoding='utf-8-sig')
(destination/'fixture.json').write_text(json.dumps(fixture))
PY
run_id="s207-dev-$(date +%s)-$$"
guest_dir="C:/vaultsyncd-lab/$run_id"
guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Path '$guest_dir/program' | Out-Null\""
GUEST_TARGET_DIR="$guest_dir/program" guest_scp "$program/"*
GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifact_dir/dev-server.ps1" "$artifact_dir/fixture.json" "$artifact_dir/hashes.json"
# Restrict the staging directory before transmitting the short-lived secret.
guest_ssh "icacls $guest_dir /inheritance:r /grant:r \"Quickemu:(OI)(CI)F\" \"SYSTEM:(OI)(CI)F\""
GUEST_TARGET_DIR="$guest_dir" guest_scp "$fixture_dir/password"
status=0
guest_ssh "powershell -NoProfile -ExecutionPolicy Bypass -File $guest_dir/dev-server.ps1" > "$artifact_dir/native.log" 2>&1 || status=$?
# Delete the transferred password even when task registration/dispatch fails.
if ! guest_ssh "powershell -NoProfile -Command \"if (Test-Path '$guest_dir/password') { Remove-Item '$guest_dir/password' -Force -ErrorAction Stop }; if (Test-Path '$guest_dir/password') { exit 1 }; exit 0\""; then
  echo 'could not confirm removal of guest password file' >&2
  status=1
fi
for name in result.json error.txt exit.txt; do
  scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST:$guest_dir/$name" "$artifact_dir/" 2>/dev/null || true
done
if [ "$status" -eq 0 ]; then
  python3 - "$artifact_dir/result.json" <<'PYRESULT'
import json, pathlib, sys
result = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding='utf-8-sig'))
assert result['ok'] and result['url'] == 'https://wlab.132140.xyz:15401'
assert result['serverId'] != result['productionServerId']
PYRESULT
fi
printf 'ARTIFACTS %s\nGUEST %s (dev agent and binding intentionally remain active)\n' "$artifact_dir" "$guest_dir"
if [ "$status" -ne 0 ]; then cat "$artifact_dir/native.log" >&2; fi
exit "$status"
