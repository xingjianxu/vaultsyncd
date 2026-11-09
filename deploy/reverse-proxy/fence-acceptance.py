#!/usr/bin/env python3
"""Disposable real NixOS unit: startup fences, live bind and durable intent replay.
Run in nix develop, arguments: package nixpkgs helper evidence-directory.
No fault injection into the persistent trial, no production fault switch, no secrets saved.
"""
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import time
import urllib.request
import urllib.error

REPO = Path(__file__).resolve().parents[2]
PACKAGE, NIXPKGS, HELPER, EVIDENCE = map(Path, sys.argv[1:])
RUN = Path('/data') / f'vs-s097-fences-{time.strftime("%Y%m%dT%H%M%S")}-{os.getpid()}'
UNIT = f'vs-s097-fences-{os.getpid()}.service'
USER = 'vaultsyncd-trial'
ORIGIN = 'http://127.0.0.1:18729'
TOKEN = None
RESULT = {'passed': False, 'run': str(RUN), 'package': str(PACKAGE), 'checks': []}

def command(args, **kw):
    r = subprocess.run(list(map(str, args)), text=True, capture_output=True, timeout=120, **kw)
    assert r.returncode == 0, (args, r.returncode, r.stdout, r.stderr)
    return r.stdout.strip()

def sudo(*args, **kw): return command(['sudo', '-n', *args], **kw)
def user(*args, **kw): return sudo('-u', USER, *args, **kw)

def api(path, body=None):
    headers = {}
    if TOKEN: headers['authorization'] = 'Bearer '+TOKEN
    if body is not None: body=json.dumps(body).encode();headers['content-type']='application/json'
    with urllib.request.urlopen(urllib.request.Request(ORIGIN+path, data=body, headers=headers), timeout=10) as r:
        return json.load(r)

def wait(fn):
    for _ in range(200):
        try:
            value=fn()
            if value:return value
        except (urllib.error.URLError, ConnectionError): pass
        time.sleep(.1)
    raise AssertionError('fixture did not converge')

def pid(): return int(sudo('systemctl','show','--property=MainPID','--value',UNIT))

def start(recovery=False):
    sudo('systemctl','start',UNIT)
    if not recovery:
        wait(lambda:api('/v1/capabilities')['server_id'])
        wait(lambda:api('/v1/health')['status']=='ok')
    else:
        def blocked():
            try:api('/v1/health')
            except urllib.error.HTTPError as e:return e.code==503
            return False
        wait(blocked)

def stop(): sudo('systemctl','stop',UNIT)

def facts(path):
    return {'stat':sudo('stat','-c','%d|%i|%h|%s|%W|%w',path),
            'sha256':sudo('sha256sum',path).split()[0]}

def runtime(vaultpath):
    current=pid()
    return {'pid':current,'namespace':sudo('readlink',f'/proc/{current}/ns/mnt'),
            'mount':sudo('nsenter','-t',current,'-m','--','findmnt','-T',vaultpath,'-n','-o','ID,MAJ:MIN,UUID,FSVAULT,OPTIONS')}

def managed(vault):return next(x for x in api('/v1/admin/vaults')['vaults'] if x['vault_id']==vault)

def dbfacts(state):
    # Read-only after service stop; no credentials selected and no mutations.
    return json.loads(user('sqlite3','-readonly','-json',state/'server.sqlite',
        "SELECT lower(hex(id)) AS vault,seq,cast(vault_file_identity as text) AS identity FROM vaults"))[0]

def seed(state, vault, name, phase):
    return json.loads(user(HELPER,state,vault,name,phase))

