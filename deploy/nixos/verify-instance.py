#!/usr/bin/env python3
"""S096 deployed trial acceptance. Uses only the fixed disposable sample Root.
Run in nix develop with package, evidence directory and unique phase arguments.
No credentials or tokens are written to evidence; admin traffic uses same-UID IPC.
"""
import hashlib
import json
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

PACKAGE = Path(sys.argv[1]).resolve(strict=True)
EVIDENCE = Path(sys.argv[2]).resolve(strict=True)
PHASE = sys.argv[3]
assert PHASE in ('test', 'switch')
ORIGIN = 'http://127.0.0.1:18721'
BASE = Path('/data/vaultsyncd-trial-s096/mnt')
SAMPLES = BASE/'samples'
STATE = BASE/'state'
CREDENTIAL = '/var/lib/vaultsyncd-trial-credentials/owner.password'
SERVICE_USER = 'vaultsyncd-trial'
SOURCE = '3640f3a3f72e09ba1302ffd19459266c2c7a23b6'


def command(args):
    r = subprocess.run(list(map(str, args)), text=True, capture_output=True, timeout=120)
    assert r.returncode == 0, (args, r.stdout, r.stderr)
    return r.stdout


def wait(predicate):
    for _ in range(200):
        try:
            result = predicate()
            if result: return result
        except (urllib.error.URLError, ConnectionError):
            pass
        time.sleep(.2)
    raise AssertionError('deployed service facts did not converge')


