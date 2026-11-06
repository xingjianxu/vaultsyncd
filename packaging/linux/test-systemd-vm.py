#!/usr/bin/env python3
"""Actual shipped systemd unit in the dedicated Ubuntu ARM64 TCG VM.
Args: VM directory, ARM64 portable output. No host system configuration changed.
"""
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shlex
import subprocess
import sys
import traceback
sys.dont_write_bytecode=True

VM=Path(sys.argv[1]).resolve(strict=True)
PACKAGE=Path(sys.argv[2]).resolve(strict=True)
UNIT='vaultsyncd-s098.service'

def main():
    meta=json.loads((VM/'vm.json').read_text())
    assert b'vaultsyncd-s098-arm64-tcg' in Path(f'/proc/{meta["pid"]}/cmdline').read_bytes()
    manifest=json.loads((PACKAGE/'runtime/share/vaultsyncd/release-manifest.json').read_text())
    assert manifest['target']=='aarch64-unknown-linux-gnu'
    common=['-o','BatchMode=yes','-o','ConnectTimeout=30','-o','StrictHostKeyChecking=yes','-o','UserKnownHostsFile='+str(VM/'known_hosts'),'-i',str(VM/'id_ed25519')]
    ssh=['ssh',*common,'-p',str(meta['ssh_port']),'ubuntu@127.0.0.1']
    def remote(*args,input=None,check=True):
        options={'input':input} if input is not None else {'stdin':subprocess.DEVNULL}
        p=subprocess.run([*ssh,shlex.join(map(str,args))],capture_output=True,text=True,timeout=180,**options)
        if check:assert p.returncode==0,'guest command failed'
        return p
    archive=next(PACKAGE.glob('vaultsyncd-*-aarch64-unknown-linux-gnu.tar.gz'))
    remote('chmod','u+w','/home/ubuntu/s098/runtime-arm64.tar.gz',check=False)
    subprocess.run(['scp',*common,'-P',str(meta['ssh_port']),str(archive),'ubuntu@127.0.0.1:/home/ubuntu/s098/runtime-arm64.tar.gz'],check=True)
    setup='''import grp,json,os,pathlib,pwd,subprocess,sys
serial=sys.argv[1]
items=json.loads(subprocess.check_output(['lsblk','-b','-J','-o','NAME,SIZE,SERIAL,FSTYPE,MOUNTPOINTS'],text=True))['blockdevices']
n=next(n for n in items if n.get('serial')==serial)
assert n['size']==2147483648 and n['fstype']=='btrfs' and '/data' in n['mountpoints']
try:pwd.getpwnam('vaultsyncd')
except KeyError:
 try:pwd.getpwuid(8721)
 except KeyError:pass
 else:raise AssertionError('foreign UID')
 subprocess.run(['groupadd','--gid','8721','vaultsyncd'],check=True)
 subprocess.run(['useradd','--uid','8721','--gid','8721','--no-create-home','--home-dir','/var/lib/vaultsyncd','--shell','/usr/sbin/nologin','vaultsyncd'],check=True)
assert pwd.getpwnam('vaultsyncd').pw_uid==8721
base=pathlib.Path('/data/s098-systemd-'+str(os.getpid()));base.mkdir(mode=0o755)
for name,target in [('state','/var/lib/vaultsyncd'),('roots','/srv/vaultsyncd-roots')]:
 source=base/name;source.mkdir(mode=0o700);os.chown(source,8721,8721)
 p=pathlib.Path(target);p.mkdir(parents=True,exist_ok=True)
 if p.is_mount():
  mount=json.loads(subprocess.check_output(['findmnt','-J','-M',target,'-o','FSROOT,FSTYPE'],text=True))['filesystems'][0]
  assert mount['fstype']=='btrfs' and mount['fsroot'].startswith('/s098-systemd-')
  subprocess.run(['umount',target],check=True)
 assert not p.is_mount() and not any(p.iterdir())
 subprocess.run(['mount','--bind',str(source),target],check=True)
sample=pathlib.Path('/srv/vaultsyncd-roots/unit.txt');sample.write_text('S098 Ubuntu shipped systemd unit data\\n');sample.chmod(0o600);os.chown(sample,8721,8721)
root=pathlib.Path('/opt/vaultsyncd')
if root.exists():
 assert (root/'share/vaultsyncd/release-manifest.json').is_file()
 root.rename('/opt/vaultsyncd-s098-preserved-'+str(os.getpid()))
root.mkdir()
subprocess.run(['tar','-xzf','/home/ubuntu/s098/runtime-arm64.tar.gz','-C',str(root)],check=True)
cfg=pathlib.Path('/etc/vaultsyncd');cfg.mkdir(exist_ok=True)
text=(root/'share/vaultsyncd/vaultsyncd.toml').read_text().replace('127.0.0.1:8721','0.0.0.0:8721')
(cfg/'vaultsyncd.toml').write_text(text)
unit=pathlib.Path('/etc/systemd/system/vaultsyncd-s098.service')
if unit.exists():
 assert not subprocess.run(['systemctl','is-active','--quiet',unit.name]).returncode==0
 unit.rename(str(unit)+'.preserved-'+str(os.getpid()))
unit.write_bytes((root/'share/vaultsyncd/vaultsyncd.service').read_bytes())
subprocess.run(['systemd-analyze','verify',str(unit)],check=True)
subprocess.run(['systemctl','daemon-reload'],check=True)
print(json.dumps({'base':str(base),'unit':str(unit),'data_device':'/dev/'+n['name'],'data_serial':serial}))
'''
    prepared=remote('sudo','-n','python3','-',meta['data_serial'],input=setup)
    (VM/'systemd-preparation.log').write_text(prepared.stdout+prepared.stderr)
    password=secrets.token_urlsafe(24)
    initialized=remote('sudo','-n','-u','vaultsyncd','/opt/vaultsyncd/bin/vaultsyncd-admin','init','--state-dir','/var/lib/vaultsyncd',input=password+'\n')
    assert 'owner' in initialized.stdout
    spec=importlib.util.spec_from_file_location('portable_acceptance',Path(__file__).with_name('test-container.py'))
    helper=importlib.util.module_from_spec(spec);spec.loader.exec_module(helper)
    helper.configure('localhost/vaultsyncd-s098:systemd-arm64','arm64',VM)
    helper.ORIGIN='http://127.0.0.1:18741';helper.RUN=VM/'systemd-browser';helper.RUN.mkdir(exist_ok=True)
    result={'passed':False,'sourceInfo':manifest['sourceInfo'],'version':manifest['version'],'checks':[]}
    started=False
    try:
        remote('sudo','-n','systemctl','start',UNIT);started=True
        helper.wait(lambda:helper.api('/v1/health')['status']=='ok')
        helper.TOKEN=helper.api('/v1/auth/login',{'login':'owner','password':password,'device_label':'S098 Ubuntu systemd'})['access_token']
        server=helper.api('/v1/capabilities')['server_id']
        root=helper.api('/v1/admin/roots',{'path':'/srv/vaultsyncd-roots'})['root_id']
        helper.wait(lambda:len(helper.api(f'/v1/roots/{root}/recovery')['items'])==1)
        before=next(x for x in helper.api('/v1/admin/roots')['roots'] if x['root_id']==root)
        def admin(*args):return json.loads(remote('sudo','-n','-u','vaultsyncd','/opt/vaultsyncd/bin/vaultsyncd-admin',*args,'--socket','/var/lib/vaultsyncd/admin/socket','--json').stdout)
        assert admin('status')['result']['phase']=='ready'
        lease=secrets.token_hex(16)
        assert admin('backup','prepare','--id',lease)['phase']=='ready'
        assert admin('backup','verify-source',lease)['integrity']=='verified'
        assert admin('backup','release',lease)['phase']=='released'
        oldpid=remote('systemctl','show',UNIT,'-p','MainPID','--value').stdout.strip()
        remote('sudo','-n','systemctl','restart',UNIT)
        helper.wait(lambda:helper.api('/v1/health')['status']=='ok')
        newpid=remote('systemctl','show',UNIT,'-p','MainPID','--value').stdout.strip()
        assert newpid!=oldpid and helper.api('/v1/capabilities')['server_id']==server
        after=next(x for x in helper.api('/v1/admin/roots')['roots'] if x['root_id']==root)
        assert after['health']=='healthy' and after['root_seq']==before['root_seq']
        status=remote('sudo','-n','cat',f'/proc/{newpid}/status').stdout
        fields=dict(l.split(':',1) for l in status.splitlines() if ':' in l)
        assert all(int(n)==8721 for n in fields['Uid'].split()) and fields['NoNewPrivs'].strip()=='1'
        assert all(int(fields[k].strip(),16)==0 for k in ['CapEff','CapBnd','CapAmb'])
        assert remote('test','!','-e','/nix').returncode==0
        helper.browser(password,root)
        result.update(passed=True,server_id=server,root_id=root,before=before,after=after,browser=helper.RESULT['browser'],process_status=status)
        result['checks']=['unmodified-shipped-Ubuntu-systemd-unit-actual-start','nonroot-zero-caps-NoNewPrivs','same-uid-admin-and-real-btrfs-backup','real-service-restart-same-identity-seq','host-Chromium-against-actual-ARM64-service-Web']
        helper.api('/v1/auth/logout',{});helper.TOKEN=None
    finally:
        if helper.TOKEN:
            try:helper.api('/v1/auth/logout',{})
            except Exception:pass
        if started:remote('sudo','-n','systemctl','stop',UNIT,check=False)
        result['journal']=remote('sudo','-n','journalctl','-u',UNIT,'--no-pager','-o','cat').stdout
        (VM/'systemd-result.json').write_text(json.dumps(result,ensure_ascii=False,indent=2))
        print(json.dumps({'passed':result['passed'],'checks':result['checks']}))

if __name__=='__main__':
    try:main()
    except Exception as error:
        print(json.dumps({'passed':False,'error_type':type(error).__name__,'frames':[{'file':Path(f.filename).name,'line':f.lineno,'function':f.name} for f in traceback.extract_tb(error.__traceback__) if 'test-' in f.filename]}))
        sys.exit(1)
