#!/usr/bin/env python3
"""Isolated NixOS-generated unit, disposable btrfs and dedicated non-root UID.
Run inside nix develop. Needs passwordless sudo; never activates vaultsyncd.service.
"""
import hashlib
import json
import os
from pathlib import Path
import secrets
import shlex
import subprocess
import sys
import time

REPO = Path(__file__).resolve().parents[2]
RUN = Path('/data/vaultsyncd-dev/vaultsyncd-temp') / f'vs-s095-{time.strftime("%Y%m%dT%H%M%S")}-{os.getpid()}'
USER = f'vs-s095-{os.getpid()}'
UNIT = f'{USER}.service'
PACKAGE = Path(sys.argv[1]).resolve(strict=True)
NIXPKGS = sys.argv[2]
RESULT = {'passed': False, 'run': str(RUN), 'package': str(PACKAGE), 'checks': []}

def command(args, *, check=True, **kwargs):
    r = subprocess.run(list(map(str, args)), text=True, capture_output=True, **kwargs)
    if check and r.returncode:
        raise AssertionError(f'{args}: {r.returncode}\n{r.stdout}\n{r.stderr}')
    return r

def sudo(*args, **kwargs):
    return command(['sudo', '-n', *args], **kwargs)

def as_user(*args, **kwargs):
    return sudo('-u', USER, *args, **kwargs)

def nix(verb, attribute, parameters=None, **kwargs):
    args = ['nix', verb, '--impure', '--file', REPO/'deploy/nixos/eval.nix',
            '--argstr', 'nixpkgs', NIXPKGS, '--argstr', 'package', PACKAGE]
    if parameters:
        args += ['--argstr', 'parameters', parameters]
    return command(args + [attribute] + kwargs.pop('flags', []), **kwargs)

