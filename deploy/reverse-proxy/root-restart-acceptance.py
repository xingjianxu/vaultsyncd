#!/usr/bin/env python3
"""Read-only trial data verification plus strict Chromium health after Root fence fix.
Args: evidence tls-tools before|test|switch|restart. Sessions revoked, no credentials saved.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import ssl
import subprocess
import sys
import traceback
import urllib.request
from playwright.sync_api import sync_playwright, expect

EVIDENCE, TOOLS = map(Path, sys.argv[1:3])
PHASE = sys.argv[3]
assert PHASE in ('before','test','switch','restart')
ROOT='58e698e16616ff0162055df963f9c1fa'
BASE=Path('/data/vaultsyncd-trial-s096/mnt')
ORIGIN='https://localhost:18443'
def command(args):
    r=subprocess.run(list(map(str,args)),text=True,capture_output=True,timeout=60)
    assert r.returncode==0, 'command failed'
    return r.stdout.strip()

def main():
    token=None
    tls=ssl.create_default_context(cafile=str(EVIDENCE/'development-ca.pem'))
    def api(path, body=None, binary=False):
        headers={}
        if token:headers['Authorization']='Bearer '+token
        if body is not None:body=json.dumps(body).encode();headers['content-type']='application/json'
        with urllib.request.urlopen(urllib.request.Request(ORIGIN+path,data=body,headers=headers),context=tls,timeout=20) as r:
            return r.read() if binary else json.load(r)
    password=command(['sudo','-n','cat','/var/lib/vaultsyncd-trial-credentials/owner.password'])
    token=api('/v1/auth/login',{'login':'owner','password':password,'device_label':'S097 read-only restart acceptance'})['access_token']
    try:
        managed=next(r for r in api('/v1/admin/roots')['roots'] if r['root_id']==ROOT)
        identity=command(['sudo','-n','-u','vaultsyncd-trial','sqlite3','-readonly',BASE/'state/server.sqlite',"SELECT cast(root_file_identity as text) FROM roots WHERE lower(hex(id))='"+ROOT+"'"])
        contents={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((BASE/'samples').iterdir()) if p.is_file()}
        checks=[]
        for label in ('linux-chromium','windows-edge'):
            prior=json.loads((EVIDENCE/f'{label}.json').read_text())
            op=prior['committed'];entry=op['entry_id'];version=op['version_id']
            assert api(f'/v1/roots/{ROOT}/operations/{op["op_id"]}')['state']=='committed'
            assert hashlib.sha256(api(f'/v1/roots/{ROOT}/entries/{entry}/versions/{version}/content',binary=True)).hexdigest()==prior['sha256']
            checks.append(label+'-old-committed-operation-and-fixed-version-still-valid')
        result={'passed':True,'phase':PHASE,'root':ROOT,'server_id':api('/v1/capabilities')['server_id'],'managed':managed,'historical_identity':identity,'contents':contents,'checks':checks}
        if PHASE!='before':
            prior=json.loads((EVIDENCE/'root-restart-before.json').read_text())
            assert result['server_id']==prior['server_id']
            assert identity==prior['historical_identity'] and contents==prior['contents']
            assert managed['root_seq']==prior['managed']['root_seq'] and managed['health']=='healthy'
            result['checks'].append('same-server-root-seq-historical-blob-and-all-sample-bytes-healthy')
            runtime=EVIDENCE/f'root-browser-private-{os.getpid()}';runtime.mkdir(mode=0o700)
            try:
                nss=runtime/'.pki/nssdb';nss.mkdir(parents=True)
                command([TOOLS/'bin/certutil','-N','-d','sql:'+str(nss),'--empty-password'])
                command([TOOLS/'bin/certutil','-A','-d','sql:'+str(nss),'-n','vaultsyncd S097 CA','-t','C,,','-i',EVIDENCE/'development-ca.pem'])
                with sync_playwright() as p:
                    browser=p.chromium.launch(headless=True,env={**os.environ,'HOME':str(runtime)},args=['--no-proxy-server'])
                    context=browser.new_context();page=context.new_page()
                    try:
                        response=page.goto(ORIGIN+'/#/account')
                        assert page.evaluate('window.isSecureContext') and response.security_details()['issuer']=='vaultsyncd S097 explicit development CA'
                        page.get_by_label('登录名',exact=True).fill('owner');page.get_by_label('密码',exact=True).fill(password)
                        page.get_by_role('button',name='登录',exact=True).click()
                        expect(page.get_by_text('已登录：owner',exact=True)).to_be_visible(timeout=20000)
                        page.get_by_role('link',name='Root 目录',exact=True).click()
                        page.wait_for_function('root=>fetch("/v1/admin/roots").then(r=>r.json()).then(d=>d.roots.some(x=>x.root_id===root&&x.health==="healthy"&&x.collection.state==="complete"))',arg=ROOT)
                        # Wait for React's own fetched rows, not only the API predicate.
                        expect(page.get_by_text('mount_changed',exact=False)).to_have_count(0)
                        expect(page.get_by_text('挂载核对通过',exact=True)).to_be_visible()
                        expect(page.get_by_text('本进程初始采集完成',exact=True)).to_be_visible()
                        page.screenshot(path=str(EVIDENCE/f'root-restart-{PHASE}.png'),full_page=True)
                        result['browser']=browser.version
                        result['checks'].append('strict-trusted-CA-real-Chromium-login-and-healthy-Root-page')
                    finally:
                        try:page.evaluate('fetch("/v1/auth/logout",{method:"POST",headers:{"content-type":"application/json"},body:"{}"}).then(r=>r.status)')
                        finally:context.close();browser.close()
            finally:shutil.rmtree(runtime)
            result['managed']=next(r for r in api('/v1/admin/roots')['roots'] if r['root_id']==ROOT)
        (EVIDENCE/f'root-restart-{PHASE}.json').write_text(json.dumps(result,indent=2,ensure_ascii=False))
        print(json.dumps({'passed':True,'phase':PHASE,'managed':managed,'checks':result['checks']},ensure_ascii=False))
    finally:api('/v1/auth/logout',{})

if __name__=='__main__':
    try:main()
    except Exception as error:
        print(json.dumps({'passed':False,'error_type':type(error).__name__,'frames':[{'line':f.lineno,'function':f.name} for f in traceback.extract_tb(error.__traceback__) if f.filename==__file__]}))
        sys.exit(1)
