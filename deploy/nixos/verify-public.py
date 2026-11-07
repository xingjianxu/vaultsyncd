#!/usr/bin/env python3
"""Explicit deployment acceptance; writes one new sample in an operator-selected Root.

Uses normal verified public HTTPS, same-UID IPC and runtime password input.
No tokens, cookies, passwords or private keys are included in the evidence.
"""
import argparse
import hashlib
import json
from pathlib import Path
import secrets
import subprocess
import sys
import time
import traceback
import urllib.error
import urllib.request


def command(args, **kwargs):
    result = subprocess.run(list(map(str, args)), capture_output=True, timeout=120, **kwargs)
    if result.returncode:
        raise RuntimeError('deployment subprocess failed')
    return result.stdout.decode().strip()


def wait(predicate):
    for _ in range(200):
        try:
            result = predicate()
            if result:
                return result
        except (urllib.error.URLError, ConnectionError):
            pass
        time.sleep(.25)
    raise TimeoutError('deployment state did not converge')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('origin')
    parser.add_argument('package', type=Path)
    parser.add_argument('evidence', type=Path)
    parser.add_argument('run_id')
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--state', type=Path, default=Path('/var/lib/vaultsyncd'))
    parser.add_argument('--user', default='vaultsyncd')
    parser.add_argument('--credential', type=Path, default=Path('/var/lib/vaultsyncd-credentials/owner.password'))
    parser.add_argument('--service', default='vaultsyncd.service')
    args = parser.parse_args()
    assert args.origin.startswith('https://') and '/' not in args.origin[8:]
    assert args.run_id and all(c.isalnum() or c in '-_' for c in args.run_id)
    package = args.package.resolve(strict=True)
    evidence = args.evidence.resolve(strict=True)
    output = evidence / f'acceptance-{args.run_id}.json'
    assert not output.exists(), 'do not overwrite prior evidence'
    token = None

    def api(path, body=None, *, method=None, binary=False, headers=None):
        request_headers = dict(headers or {})
        if token:
            request_headers['authorization'] = 'Bearer ' + token
        if isinstance(body, dict):
            body = json.dumps(body).encode()
            request_headers['content-type'] = 'application/json'
        request = urllib.request.Request(args.origin + path, data=body, headers=request_headers, method=method)
        with urllib.request.urlopen(request, timeout=30) as response:
            data = response.read()
            return data if binary else json.loads(data)

    def admin(*params):
        return json.loads(command(['sudo', '-n', '-u', args.user, package / 'bin/vaultsyncd-admin',
                                  *params, '--socket', args.state / 'admin/socket', '--json']))

    def disk_hash(path):
        return command(['sudo', '-n', '-u', args.user, 'sha256sum', path]).split()[0]

    wait(lambda: api('/v1/health')['status'] == 'ok')
    capabilities = api('/v1/capabilities')
    manifest = json.loads((package / 'share/vaultsyncd/release-manifest.json').read_text())
    result = {'origin': args.origin, 'source': manifest['sourceInfo'], 'capabilities': capabilities,
              'checks': ['verified-public-HTTPS-health-and-capabilities']}
    password = command(['sudo', '-n', 'cat', args.credential])
    issued = api('/v1/auth/login', {'login': 'owner', 'password': password, 'device_label': 'deployment acceptance'})
    del password
    token = issued['access_token']
    try:
        sample = args.root / f'deployment-{args.run_id}.txt'
        original = f'deployment {args.run_id} initial local sample\n'.encode()
        command(['sudo', '-n', '-u', args.user, 'python3', '-c',
                 'import pathlib,sys; p=pathlib.Path(sys.argv[1]); f=p.open("xb"); f.write(sys.stdin.buffer.read()); f.close(); p.chmod(0o600)',
                 sample], input=original)
        registered = api('/v1/admin/roots')['roots']
        matching = [root for root in registered if root.get('path') == str(args.root)]
        if matching:
            root = matching[0]['root_id']
        else:
            root = api('/v1/admin/roots', {'path': str(args.root)})['root_id']
        initial_hash = hashlib.sha256(original).hexdigest()
        head = wait(lambda: next((item for item in api(f'/v1/roots/{root}/recovery')['items']
                                 if item['name'] == sample.name and item.get('hash') == initial_hash), None))
        pattern = f'vaultsyncd {args.run_id} verified public HTTPS\n'.encode()
        content = (pattern * (16 * 1024 * 1024 // len(pattern) + 1))[:16 * 1024 * 1024]
        digest = hashlib.sha256(content).hexdigest()
        upload = api(f'/v1/roots/{root}/uploads', {'total_size': str(len(content)), 'hash': digest, 'chunk_size': str(len(content))})
        part = f'/v1/roots/{root}/uploads/{upload["upload_id"]}/parts/0'
        try:
            api(part, content + b'x', method='PUT', headers={'content-type': 'application/octet-stream', 'x-part-sha256': digest})
        except urllib.error.HTTPError as error:
            assert error.code == 413
        else:
            raise AssertionError('oversize upload was accepted')
        api(part, content, method='PUT', headers={'content-type': 'application/octet-stream', 'x-part-sha256': digest})
        api(f'/v1/roots/{root}/uploads/{upload["upload_id"]}/finalize', {})
        snapshot = api(f'/v1/roots/{root}/snapshots', {})
        operation = {'kind': 'put_content', 'op_id': secrets.token_hex(16), 'device_id': secrets.token_hex(16),
                     'server_epoch': snapshot['server_epoch'], 'entry_id': head['entry_id'],
                     'base_revision': head['revision'], 'upload_id': upload['upload_id']}
        committed = api(f'/v1/roots/{root}/operations', operation)
        if committed['state'] == 'pending':
            committed = wait(lambda: (value if (value := api(f'/v1/roots/{root}/operations/{operation["op_id"]}'))['state'] != 'pending' else None))
        assert committed['state'] == 'committed' and not committed.get('conflict')
        fixed = f'/v1/roots/{root}/entries/{head["entry_id"]}/versions/{committed["version_id"]}/content'
        assert hashlib.sha256(api(fixed, binary=True)).hexdigest() == disk_hash(sample) == digest
        result['checks'].append('16MiB-upload-finalize-committed-fixed-version-and-disk-hash-oversize-413')
        before = command(['systemctl', 'show', args.service, '--property=MainPID', '--value'])
        command(['sudo', '-n', 'systemctl', 'restart', args.service])
        wait(lambda: api('/v1/health')['status'] == 'ok')
        after = command(['systemctl', 'show', args.service, '--property=MainPID', '--value'])
        assert before != after and int(after) > 0
        assert api('/v1/capabilities')['server_id'] == capabilities['server_id']
        replay = api(f'/v1/roots/{root}/operations', operation)
        assert replay['state'] == 'committed' and replay['replayed']
        assert replay['version_id'] == committed['version_id']
        assert hashlib.sha256(api(fixed, binary=True)).hexdigest() == disk_hash(sample) == digest
        result['checks'].append('restart-preserves-server-identity-version-and-idempotent-replay')
        backup = secrets.token_hex(16)
        assert admin('backup', 'prepare', '--id', backup)['phase'] == 'ready'
        assert admin('backup', 'verify-source', backup)['integrity'] == 'verified'
        assert admin('backup', 'release', backup)['phase'] == 'released'
        assert admin('status')['result']['phase'] == 'ready'
        fields = dict(line.split(':', 1) for line in command(['sudo', '-n', 'cat', f'/proc/{after}/status']).splitlines() if ':' in line)
        assert all(int(fields[k].strip(), 16) == 0 for k in ('CapEff', 'CapBnd', 'CapAmb'))
        assert fields['NoNewPrivs'].strip() == '1' and int(fields['Uid'].split()[0]) != 0
        result['checks'].append('same-uid-IPC-unprivileged-btrfs-backup-verify-release')
        result.update(passed=True, root_id=root, sample=str(sample), committed=committed, sha256=digest)
        output.write_text(json.dumps(result, indent=2))
        print(json.dumps(result, indent=2))
    finally:
        api('/v1/auth/logout', {})


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        frames = [{'file': Path(f.filename).name, 'line': f.lineno} for f in traceback.extract_tb(error.__traceback__)
                  if f.filename == __file__]
        print(json.dumps({'passed': False, 'error_type': type(error).__name__,
                          'http_status': getattr(error, 'code', None), 'frames': frames}))
        sys.exit(1)
