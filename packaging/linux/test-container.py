#!/usr/bin/env python3
"""Actual Ubuntu image, btrfs bind, HTTP/Web, native admin/backup and restart.
Args: rootless-image-name amd64|arm64 evidence-parent. Own image/loop/unit only.
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
import traceback
import urllib.error
import urllib.request
UID=8721
TOKEN=None

def configure(image,arch,parent):
    global IMAGE,ARCH,PARENT,RUN,NAME,PORT,ORIGIN,RESULT
    IMAGE,ARCH,PARENT=image,arch,parent
    assert IMAGE.startswith('localhost/vaultsyncd-s098:') and ARCH in ('amd64','arm64')
    RUN=Path(PARENT)/f'accept-{ARCH}-{os.getpid()}'
    NAME=f'vs-s098-{ARCH}-{os.getpid()}'
    PORT=18739 if ARCH=='amd64' else 18740
    ORIGIN=f'http://127.0.0.1:{PORT}'
    RESULT={'passed':False,'run':str(RUN),'image':IMAGE,'architecture':ARCH,'checks':[]}

def command(args, *, binary=False, check=True, **kw):
    if 'input' not in kw:kw['stdin']=subprocess.DEVNULL
    p=subprocess.run(list(map(str,args)),capture_output=True,text=not binary,timeout=180,**kw)
    if check:assert p.returncode==0,'command failed: '+str(args)
    return p

def sudo(*args,**kw):return command(['sudo','-n',*args],**kw)
def engine(*args,**kw):return sudo('podman',*args,**kw)

def api(path, body=None, method=None, binary=False, headers=None):
    h={} if headers is None else dict(headers)
    if TOKEN:h['authorization']='Bearer '+TOKEN
    if isinstance(body,dict):body=json.dumps(body).encode();h['content-type']='application/json'
    try:
        with urllib.request.urlopen(urllib.request.Request(ORIGIN+path,data=body,headers=h,method=method),timeout=30) as response:
            return response.read() if binary else json.load(response)
    except urllib.error.HTTPError as error:
        RESULT['last_api_error']={'path':path,'status':error.code,'body':error.read().decode()}
        raise

def wait(fn):
    for _ in range(300):
        try:
            value=fn()
            if value:return value
        except (urllib.error.URLError,ConnectionError):pass
        time.sleep(.2)
    raise AssertionError('facts did not converge')

def admin(*args):
    p=engine('exec','--user',f'{UID}:{UID}',NAME,'/opt/vaultsyncd/bin/vaultsyncd-admin',*args,'--socket','/var/lib/vaultsyncd/admin/socket','--json')
    return json.loads(p.stdout)

def browser(password,root):
    if os.environ.get('VAULTSYNCD_TEST_EXTERNAL_BROWSER')=='stdio':
        # The host runs its locked real Chromium against the VM's forwarded
        # endpoint. Only non-secret coordination crosses stdout; the owner
        # password arrives once over the existing anonymous SSH stdin pipe.
        print(json.dumps({'stage':'browser_pending','root':root,'origin':ORIGIN}),flush=True)
        proof=json.loads(sys.stdin.readline())
        assert proof['passed'] and proof['root']==root
        RESULT['browser']=proof['browser']
        return
    from playwright.sync_api import sync_playwright, expect
    with sync_playwright() as p:
        chromium=p.chromium.launch(headless=True,args=['--no-proxy-server'])
        context=chromium.new_context();page=context.new_page()
        try:
            page.goto(ORIGIN+'/#/account')
            page.get_by_label('登录名',exact=True).fill('owner');page.get_by_label('密码',exact=True).fill(password)
            page.get_by_role('button',name='登录',exact=True).click()
            expect(page.get_by_text('已登录：owner',exact=True)).to_be_visible(timeout=30000)
            page.get_by_role('link',name='Root 目录',exact=True).click()
            expect(page.get_by_text('挂载核对通过',exact=True)).to_be_visible(timeout=30000)
            expect(page.get_by_text('本进程初始采集完成',exact=True)).to_be_visible(timeout=30000)
            assert page.evaluate('root=>fetch("/v1/admin/roots").then(r=>r.json()).then(d=>d.roots.some(x=>x.root_id===root&&x.health==="healthy"))',root)
            page.screenshot(path=str(RUN/'web.png'),full_page=True)
            RESULT['browser']=chromium.version
        finally:
            try:page.evaluate('fetch("/v1/auth/logout",{method:"POST",headers:{"content-type":"application/json"},body:"{}"}).then(r=>r.status)')
            finally:context.close();chromium.close()

def main():
    global TOKEN
    RUN.mkdir(mode=0o755)
    mount=RUN/'mnt';mount.mkdir()
    image=RUN/'volume.img';image.touch(mode=0o600,exist_ok=False)
    with image.open('wb') as f:f.truncate(1024*1024*1024)
    loop=None;mounted=False;created=False
    try:
        inspected=json.loads(command(['podman','image','inspect',IMAGE]).stdout)[0]
        assert inspected['Architecture']==ARCH and inspected['Config']['User']=='8721:8721'
        assert inspected['Config']['Healthcheck']['Test']
        archive=RUN/'image.oci'
        command(['podman','save','--format','oci-archive','-o',archive,IMAGE]);engine('load','-i',archive)
        RESULT['image_id']=inspected['Id'];RESULT['labels']=inspected['Config']['Labels']
        overlay=engine('run','--rm','--network','none','--cap-drop=all','--security-opt','no-new-privileges',
            '--entrypoint','sh',IMAGE,'-c','/opt/vaultsyncd/bin/vaultsyncd-server --config /etc/vaultsyncd/vaultsyncd.toml; result=$?; test ! -e /var/lib/vaultsyncd/server.sqlite || exit 99; exit "$result"',check=False)
        assert overlay.returncode not in (0,99) and 'writable btrfs required' in overlay.stderr
        RESULT['overlay']={'exit':overlay.returncode,'stderr':overlay.stderr}
        RESULT['checks'].append('actual-default-overlay-rejected-before-database-open')
        loop=sudo('losetup','--find','--show',image).stdout.strip()
        sudo('mkfs.btrfs','-q',loop);sudo('mount','-o','user_subvol_rm_allowed',loop,mount);mounted=True
        state=mount/'state';roots=mount/'roots'
        for directory in (state,roots):sudo('mkdir','-m','700',directory);sudo('chown',f'{UID}:{UID}',directory)
        sample=roots/'sample.txt'
        old=b'S098 actual Ubuntu externally observed content\n'
        temp=RUN/'sample-input.txt';temp.write_bytes(old);sudo('install','-m','600','-o',UID,'-g',UID,temp,sample)
        mounts=['-v',f'{state}:/var/lib/vaultsyncd:rw','-v',f'{roots}:/srv/vaultsyncd-roots:rw']
        flags=['--user',f'{UID}:{UID}','--read-only','--cap-drop=all','--security-opt','no-new-privileges',
               '--cpus','2','--pids-limit','256',*mounts]
        password=sys.stdin.readline().strip() if os.environ.get('VAULTSYNCD_TEST_PASSWORD_STDIN')=='1' else secrets.token_urlsafe(24)
        assert len(password)>=24
        init=engine('run','--rm','-i','--network','none',*flags,'--entrypoint','/opt/vaultsyncd/bin/vaultsyncd-admin',IMAGE,'init','--state-dir','/var/lib/vaultsyncd',input=password+'\n')
        assert 'owner' in init.stdout
        listen='0.0.0.0' if os.environ.get('VAULTSYNCD_TEST_ISOLATED_VM')=='1' else '127.0.0.1'
        engine('run','-d','--name',NAME,*flags,'-p',f'{listen}:{PORT}:8721',IMAGE);created=True
        wait(lambda:api('/v1/health')['status']=='ok')
        issued=api('/v1/auth/login',{'login':'owner','password':password,'device_label':'S098 Ubuntu actual acceptance'});TOKEN=issued['access_token']
        capabilities=api('/v1/capabilities');RESULT['server_id']=capabilities['server_id']
        facts=json.loads(engine('inspect',NAME).stdout)[0]
        status=engine('exec',NAME,'cat','/proc/1/status').stdout
        fields=dict(l.split(':',1) for l in status.splitlines() if ':' in l)
        assert all(int(n)==UID for n in fields['Uid'].split())
        assert all(int(fields[k].strip(),16)==0 for k in ['CapEff','CapBnd','CapAmb']) and fields['NoNewPrivs'].strip()=='1'
        engine('exec',NAME,'test','!','-e','/nix')
        engine('exec',NAME,'test','!','-e','/dev/fuse')
        RESULT['process']={'pid':facts['State']['Pid'],'uid':UID,'capabilities':0,'NoNewPrivs':1,'uname':engine('exec',NAME,'uname','-m').stdout.strip()}
        root=api('/v1/admin/roots',{'path':'/srv/vaultsyncd-roots'})['root_id'];RESULT['root_id']=root
        def head():return next((r for r in api(f'/v1/roots/{root}/recovery')['items'] if r['name']=='sample.txt' and r.get('hash')==hashlib.sha256(old).hexdigest()),None)
        entry=wait(head)
        content=(f'S098 {ARCH} portable committed '+secrets.token_hex(8)+'\n').encode()*512
        digest=hashlib.sha256(content).hexdigest()
        upload=api(f'/v1/roots/{root}/uploads',{'total_size':str(len(content)),'hash':digest,'chunk_size':'0'})
        api(f'/v1/roots/{root}/uploads/{upload["upload_id"]}/parts/0',content,method='PUT',headers={'content-type':'application/octet-stream','x-part-sha256':digest})
        api(f'/v1/roots/{root}/uploads/{upload["upload_id"]}/finalize',{})
        operation={'kind':'put_content','op_id':secrets.token_hex(16),'device_id':secrets.token_hex(16),
                   'server_epoch':api(f'/v1/roots/{root}/snapshots',{})['server_epoch'],'entry_id':entry['entry_id'],
                   'base_revision':entry['revision'],'upload_id':upload['upload_id']}
        committed=api(f'/v1/roots/{root}/operations',operation)
        if committed['state']=='pending':committed=wait(lambda:(x if x['state']!='pending' else None) if (x:=api(f'/v1/roots/{root}/operations/{operation["op_id"]}')) else None)
        assert committed['state']=='committed' and not committed.get('conflict')
        fixed=f'/v1/roots/{root}/entries/{entry["entry_id"]}/versions/{committed["version_id"]}/content'
        assert api(fixed,binary=True)==content and sudo('cat',sample,binary=True).stdout==content
        RESULT.update(committed=committed,sha256=digest)
        RESULT['checks'].append('actual-Ubuntu-server-admin-init-observer-upload-finalize-committed-fixed-content')
        backup=secrets.token_hex(16)
        assert admin('backup','prepare','--id',backup)['phase']=='ready'
        assert admin('backup','verify-source',backup)['integrity']=='verified'
        export=admin('backup','export',backup,'--output','/var/lib/vaultsyncd/accepted.vsbk')
        verified=json.loads(engine('exec','--user',f'{UID}:{UID}',NAME,'/opt/vaultsyncd/bin/vaultsyncd-admin','backup','verify','/var/lib/vaultsyncd/accepted.vsbk','--json').stdout)
        assert verified['state']=='verified'
        assert admin('backup','release',backup)['phase']=='released'
        RESULT['backup']={'export':export,'verified':verified}
        RESULT['checks'].append('unprivileged-real-btrfs-prepare-source-verify-export-archive-verify-release')
        engine('stop','--time','60',NAME);engine('start',NAME)
        wait(lambda:api('/v1/health')['status']=='ok')
        assert api('/v1/capabilities')['server_id']==capabilities['server_id']
        replay=api(f'/v1/roots/{root}/operations',operation)
        assert replay['state']=='committed' and replay['replayed'] and replay['version_id']==committed['version_id'] and replay['root_seq']==committed['root_seq']
        assert api(fixed,binary=True)==content and sudo('cat',sample,binary=True).stdout==content
        browser(password,root)
        RESULT['checks'].append('real-container-restart-keeps-identity-version-bytes-idempotency-and-Chromium-Web')
        api('/v1/auth/logout',{});TOKEN=None
        RESULT['server_log']=engine('logs',NAME).stdout
        RESULT['passed']=True
    finally:
        if TOKEN:
            try:api('/v1/auth/logout',{})
            except (OSError,urllib.error.URLError):pass
        if created:engine('stop','--time','60',NAME,check=False);engine('rm',NAME,check=False)
        if mounted:sudo('umount',mount)
        if loop:sudo('losetup','-d',loop)
        (RUN/'result.json').write_text(json.dumps(RESULT,ensure_ascii=False,indent=2))
        print(json.dumps({'passed':RESULT['passed'],'run':str(RUN),'checks':RESULT['checks']}))

if __name__=='__main__':
    try:
        configure(*sys.argv[1:])
        main()
    except Exception as error:
        print(json.dumps({'passed':False,'error_type':type(error).__name__,'frames':[{'line':f.lineno,'function':f.name} for f in traceback.extract_tb(error.__traceback__) if f.filename==__file__]}))
        sys.exit(1)