def main():
    RUN.mkdir(mode=0o755)
    mount = RUN/'mnt'; mount.mkdir()
    image = RUN/'volume.img'; image.touch(mode=0o600)
    with image.open('wb') as f: f.truncate(1024*1024*1024)
    loop = None; mounted = False; account = False; unit_file = Path('/run/systemd/system')/UNIT
    try:
        loop = sudo('losetup', '--find', '--show', image).stdout.strip()
        sudo('mkfs.btrfs', '-q', loop)
        sudo('mount', loop, mount); mounted = True
        sudo('chown', f'{os.getuid()}:{os.getgid()}', mount)
        sysusers = RUN/'sysusers.conf'
        sysusers.write_text(f'u {USER} - "S095 disposable service" /var/empty /run/current-system/sw/bin/nologin\n')
        sudo('systemd-sysusers', sysusers); account = True
        uid = int(command(['id', '-u', USER]).stdout)
        gid = int(command(['id', '-g', USER]).stdout)
        assert uid != 0
        state = mount/'state'; state.mkdir(mode=0o700)
        sudo('chown', f'{uid}:{gid}', state)
        vaults = mount/'vault'; vaults.mkdir(mode=0o700)
        sample = vaults/'keep.txt'; sample.write_bytes(b'S095 user data must remain intact\n'); sample.chmod(0o600)
        original = hashlib.sha256(sample.read_bytes()).hexdigest()
        outside = mount/'outside'; outside.mkdir(); outside.chmod(0o777)
        params = RUN/'parameters.json'
        parameters = {'stateDir': str(state), 'vaultPaths': [str(vaults)], 'user': USER, 'group': USER,
                      'settings': {'server': {'listen': '127.0.0.1:0'}}}
        params.write_text(json.dumps(parameters))
        RESULT['toplevel'] = json.loads(nix('eval', 'toplevel', params, flags=['--json']).stdout)
        built = json.loads(nix('build', 'unit', params, flags=['--no-link', '--json', '-L']).stdout)
        unit_dir = Path(built[0]['outputs']['out'])
        text = (unit_dir/'vaultsyncd.service').read_text()
        (RUN/'generated.service').write_text(text)
        policy = json.loads(nix('eval', 'policy', params, flags=['--json']).stdout)
        (RUN/'policy.json').write_text(json.dumps(policy, indent=2))
        preflight = policy['ExecStartPre']
        # DAC failure and mount option failure are diagnosed before DB open.
        r = as_user(preflight, check=False)
        assert r.returncode and not (state/'server.sqlite').exists()
        assert 'user_subvol_rm_allowed' in r.stderr
        RESULT['checks'].append('missing-unprivileged-delete-mount-option-rejected')
        sudo('mount', '-o', 'remount,user_subvol_rm_allowed', mount)
        r = as_user(preflight, check=False)
        assert r.returncode and 'access required' in r.stderr
        RESULT['checks'].append('vault-without-explicit-permission-rejected')
        sudo('setfacl', '-m', f'u:{uid}:rwx', vaults)
        sudo('setfacl', '-m', f'u:{uid}:r', sample)
        command(['chattr', '+C', vaults])
        r = as_user(preflight, check=False)
        assert r.returncode and 'NOCOW' in r.stderr
        command(['chattr', '-C', vaults])
        RESULT['checks'].append('nocow-rejected-without-changing-data')
        as_user(preflight)
        # Password is only an anonymous stdin pipe, never Nix input/store/config/argv.
        password = secrets.token_urlsafe(24)
        as_user(PACKAGE/'bin/vaultsyncd-admin', 'init', '--state-dir', state, input=password+'\n')
        RESULT['checks'].append('offline-owner-init-as-service-uid')
        probe = RUN/'probe.py'
        probe.write_text('''import json,os,pathlib,sys
vault,outside,system,state=map(pathlib.Path,sys.argv[1:])
r={"uid":os.geteuid(),"status":pathlib.Path("/proc/self/status").read_text()}
(vault/"unit-written.txt").write_text("non-root service write\\n")
r["vault_write"]=True
for name,p in [("outside",outside/"denied.txt"),("system",system)]:
    try:
        with p.open("x") as f:f.write("unexpected")
    except OSError as e:r[name]={"denied":True,"errno":e.errno}
    else:raise AssertionError("sandbox allowed "+str(p))
(state/"probe.json").write_text(json.dumps(r))
''')
        system_probe = Path('/etc')/f'{USER}-denied.txt'
        exec_post = ' '.join(shlex.quote(str(v)) for v in [sys.executable, probe, vaults, outside, system_probe, state])
        # Only the unit name and an in-sandbox evidence probe differ from the generated unit.
        text += f'\n[Service]\nExecStartPost={exec_post}\n'
        temp_unit = RUN/UNIT; temp_unit.write_text(text)
        sudo('install', '-m', '644', temp_unit, unit_file)
        sudo('systemctl', 'daemon-reload')
        sudo('systemctl', 'start', UNIT)
        assert sudo('systemctl', 'is-active', UNIT).stdout.strip() == 'active'
        evidence = json.loads(as_user('cat', state/'probe.json').stdout)
        assert evidence['uid'] == uid and evidence['outside']['denied'] and evidence['system']['denied']
        status = dict(line.split(':', 1) for line in evidence['status'].splitlines() if ':' in line)
        assert int(status['CapEff'].strip(), 16) == 0 and status['NoNewPrivs'].strip() == '1'
        main_pid = int(sudo('systemctl', 'show', '--property=MainPID', '--value', UNIT).stdout)
        server_status = sudo('cat', f'/proc/{main_pid}/status').stdout
        main_fields = dict(line.split(':', 1) for line in server_status.splitlines() if ':' in line)
        assert all(int(v) == uid for v in main_fields['Uid'].split())
        assert int(main_fields['CapEff'].strip(), 16) == 0 and main_fields['NoNewPrivs'].strip() == '1'
        RESULT['server_status'] = server_status
        RESULT['identity'] = evidence
        RESULT['checks'].append('actual-generated-unit-nonroot-empty-capabilities-nonewprivs')
        RESULT['checks'].append('explicit-vault-writable-world-writable-sibling-and-etc-readonly')
        socket = state/'admin/socket'
        def admin(*args, check=True):
            return as_user(PACKAGE/'bin/vaultsyncd-admin', *args, '--socket', socket, '--json', check=check)
        for _ in range(100):
            if as_user('test', '-S', socket, check=False).returncode == 0: break
            time.sleep(.1)
        command(['btrfs', 'subvolume', 'create', vaults/'nested'])
        nested = json.loads(admin('vaults', 'register', vaults, check=False).stdout)
        assert nested['status'] >= 400
        command(['btrfs', 'subvolume', 'delete', vaults/'nested'])
        RESULT['checks'].append('nested-subvolume-vault-admission-rejected')
        registered = json.loads(admin('vaults', 'register', vaults).stdout)
        (RUN/'registered.json').write_text(json.dumps(registered, indent=2))
        assert registered['status'] < 400
        denied = json.loads(admin('vaults', 'register', outside, check=False).stdout)
        assert denied['status'] >= 400
        RESULT['checks'].append('real-vault-registration-and-api-whitelist-enforced')
        backup_id = secrets.token_hex(16)
        prepared = json.loads(admin('backup', 'prepare', '--id', backup_id).stdout)
        (RUN/'prepared.json').write_text(json.dumps(prepared, indent=2))
        assert prepared['phase'] == 'ready' and not prepared['job_pending']
        snapshot = state/'backups'/backup_id/'snapshot'
        assert as_user('/run/current-system/sw/bin/btrfs', 'property', 'get', '-ts', snapshot, 'ro').stdout == 'ro=true\n'
        verified = json.loads(admin('backup', 'verify-source', backup_id).stdout)
        (RUN/'verified.json').write_text(json.dumps(verified, indent=2))
        assert verified['integrity'] == 'verified' and not verified['job_pending']
        released = json.loads(admin('backup', 'release', backup_id).stdout)
        (RUN/'released.json').write_text(json.dumps(released, indent=2))
        assert released['phase'] == 'released' and not released['job_pending']
        assert as_user('test', '-e', snapshot, check=False).returncode != 0
        RESULT['checks'].append('ordinary-uid-real-btrfs-backup-prepare-verify-release')
        assert sample.stat().st_uid == os.getuid() and hashlib.sha256(sample.read_bytes()).hexdigest() == original
        assert not (outside/'denied.txt').exists() and not system_probe.exists()
        assert password not in (RUN/'generated.service').read_text()
        generated_config = Path(shlex.split(policy['ExecStart'])[-1]).read_text()
        assert password not in generated_config and 'password' not in generated_config
        (RUN/'generated.toml').write_text(generated_config)
        RESULT['checks'].append('user-data-owner-and-bytes-preserved-no-recursive-chown')
        # Defaults and forbidden configurations are evaluated, not merely string-matched.
        default_policy = json.loads(nix('eval', 'policy', flags=['--json']).stdout)
        assert default_policy['StateDirectory'] == 'vaultsyncd' and default_policy['StateDirectoryMode'] == '0700'
        for name, override in [('root-user', {'user':'root'}), ('system-root', {'vaultPaths':['/etc']}),
                               ('state-overlap', {'vaultPaths':['/var/lib/vaultsyncd/data']}),
                               ('secret-option', {'settings':{'auth':{'password':'not-a-secret-sentinel'}}})]:
            f = RUN/f'{name}.json'; f.write_text(json.dumps(override))
            r = nix('eval', 'toplevel', f, flags=['--json'], check=False)
            (RUN/f'{name}.stderr').write_text(r.stderr)
            assert r.returncode != 0, name
        RESULT['checks'].append('default-statedirectory-and-invalid-config-evaluation-rejected')
        RESULT['passed'] = True
    finally:
        if unit_file.exists():
            sudo('journalctl', '-u', UNIT, '--no-pager', '-o', 'cat', check=False)
            journal = sudo('journalctl', '-u', UNIT, '--no-pager', '-o', 'cat', check=False).stdout
            (RUN/'service.log').write_text(journal)
            sudo('systemctl', 'stop', UNIT)
            sudo('rm', unit_file)
            sudo('systemctl', 'daemon-reload')
            sudo('systemctl', 'reset-failed', UNIT, check=False)
        if mounted: sudo('umount', mount)
        if loop: sudo('losetup', '-d', loop)
        if account:
            sudo('userdel', USER)
            sudo('groupdel', USER, check=False)  # userdel can remove matching private group.
        image.unlink()
        RESULT['cleanup'] = 'test unit/account/loop mount/image removed; no formal service activated'
        (RUN/'result.json').write_text(json.dumps(RESULT, indent=2, ensure_ascii=False))
        print(json.dumps(RESULT, indent=2, ensure_ascii=False))

if __name__ == '__main__': main()
