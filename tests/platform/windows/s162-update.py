#!/usr/bin/env python3
"""Controlled version fixtures exercising production update PE on native Windows.

Does not publish a release or claim previous-version compatibility. Uses real ZIP
ownership, per-run LOCALAPPDATA, NTFS, IPC, WebView and the candidate maintenance
entry. Requires the existing Quickemu desktop and no agent in that user session.
"""
import base64
import hashlib
import json
import os
import selectors
from pathlib import Path
import shutil
import subprocess
import sys
import time
import zipfile

ROOT = Path(__file__).resolve().parents[3]
TEMP = Path('/data/vaultsyncd-temp/S162-client-update/evidence')
KEY = '/data/vaultsyncd-lab/win-lab/ssh/vaultsyncd-lab_ed25519'
OPTIONS = ['-i', KEY, '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes', '-o',
           'UserKnownHostsFile=/data/vaultsyncd-lab/win-lab/ssh/known_hosts',
           '-o', 'ConnectTimeout=10', '-o', 'LogLevel=ERROR']
SSH = ['ssh', '-p', '2222', *OPTIONS, 'Quickemu@127.0.0.1']
SCP = ['scp', '-P', '2222', *OPTIONS]
EXES = ['vaultsyncd-agent.exe', 'vaultsyncd-session.exe', 'vaultsyncd-cli.exe',
        'vaultsyncd-desktop.exe', 'vaultsyncd-zip.exe']


def run(args):
    return subprocess.run([str(x) for x in args], check=True, capture_output=True,
                          text=True, cwd=ROOT, timeout=120).stdout


def remote(script):
    encoded = base64.b64encode(script.encode('utf-16-le')).decode()
    return run([*SSH, 'powershell.exe -NoProfile -ExecutionPolicy Bypass -EncodedCommand ' + encoded])


def execute_test(script, log_path):
    """Stream guest progress and bound the SSH controller without killing a writer."""
    encoded = base64.b64encode(script.encode('utf-16-le')).decode()
    command = [*SSH, 'powershell.exe -NoProfile -ExecutionPolicy Bypass -EncodedCommand ' + encoded]
    started = time.monotonic()
    heartbeat = started
    output = []
    process = subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        with selectors.DefaultSelector() as selector, log_path.open('w') as log:
            selector.register(process.stdout, selectors.EVENT_READ)
            while selector.get_map():
                for key, _ in selector.select(1):
                    data = os.read(key.fd, 65536)
                    if not data:
                        selector.unregister(key.fileobj)
                        continue
                    chunk = data.decode('utf-8', errors='replace')
                    output.append(chunk)
                    log.write(chunk)
                    log.flush()
                    print(chunk, end='', flush=True)
                now = time.monotonic()
                if now - heartbeat >= 20:
                    print(f'Native validation active: {int(now-started)}s; guest evidence retained.', flush=True)
                    heartbeat = now
                if now - started > 1000:
                    raise subprocess.TimeoutExpired(command, 1000)
        code = process.wait(timeout=10)
        if code:
            raise subprocess.CalledProcessError(code, command, output=''.join(output))
        return ''.join(output)
    finally:
        if process.poll() is None:
            process.terminate()  # SSH controller only; guest transaction remains recoverable.
            process.wait(timeout=10)


def collect_logs(guest, artifact):
    # Full programs/state/backups stay in the VM; the input ZIPs already exist on host.
    remote(f"""Add-Type -AssemblyName System.IO.Compression.FileSystem
$root='{guest}';$dest=$root+'/evidence-logs.zip'
$zip=[IO.Compression.ZipFile]::Open($dest,[IO.Compression.ZipArchiveMode]::Create)
try {{ Get-ChildItem $root -Recurse -File | Where-Object {{ $_.Extension -in @('.json','.log','.txt','.out','.err') -and $_.Length -lt 4194304 }} | ForEach-Object {{
  $name=$_.FullName.Substring(([IO.Path]::GetFullPath($root)).Length).TrimStart('\\').Replace('\\','/')
  [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip,$_.FullName,$name) | Out-Null
}} }} finally {{ $zip.Dispose() }}""")
    run([*SCP, f'Quickemu@127.0.0.1:{guest}/evidence-logs.zip', artifact / 'evidence-logs.zip'])


