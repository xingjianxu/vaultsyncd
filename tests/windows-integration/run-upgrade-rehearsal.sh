#!/usr/bin/env bash
# S091独立SQLite/协议fixture，不启动agent/CFAPI/GUI，不重启VM。run前由父协调窗口。
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$script_dir/../platform/windows/common.sh"
cd "$(repo_root)"
mode=${1:-build}
artifacts=${2:-artifacts/S091/windows}
mkdir -p "$artifacts"
if [[ $mode == build ]]; then
    test "${VAULTSYNCD_WINDOWS_SHELL:-}" = 1
    export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static -C link-arg=/ignore:4099"
    cargo xwin test --locked --target "$TARGET_TRIPLE" -p vaultsyncd-client-store -p vaultsyncd-server-store --lib --no-run --message-format=json > "$artifacts/store-build.json" 2> "$artifacts/store-build.log"
    cargo xwin test --locked --target "$TARGET_TRIPLE" -p vaultsyncd-protocol -p vaultsyncd-local-ipc --test upgrade --no-run --message-format=json > "$artifacts/protocol-build.json" 2> "$artifacts/protocol-build.log"
    python3 - "$artifacts" <<'PY'
import hashlib,json,pathlib,shutil,subprocess,sys,time
vault=pathlib.Path.cwd();out=pathlib.Path(sys.argv[1]);built={}
for name in ['store','protocol']:
    for line in (out/(name+'-build.json')).read_text().splitlines():
        try:v=json.loads(line)
        except ValueError:continue
        if v.get('reason')=='compiler-artifact' and v.get('executable') and v.get('profile',{}).get('test'):
            built[v['package_id'].split('#')[-1].split('@')[0]]=pathlib.Path(v['executable'])
spec=[('vaultsyncd-client-store','client-store.exe','db::s091::s091_client_upgrade_failure_and_complete_state_rollback','S091 client platform=windows'),('vaultsyncd-server-store','server-store.exe','db::s091::s091_server_upgrade_preserves_history_and_failure_backup','S091 server fixture_schema=12'),('vaultsyncd-protocol','remote-protocol.exe','s091_remote_major_rejected_minor_unknown_fields_and_features_do_not_enable_behavior','1 passed; 0 failed'),('vaultsyncd-local-ipc','local-protocol.exe','s091_local_fixed_minor_compatibility_and_major_refusal','1 passed; 0 failed')]
cases=[]
for package,name,case,marker in spec:
    shutil.copyfile(built[package],out/name);cases.append(dict(file=name,sha256=hashlib.sha256((out/name).read_bytes()).hexdigest(),filter=case,marker=marker))
source_files=['tests/contract/upgrade/client.rs','tests/contract/upgrade/server.rs','tests/contract/upgrade/support.rs','tests/contract/fixtures/s091/profiles.json','crates/protocol/tests/upgrade.rs','crates/local-ipc/tests/upgrade.rs']
source=dict(commit=subprocess.check_output(['git','rev-parse','HEAD']).decode().strip(),dirty=bool(subprocess.check_output(['git','status','--porcelain'])),files_sha256={p:hashlib.sha256((vault/p).read_bytes()).hexdigest()for p in source_files})
manifest=dict(run_id='s091-upgrade-'+str(time.time_ns()),source=source,formal_previous_release=False,cases=cases)
(out/'native-input.json').write_text(json.dumps(manifest,indent=2)+'\n')
shutil.copyfile(vault/'tests/windows-integration/upgrade-rehearsal.ps1',out/'upgrade-rehearsal.ps1')
print(json.dumps({'run_id':manifest['run_id'],'cases':[(c['file'],c['sha256'])for c in cases]}))
PY
elif [[ $mode == run ]]; then
    run_id=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["run_id"])' "$artifacts/native-input.json")
    [[ $run_id =~ ^s091-upgrade-[0-9]+$ ]]
    guest_dir="C:/vaultsyncd-lab/$run_id"
    guest_ssh "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '$guest_dir' | Out-Null\""
    GUEST_TARGET_DIR="$guest_dir" guest_scp "$artifacts/client-store.exe" "$artifacts/server-store.exe" "$artifacts/remote-protocol.exe" "$artifacts/local-protocol.exe" "$artifacts/native-input.json" "$artifacts/upgrade-rehearsal.ps1"
    input_hash=$(sha256sum "$artifacts/native-input.json" | cut -d' ' -f1)
    guest_ssh "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $guest_dir/upgrade-rehearsal.ps1 -InputHash $input_hash" | tee "$artifacts/native-output.log"
    guest_ssh "powershell -NoProfile -Command \"Get-Content -Raw -LiteralPath '$guest_dir/native-result.json'\"" > "$artifacts/native-result.json"
    python3 - "$artifacts/native-input.json" "$artifacts/native-result.json" <<'PY'
import json,sys
expected=json.load(open(sys.argv[1]));actual=json.load(open(sys.argv[2],encoding='utf-8-sig'))
assert actual['run_id']==expected['run_id'] and actual['filesystem']=='NTFS' and not actual['formal_previous_release']
assert len(actual['cases'])==4 and all(c['passed']==1 and c['failed']==0 for c in actual['cases'])
assert {c['file']:c['sha256']for c in actual['cases']}=={c['file']:c['sha256']for c in expected['cases']}
print('S091 Windows native: 4 fixtures passed, host/guest SHA-256 match; development profiles, not old formal release')
PY
    guest_ssh "powershell -NoProfile -Command \"Remove-Item -LiteralPath '$guest_dir' -Recurse -Force; if (Test-Path -LiteralPath '$guest_dir') { throw 'fixture cleanup incomplete' }\""
    printf 'S091 guest fixture cleaned: %s\n' "$guest_dir" | tee "$artifacts/native-cleanup.log"
else
    echo 'usage: run-upgrade-rehearsal.sh build|run [artifact-dir]' >&2
    exit 2
fi