def run():
    global TOKEN, HELPER
    RUN.mkdir(mode=0o755)
    RESULT['helper_sha256']=hashlib.sha256(HELPER.read_bytes()).hexdigest()
    shutil.copy2(HELPER,RUN/'fence-boundary')
    HELPER=RUN/'fence-boundary';HELPER.chmod(0o755)
    mount=RUN/'mnt';mount.mkdir()
    image=RUN/'volume.img';image.touch(mode=0o600)
    with image.open('wb') as f:f.truncate(1024*1024*1024)
    loop=None;mounted=False;spacer_loop=None;spacer_mounted=False;keeper=None;keeper_pid=None
    spacer=RUN/'spacer';spacer.mkdir()
    unit_file=Path('/run/systemd/system')/UNIT
    try:
        loop=sudo('losetup','--find','--show',image)
        sudo('mkfs.btrfs','-q',loop)
        spacer_image=RUN/'spacer.img';spacer_image.touch(mode=0o600)
        with spacer_image.open('wb') as f:f.truncate(256*1024*1024)
        spacer_loop=sudo('losetup','--find','--show',spacer_image)
        sudo('mkfs.btrfs','-q',spacer_loop)
        sudo('mount','-o','user_subvol_rm_allowed',loop,mount);mounted=True
        state=mount/'state';vaultpath=mount/'vault'
        for path in (state,vaultpath):sudo('mkdir','-m','700',path);sudo('chown',f'{USER}:{USER}',path)
        for name in ('keep.txt','same-dev.txt','published-remount.txt','prepared-remount.txt'):
            user('sh','-c','umask 077; printf "%s\\n" "$1" > "$2"','fixture',f'S097 original {name}',vaultpath/name)
        params=RUN/'parameters.json'
        params.write_text(json.dumps({'stateDir':str(state),'vaultPaths':[str(vaultpath)],'user':USER,'group':USER,'settings':{'server':{'listen':'127.0.0.1:18729'}}}))
        unit=json.loads(command(['nix','build','--impure','--file',REPO/'deploy/nixos/eval.nix','--argstr','nixpkgs',NIXPKGS,'--argstr','package',PACKAGE,'--argstr','parameters',params,'unit','--no-link','--json']))[0]['outputs']['out']
        sudo('install','-m','644',Path(unit)/'vaultsyncd.service',unit_file)
        password=secrets.token_urlsafe(24)
        user(PACKAGE/'bin/vaultsyncd-admin','init','--state-dir',state,input=password+'\n')
        sudo('systemctl','daemon-reload');start()
        issued=api('/v1/auth/login',{'login':'owner','password':password,'device_label':'S097 fence fixture'})
        TOKEN=issued['access_token'];password=None
        vault=api('/v1/admin/vaults',{'path':str(vaultpath)})['vault_id']
        wait(lambda:len(api(f'/v1/vaults/{vault}/recovery')['items'])==4)
        wait(lambda:managed(vault)['collection']['state']=='complete')
        RESULT['vault']=vault;RESULT['initial_managed']=managed(vault)
        keep=facts(vaultpath/'keep.txt')
        runtime_before=runtime(vaultpath)
        # Pin the old namespace read-only: otherwise the kernel may recycle both
        # namespace inode and mount IDs immediately and fail to exercise the bug.
        keeper=subprocess.Popen(['sudo','-n','nsenter','-t',str(pid()),'-m','--','sh','-c','echo $$; exec sleep 90'],stdout=subprocess.PIPE,text=True)
        keeper_pid=int(keeper.stdout.readline().strip())
        stop();initial=dbfacts(state);start()
        healthy=wait(lambda:managed(vault) if managed(vault)['health']=='healthy' else None)
        runtime_after=runtime(vaultpath)
        assert runtime_before['namespace']!=runtime_after['namespace']
        assert runtime_before['mount'].split()[0]!=runtime_after['mount'].split()[0]
        sudo('kill','-TERM',keeper_pid);keeper.wait(timeout=10);keeper=None;keeper_pid=None
        stop();restarted=dbfacts(state)
        assert initial==restarted and keep==facts(vaultpath/'keep.txt')
        RESULT['normal_restart']={'before':initial,'after':restarted,'managed':healthy,'runtime_before':runtime_before,'runtime_after':runtime_after,'keep':keep}
        RESULT['checks'].append('systemd-namespace-restart-same-vault-blob-seq-file-healthy')
        # Real Prepared boundary, ordinary service restart keeps anonymous device.
        same=seed(state,vault,'same-dev.txt','prepared');before=facts(vaultpath/'same-dev.txt')
        start();op=wait(lambda: (x if x['state']=='committed' else None) if (x:=api(f'/v1/vaults/{vault}/operations/{same["op"]}')) else None)
        assert facts(vaultpath/'same-dev.txt')['sha256']==same['after_hash']
        RESULT['same_device_prepared']={'boundary':same,'before':before,'after':facts(vaultpath/'same-dev.txt'),'operation':op}
        RESULT['checks'].append('prepared-durable-intent-redone-after-real-systemd-restart')
        # Live same-directory bind changes mount fence even with identical storage identity.
        current=pid();ns_before=sudo('readlink',f'/proc/{current}/ns/mnt')
        seq=api(f'/v1/vaults/{vault}/snapshots',{})['high_water_seq']
        sudo('nsenter','-t',current,'-m','--','mount','--bind',vaultpath,vaultpath)
        try:
            blocked=wait(lambda:managed(vault) if managed(vault)['collection']['state']=='blocked' else None)
            assert blocked['health']=='unavailable'
            time.sleep(2)
            assert api(f'/v1/vaults/{vault}/snapshots',{})['high_water_seq']==seq
            assert len(api(f'/v1/vaults/{vault}/recovery')['items'])==4
            assert facts(vaultpath/'keep.txt')==keep
            RESULT['bind']={'namespace':ns_before,'managed':blocked,'seq':seq,'entries':4}
        finally:sudo('nsenter','-t',current,'-m','--','umount',vaultpath)
        wait(lambda:managed(vault)['health']=='healthy')
        RESULT['checks'].append('live-identical-directory-bind-blocked-zero-deletions-original-fence-restored')
        # Published boundary: remount changes dev, content proof permits metadata-only completion.
        stop();published=seed(state,vault,'published-remount.txt','published');published_before=facts(vaultpath/'published-remount.txt')
        prior=dbfacts(state)
        sudo('umount',mount);mounted=False
        # Kernel may immediately recycle the old anonymous dev. Occupy that number
        # with another disposable btrfs before remounting the same original volume.
        sudo('mount',spacer_loop,spacer);spacer_mounted=True
        sudo('mount','-o','user_subvol_rm_allowed',loop,mount);mounted=True
        published_remounted=facts(vaultpath/'published-remount.txt')
        assert published_before['stat'].split('|')[0]!=published_remounted['stat'].split('|')[0], 'must actually change anonymous dev'
        assert published_before['stat'].split('|')[1:]==published_remounted['stat'].split('|')[1:]
        start();op=wait(lambda: (x if x['state']=='committed' else None) if (x:=api(f'/v1/vaults/{vault}/operations/{published["op"]}')) else None)
        assert facts(vaultpath/'published-remount.txt')==published_remounted
        RESULT['published_remount']={'boundary':published,'before':published_before,'remounted':published_remounted,'after':facts(vaultpath/'published-remount.txt'),'operation':op,'old_vault':prior}
        RESULT['checks'].append('published-durable-intent-completed-after-dev-change-without-overwrite')
        # Old FileKey lacks birth. Prepared + changed dev must preserve before bytes and pending.
        stop();prepared=seed(state,vault,'prepared-remount.txt','prepared');prepared_before=facts(vaultpath/'prepared-remount.txt')
        sudo('umount',mount);mounted=False
        sudo('umount',spacer);spacer_mounted=False
        sudo('mount','-o','user_subvol_rm_allowed',loop,mount);mounted=True
        prepared_remounted=facts(vaultpath/'prepared-remount.txt')
        assert prepared_before['stat'].split('|')[0]!=prepared_remounted['stat'].split('|')[0]
        start(recovery=True);time.sleep(2)
        assert api(f'/v1/vaults/{vault}/operations/{prepared["op"]}')['state']=='pending'
        assert facts(vaultpath/'prepared-remount.txt')==prepared_remounted
        assert prepared_remounted['sha256']==prepared['before_hash']
        stop()
        unfinished=json.loads(user('sqlite3','-readonly','-json',state/'server.sqlite',"SELECT phase FROM fs_intents WHERE phase!='finalized'"))
        assert unfinished
        RESULT['prepared_remount']={'boundary':prepared,'before':prepared_before,'after':prepared_remounted,'unfinished':unfinished,'vault':dbfacts(state)}
        RESULT['checks'].append('prepared-old-dev-cannot-prove-legacy-file-identity-preserves-pending-and-content')
        RESULT['journal']=sudo('journalctl','-u',UNIT,'--no-pager','-o','cat')
        assert '启动意图复用已准入Vault围栏' in RESULT['journal']
        RESULT['passed']=True
    finally:
        if keeper_pid:
            subprocess.run(['sudo','-n','kill','-TERM',str(keeper_pid)],capture_output=True)
            keeper.wait(timeout=10)
        subprocess.run(['sudo','-n','systemctl','stop',UNIT],capture_output=True)
        if unit_file.exists():sudo('rm',unit_file);sudo('systemctl','daemon-reload')
        if mounted:sudo('umount',mount)
        if spacer_mounted:sudo('umount',spacer)
        if loop:sudo('losetup','-d',loop)
        if spacer_loop:sudo('losetup','-d',spacer_loop)
        # Keep dedicated offline data for evidence/recovery; never remove unknown paths.
        (EVIDENCE/'fence-acceptance.json').write_text(json.dumps(RESULT,indent=2))
        print(json.dumps({'passed':RESULT['passed'],'run':str(RUN),'checks':RESULT['checks']}))

run()