def sha(data):
    return hashlib.sha256(data).hexdigest()


def package(binaries, version, old=False, broken_start=False):
    files = {name: (binaries / name).read_bytes() + (b'S162 old version fixture' if old else b'')
             for name in EXES}
    if broken_start:
        # A valid but unsuitable PE simulates a package whose launcher cannot start
        # the new agent. All manifests still match, so startup rollback is exercised.
        files['vaultsyncd-session.exe'] = files['vaultsyncd-cli.exe']
    source = {'version': version, 'commit': run(['git', 'rev-parse', 'HEAD']).strip(),
              'acceptance_fixture': 'uncommitted S162 build; artificial version metadata',
              'diff_sha256': sha(run(['git', 'diff']).encode())}
    def item(name):
        return {'path': name, 'bytes': len(files[name]), 'sha256': sha(files[name])}
    files['installed-files.json'] = json.dumps({
        'format': 1, 'source': source, 'signature_status': 'unsigned-test',
        'files': [item(n) for n in EXES[:4]]}).encode()
    files['UNSIGNED-TEST.txt'] = b'Controlled S162 test fixture, not a published release.\n'
    for name in ['Start vaultsyncd.cmd', 'Stop vaultsyncd.cmd', 'README-ZIP.txt']:
        files[name] = (ROOT / 'packaging/windows/zip' / name).read_bytes()
    files['zip-files.json'] = json.dumps({
        'format': 1, 'source': source, 'signature_status': 'unsigned-test',
        'files': [item(n) for n in files]}).encode()
    return files


def main():
    if len(sys.argv) not in (2, 3) or (len(sys.argv) == 3 and sys.argv[2] != '--installed-only'):
        raise SystemExit('usage: s162-update.py <Windows debug binaries directory> [--installed-only]')
    binaries = Path(sys.argv[1]).resolve()
    run_id = 's162-' + time.strftime('%Y%m%dT%H%M%S') + '-' + str(os.getpid())
    artifact = TEMP / run_id
    artifact.mkdir(parents=True)
    for name, old, version, broken in [('old.zip', True, '0.1.0', False),
                                       ('new.zip', False, '0.1.1', False),
                                       ('bad-start.zip', False, '0.1.1', True)]:
        with zipfile.ZipFile(artifact / name, 'x', zipfile.ZIP_DEFLATED) as target:
            for filename, data in package(binaries, version, old, broken).items():
                target.writestr(filename, data)
    for pattern, name in [('vaultsyncd_agent-*.exe', 'agent-tests.exe'),
                          ('vaultsyncd_platform_windows-*.exe', 'platform-tests.exe')]:
        candidates = list((binaries / 'deps').glob(pattern))
        shutil.copyfile(max(candidates, key=lambda p: p.stat().st_mtime), artifact / name)
    script = artifact / 's162-update.ps1'
    # Windows PowerShell 5.1 requires BOM for Chinese literals.
    script.write_text((ROOT / 'tests/platform/windows/s162-update.ps1').read_text(), encoding='utf-8-sig')
    hashes = {p.name: sha(p.read_bytes()) for p in artifact.iterdir() if p.is_file()}
    (artifact / 'hashes.json').write_text(json.dumps(hashes))
    guest = 'C:/vaultsyncd-lab/' + run_id
    remote(f"New-Item -ItemType Directory -Path '{guest}' | Out-Null")
    run([*SCP, *artifact.iterdir(), f'Quickemu@127.0.0.1:{guest}/'])
    try:
        output = execute_test(f"& '{guest}/s162-update.ps1'" + (' -InstalledOnly' if len(sys.argv) == 3 else ''), artifact / 'native.log')
        (artifact / 'native.log').write_text(output)
        print(output)
    except subprocess.CalledProcessError as error:
        (artifact / 'native.log').write_text(error.stdout or '')
        (artifact / 'native-error.log').write_text(error.stderr or '')
        raise
    finally:
        # Only this run's artifacts; guest evidence and test profiles are retained.
        try:
            collect_logs(guest, artifact)
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
            (artifact / 'copy-error.log').write_text(error.stderr or '')
        print('EVIDENCE', artifact)


if __name__ == '__main__':
    main()
