#!/usr/bin/env python3
"""Actual shipped Compose on existing vaultful Podman socket, own btrfs loop only.
Args: locally verified image, evidence parent. No daemon/system config changes.
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import time
sys.dont_write_bytecode=True

IMAGE=sys.argv[1]
PARENT=Path(sys.argv[2]).resolve(strict=True)
assert IMAGE.startswith('localhost/vaultsyncd-s098:') and IMAGE.endswith('-amd64')
RUN=PARENT/('compose-'+str(os.getpid()));RUN.mkdir(mode=0o755)
PROJECT='vs-s098-'+str(os.getpid())
FILE=Path(__file__).resolve().parents[2]/'deploy/compose/compose.yaml'
RUNNER=FILE.with_name('run.py')
COMPOSE=shutil.which('docker-compose');assert COMPOSE
RESULT={'passed':False,'run':str(RUN),'image':IMAGE,'checks':[]}

def cmd(args,**kw):
    if 'input' not in kw:kw['stdin']=subprocess.DEVNULL
    p=subprocess.run(list(map(str,args)),text=True,capture_output=True,timeout=180,**kw)
    assert p.returncode==0, 'command failed: '+str(args)+'\n'+p.stderr
    return p.stdout
def sudo(*args,**kw):return cmd(['sudo','-n',*args],**kw)
def engine(*args,**kw):return sudo('podman',*args,**kw)

def main():
    spec=importlib.util.spec_from_file_location('acceptance',Path(__file__).with_name('test-container.py'))
    helper=importlib.util.module_from_spec(spec);spec.loader.exec_module(helper)
    helper.configure(IMAGE,'amd64',PARENT);helper.ORIGIN='http://127.0.0.1:18742'
    image=RUN/'volume.img';image.touch(mode=0o600)
    with image.open('wb') as f:f.truncate(1024*1024*1024)
    mount=RUN/'mnt';mount.mkdir();loop=None;mounted=False;up=False
    state=mount/'state';vaults=mount/'vaults'
    variables={'PATH':str(Path(COMPOSE).parent)+':/run/current-system/sw/bin:/usr/bin:/bin','DOCKER_HOST':'unix:///run/podman/podman.sock','VAULTSYNCD_IMAGE':IMAGE,
        'VAULTSYNCD_STATE':str(state),'VAULTSYNCD_VAULTS':str(vaults),'VAULTSYNCD_PORT':'18742'}
    def compose(*args,override=None):
        env=variables| (override or {})
        assert args[0] in ('up','down','restart','ps','config')
        return sudo('env',*[k+'='+v for k,v in env.items()],sys.executable,RUNNER,'--project-name',PROJECT,args[0])
    try:
        loop=sudo('losetup','--find','--show',image).strip()
        sudo('mkfs.btrfs','-q',loop);sudo('mount','-o','user_subvol_rm_allowed',loop,mount);mounted=True
        for p in (state,vaults):sudo('mkdir','-m','700',p);sudo('chown','8721:8721',p)
        password=secrets.token_urlsafe(24)
        engine('run','--rm','-i','--network','none','--user','8721:8721','--cap-drop=all','--security-opt','no-new-privileges',
            '-v',str(state)+':/var/lib/vaultsyncd','--entrypoint','/opt/vaultsyncd/bin/vaultsyncd-admin',IMAGE,'init','--state-dir','/var/lib/vaultsyncd',input=password+'\n')
        configuration=json.loads(compose('config','--format','json'))
        service=configuration['services']['vaultsyncd']
        assert service['read_only'] and service['user']=='8721:8721'
        assert all(v['bind']['create_host_path'] is False for v in service['volumes'])
        compose('up','-d','--pull','never');up=True
        helper.wait(lambda:helper.api('/v1/health')['status']=='ok')
        helper.TOKEN=helper.api('/v1/auth/login',{'login':'owner','password':password,'device_label':'S098 Compose'})['access_token']
        vault=helper.api('/v1/admin/vaults',{'path':'/srv/vaultsyncd-vaults'})['vault_id']
        before=next(x for x in helper.api('/v1/admin/vaults')['vaults'] if x['vault_id']==vault)
        cid=engine('ps','--filter','label=com.docker.compose.project='+PROJECT,'--format','{{.ID}}').strip()
        assert cid and len(cid.splitlines())==1
        status=engine('exec',cid,'cat','/proc/1/status')
        fields=dict(l.split(':',1) for l in status.splitlines() if ':' in l)
        assert all(int(v)==8721 for v in fields['Uid'].split()) and fields['NoNewPrivs'].strip()=='1'
        assert int(fields['CapEff'],16)==0 and int(fields['CapBnd'],16)==0
        engine('exec',cid,'sh','-c','test ! -e /nix && test ! -e /dev/fuse && ! command -v node')
        compose('restart');helper.wait(lambda:helper.api('/v1/health')['status']=='ok')
        after=next(x for x in helper.api('/v1/admin/vaults')['vaults'] if x['vault_id']==vault)
        assert after['health']=='healthy' and after['vault_seq']==before['vault_seq']
        helper.api('/v1/auth/logout',{});helper.TOKEN=None
        RESULT['checks']+=['shipped-compose-actual-up','nonvault-readonly-zero-capabilities-no-nix-node-fuse','real-btrfs-vault-healthy-after-compose-restart']
        RESULT.update(container_id=cid,before=before,after=after,process_status=status,compose_sha256=hashlib.sha256(FILE.read_bytes()).hexdigest())
        compose('down');up=False
        missing=RUN/'missing-state'
        try:compose('up','-d','--pull','never',override={'VAULTSYNCD_STATE':str(missing)})
        except AssertionError as error:
            RESULT['missing_source_error']=str(error)
            assert not missing.exists()
        else:raise AssertionError('missing bind source unexpectedly accepted')
        RESULT['checks'].append('missing-bind-source-rejected-without-creating-directory')
        RESULT['passed']=True
    finally:
        if helper.TOKEN:
            try:helper.api('/v1/auth/logout',{})
            except Exception:pass
        try:compose('down')
        except Exception:pass
        if mounted:sudo('umount',mount)
        if loop:sudo('losetup','-d',loop)
        RESULT['cleanup']='own compose removed and loop unmounted/detached; data image retained offline'
        (RUN/'result.json').write_text(json.dumps(RESULT,ensure_ascii=False,indent=2))
        print(json.dumps(RESULT,ensure_ascii=False))

if __name__=='__main__':main()