def main():
    manifest = json.loads((PACKAGE/'share/vaultsyncd/release-manifest.json').read_text())
    assert manifest['sourceInfo']['commit'] == SOURCE
    token = None

    def api(path, body=None, *, method=None, binary=False, headers=None):
        h = {} if headers is None else dict(headers)
        if token: h['authorization'] = 'Bearer '+token
        if isinstance(body, dict):
            body = json.dumps(body).encode(); h['content-type'] = 'application/json'
        request = urllib.request.Request(ORIGIN+path, data=body, headers=h, method=method)
        with urllib.request.urlopen(request, timeout=30) as r:
            data = r.read()
            if binary: return r.status, dict(r.headers), data
            return json.loads(data)

    wait(lambda: api('/v1/health')['status'] == 'ok')
    capabilities = api('/v1/capabilities')
    assert capabilities['protocol']['major'] == 1 and capabilities['server_id']
    result = {'passed': False, 'phase': PHASE, 'package': str(PACKAGE), 'source': SOURCE,
              'capabilities': capabilities, 'checks': ['new-source-capabilities-route-and-health']}
    password = command(['sudo', '-n', 'cat', CREDENTIAL]).strip()
    issued = api('/v1/auth/login', {'login':'owner', 'password':password, 'device_label':'S096 isolated trial'})
    token = issued['access_token']
    try:
        sample = SAMPLES/f's096-{PHASE}.txt'
        assert not sample.exists(), 'use a fresh phase; never overwrite an existing fixture'
        old = f'S096 {PHASE} original observed content\n'.encode()
        sample.write_bytes(old); sample.chmod(0o600)
        command(['setfacl', '-m', f'u:{SERVICE_USER}:rw', sample])
        original_keep = hashlib.sha256((SAMPLES/'keep.txt').read_bytes()).hexdigest()
        roots = api('/v1/roots')['roots']
        root = roots[0]['root_id'] if roots else api('/v1/admin/roots', {'path':str(SAMPLES)})['root_id']
        head = wait(lambda: next((item for item in api(f'/v1/roots/{root}/recovery')['items']
                                 if item['name'] == sample.name and item.get('hash') == hashlib.sha256(old).hexdigest()), None))
        content = f'S096 {PHASE} committed upload {secrets.token_hex(8)}\n'.encode()
        digest = hashlib.sha256(content).hexdigest()
        upload = api(f'/v1/roots/{root}/uploads', {'total_size':str(len(content)), 'hash':digest, 'chunk_size':'0'})
        api(f'/v1/roots/{root}/uploads/{upload["upload_id"]}/parts/0', content, method='PUT',
            headers={'content-type':'application/octet-stream', 'x-part-sha256':digest})
        api(f'/v1/roots/{root}/uploads/{upload["upload_id"]}/finalize', {})
        snapshot = api(f'/v1/roots/{root}/snapshots', {})
        operation = {'kind':'put_content', 'op_id':secrets.token_hex(16), 'device_id':secrets.token_hex(16),
                     'server_epoch':snapshot['server_epoch'], 'entry_id':head['entry_id'],
                     'base_revision':head['revision'], 'upload_id':upload['upload_id']}
        committed = api(f'/v1/roots/{root}/operations', operation)
        if committed['state'] == 'pending':
            committed = wait(lambda: (r if (r := api(f'/v1/roots/{root}/operations/{operation["op_id"]}'))['state'] != 'pending' else None))
        assert committed['state'] == 'committed' and not committed.get('conflict')
        assert sample.read_bytes() == content
        fixed_path = f'/v1/roots/{root}/entries/{head["entry_id"]}/versions/{committed["version_id"]}/content'
        assert api(fixed_path, binary=True)[2] == content
        result['checks'].append('owner-login-local-observer-upload-finalize-committed-and-fixed-download')
        before = int(command(['systemctl', 'show', '--property=MainPID', '--value', 'vaultsyncd.service']))
        command(['sudo', '-n', 'systemctl', 'restart', 'vaultsyncd.service'])
        wait(lambda: api('/v1/health')['status'] == 'ok')
        after = int(command(['systemctl', 'show', '--property=MainPID', '--value', 'vaultsyncd.service']))
        assert before != after and after > 0
        assert api('/v1/capabilities')['server_id'] == capabilities['server_id']
        replay = api(f'/v1/roots/{root}/operations', operation)
        assert replay['state'] == 'committed' and replay['replayed']
        assert replay['version_id'] == committed['version_id'] and replay['root_seq'] == committed['root_seq']
        assert api(fixed_path, binary=True)[2] == content and sample.read_bytes() == content
        assert hashlib.sha256((SAMPLES/'keep.txt').read_bytes()).hexdigest() == original_keep
        result['checks'].append('restart-stable-server-and-entry-identity-fixed-bytes-and-idempotent-replay')
        process_status = command(['sudo', '-n', 'cat', f'/proc/{after}/status'])
        fields = dict(line.split(':',1) for line in process_status.splitlines() if ':' in line)
        uid = int(command(['id', '-u', SERVICE_USER]))
        assert uid != 0 and all(int(n) == uid for n in fields['Uid'].split())
        assert all(int(fields[k].strip(),16) == 0 for k in ('CapEff','CapBnd','CapAmb'))
        assert fields['NoNewPrivs'].strip() == '1'
        result['main_process'] = {'before_pid':before, 'after_pid':after, 'uid':uid, 'capabilities':'0', 'NoNewPrivs':1}
        def admin(*args):
            text = command(['sudo', '-n', '-u', SERVICE_USER, PACKAGE/'bin/vaultsyncd-admin', *args,
                            '--socket', STATE/'admin/socket', '--json'])
            return json.loads(text)
        backup_id = secrets.token_hex(16)
        assert admin('backup','prepare','--id',backup_id)['phase'] == 'ready'
        assert admin('backup','verify-source',backup_id)['integrity'] == 'verified'
        assert admin('backup','release',backup_id)['phase'] == 'released'
        assert admin('status')['result']['phase'] == 'ready'
        result['checks'].append('same-uid-admin-and-unprivileged-btrfs-backup-verify-release')
        baseline = json.loads((EVIDENCE/'health-before.json').read_text())
        for unit in ('sshd','caddy','postgresql','memtrack-web','memtrack-grpc','receiptd','labtrack','thesisd','dae','systemd-networkd'):
            observed = command(['systemctl','show',unit,'--property=MainPID,ActiveState,InvocationID'])
            assert observed == baseline[unit], f'existing service changed: {unit}'
        for port in (3200,3300,15312):
            with urllib.request.urlopen(f'http://127.0.0.1:{port}/',timeout=20) as response:
                assert response.status == baseline[str(port)] == 200
        with socket.create_connection(('127.0.0.1',22), timeout=5) as s:
            assert s.recv(256).decode().strip() == baseline['ssh_banner']
        assert hashlib.sha256(Path('/etc/nixos/flake.lock').read_bytes()).hexdigest() == baseline['lock_sha256']
        result['checks'].append('ssh-existing-services-pids-and-http-healthy-host-lock-unchanged')
        result.update(passed=True, root_id=root, committed=committed, sha256=digest)
        (EVIDENCE/f'acceptance-{PHASE}.json').write_text(json.dumps(result, indent=2))
        print(json.dumps(result, indent=2))
    finally:
        api('/v1/auth/logout', {})

if __name__ == '__main__': main()
