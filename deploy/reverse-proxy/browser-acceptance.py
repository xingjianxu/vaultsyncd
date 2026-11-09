#!/usr/bin/env python3
"""Real strict-TLS Chromium/Windows Edge same-origin acceptance; no certificate bypass.
Args: evidence-dir tls-tools-dir [CDP-url origin label]. No cookies/HAR/profiles exported.
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
import ssl
import urllib.request
import urllib.error
import traceback
from playwright.sync_api import sync_playwright, expect, Error

EVIDENCE = Path(sys.argv[1]).resolve(strict=True)
TOOLS = Path(sys.argv[2]).resolve(strict=True)
CDP = sys.argv[3] if len(sys.argv) > 3 else None
ORIGIN = sys.argv[4] if len(sys.argv) > 4 else 'https://localhost:18443'
LABEL = sys.argv[5] if len(sys.argv) > 5 else 'linux-chromium'
assert ORIGIN in ('https://localhost:18443', 'https://10.0.2.2:18443')
assert LABEL in ('linux-chromium', 'windows-edge')
VAULT = '58e698e16616ff0162055df963f9c1fa'
SERVER = '9e9d06c5d0d371a43c2cb4043de02cd8'
SAMPLES = Path('/data/vaultsyncd-trial-s096/mnt/samples')
CA = EVIDENCE/'development-ca.pem'


def command(args):
    r = subprocess.run(list(map(str,args)), text=True, capture_output=True, timeout=60)
    assert r.returncode == 0, (args, r.stderr)
    return r.stdout


def run(browser, result):
    context = browser.new_context()  # Strict defaults: ignore_https_errors is never enabled.
    page = context.new_page()
    try:
        try:page.goto(ORIGIN.replace(':18443',':18444'),timeout=10000)
        except Error as error:assert 'CERT_COMMON_NAME_INVALID' in str(error)
        else:raise AssertionError('trusted CA unexpectedly disabled certificate hostname verification')
        result['checks'].append('explicitly-trusted-CA-still-rejects-wrong-certificate-hostname')
        page.close();page=context.new_page()
        response = page.goto(ORIGIN+'/#/account')
        details = response.security_details()
        assert details and 'vaultsyncd S097 explicit development CA' in details['issuer']
        assert page.evaluate('window.isSecureContext')
        password = command(['sudo','-n','cat','/var/lib/vaultsyncd-trial-credentials/owner.password']).strip()
        page.get_by_label('登录名',exact=True).fill('owner')
        page.get_by_label('密码',exact=True).fill(password)
        page.get_by_role('button',name='登录',exact=True).click()
        expect(page.get_by_text('已登录：owner',exact=True)).to_be_visible(timeout=20000)
        cookies = context.cookies()
        session = [c for c in cookies if c['name'] in ('vaultsyncd_session','vaultsyncd_refresh')]
        assert len(session) == 2 and all(c['httpOnly'] and c['secure'] and c['sameSite']=='Strict' for c in session)
        assert 'vaultsyncd_session' not in page.evaluate('document.cookie')
        result['cookies'] = [{k:c[k] for k in ('name','httpOnly','secure','sameSite','path')} for c in session]
        result['tls'] = details
        result['checks'].append('real-browser-TLS-ui-login-secure-httponly-strict-cookie')
        def api(path, body=None):
            return page.evaluate('''async ([path,body]) => {
              const r=await fetch(path,{method:body?'POST':'GET',headers:body?{'content-type':'application/json'}:undefined,body:body?JSON.stringify(body):undefined});
              const value=await r.json(); if(!r.ok)throw new Error(JSON.stringify(value));return value;
            }''',[path,body])
        assert api('/v1/capabilities')['server_id'] == SERVER
        sample = SAMPLES/f's097-{LABEL}-{os.getpid()}.txt'
        with sample.open('xb') as f: f.write(b'S097 original disposable browser fixture\n')
        sample.chmod(0o600)
        command(['setfacl','-m','u:vaultsyncd-trial:rw',sample])
        head = None
        for _ in range(150):
            head = next((x for x in api(f'/v1/vaults/{VAULT}/recovery')['items'] if x['name']==sample.name),None)
            if head:break
            time.sleep(.2)
        assert head
        # Streaming fetch is authenticated with actual same-origin browser cookies.
        page.evaluate('''vault => {
          window.s097={controller:new AbortController(),text:'',ready:false,failed:null};
          fetch(`/v1/vaults/${vault}/events`,{signal:window.s097.controller.signal}).then(async r=>{
            if(!r.ok||!r.headers.get('content-type').startsWith('text/event-stream'))throw Error('SSE headers');
            window.s097.ready=true;const reader=r.body.getReader(),decoder=new TextDecoder();
            while(true){const {value,done}=await reader.read();if(done)break;
              window.s097.text+=decoder.decode(value,{stream:true});if(window.s097.text.length>65536)throw Error('SSE bound');}
          }).catch(e=>{if(e.name!=='AbortError')window.s097.failed=String(e)});
        }''',VAULT)
        def until(expression, arg=None, timeout=6):
            end=time.monotonic()+timeout
            while time.monotonic()<end:
                if page.evaluate(expression,arg):return
                time.sleep(.1)
            raise AssertionError('browser SSE did not converge')
        until("() => window.s097.ready&&window.s097.text.includes('event: seq')")
        # Keep the real stream alive beyond the backend's 15-second heartbeat interval.
        page.wait_for_timeout(16000)
        assert page.evaluate("!window.s097.failed && /(^|\\n):/.test(window.s097.text)")
        result['checks'].append('SSE-initial-seq-and-15s-heartbeat-not-buffered-or-timed-out')
        maximum = api('/v1/capabilities')['limits']['max_upload_part_bytes']
        assert maximum == '16777216'
        byte = 0x62 if LABEL=='linux-chromium' else 0x77
        digest = page.evaluate('''async value=>{window.s097.content=new Uint8Array(16777216).fill(value);
          return Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256',window.s097.content))).map(b=>b.toString(16).padStart(2,'0')).join('');}''',byte)
        upload = api(f'/v1/vaults/{VAULT}/uploads',{'total_size':maximum,'hash':digest,'chunk_size':maximum})
        part = f'/v1/vaults/{VAULT}/uploads/{upload["upload_id"]}/parts/0'
        too_large = page.evaluate('''async ([url,hash])=>{const r=await fetch(url,{method:'PUT',headers:{'content-type':'application/octet-stream','x-part-sha256':hash},body:new Uint8Array(16777217)});await r.text();return r.status;}''',[part,digest])
        assert too_large == 413, f'proxy maximum+1 must be 413: {too_large}'
        accepted = page.evaluate('''async ([url,hash])=>{const r=await fetch(url,{method:'PUT',headers:{'content-type':'application/octet-stream','x-part-sha256':hash},body:window.s097.content});const value=await r.json();if(!r.ok)throw Error(JSON.stringify(value));return r.status;}''',[part,digest])
        assert accepted == 200
        api(f'/v1/vaults/{VAULT}/uploads/{upload["upload_id"]}/finalize',{})
        epoch = api(f'/v1/vaults/{VAULT}/snapshots',{})['server_epoch']
        operation={'kind':'put_content','op_id':secrets.token_hex(16),'device_id':secrets.token_hex(16),'server_epoch':epoch,'entry_id':head['entry_id'],'base_revision':head['revision'],'upload_id':upload['upload_id']}
        committed = api(f'/v1/vaults/{VAULT}/operations',operation)
        for _ in range(100):
            if committed['state']!='pending':break
            time.sleep(.1);committed=api(f'/v1/vaults/{VAULT}/operations/{operation["op_id"]}')
        assert committed['state']=='committed' and not committed.get('conflict')
        until('seq => Array.from(window.s097.text.matchAll(/"seq":"(\\d+)"/g)).some(m => BigInt(m[1]) >= BigInt(seq))', committed['vault_seq'])
        fixed=f'/v1/vaults/{VAULT}/entries/{head["entry_id"]}/versions/{committed["version_id"]}/content'
        downloaded = page.evaluate('''async url=>{const r=await fetch(url);if(!r.ok)throw Error('download');const data=await r.arrayBuffer();return {size:data.byteLength,hash:Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256',data))).map(b=>b.toString(16).padStart(2,'0')).join('')}}''',fixed)
        assert downloaded=={'size':16777216,'hash':digest}
        assert hashlib.sha256(sample.read_bytes()).hexdigest()==digest
        result['checks'].append('16MiB-TLS-part-accepted-maximum-plus-one-413-finalize-committed-and-fixed-download')
        result['checks'].append('SSE-promptly-signals-new-committed-VaultSeq-during-upload')
        # The browser engine uses its NSS/Windows trust store. A separate strict
        # checker supplies the CA explicitly and never prints Cookie headers.
        cookie_header='; '.join(c['name']+'='+c['value'] for c in cookies)
        tls=ssl.create_default_context(cafile=str(CA))
        def header_check(origin):
            h={'Cookie':cookie_header,'Content-Type':'application/json','Origin':origin,
               'Host':ORIGIN.removeprefix('https://'),'X-Forwarded-Host':'evil.invalid',
               'X-Forwarded-Proto':'http','Forwarded':'host=evil.invalid;proto=http'}
            q=urllib.request.Request('https://localhost:18443'+f'/v1/vaults/{VAULT}/snapshots',data=b'{}',headers=h)
            try:
                with urllib.request.urlopen(q,context=tls,timeout=15) as response:return response.status
            except urllib.error.HTTPError as error:return error.code
        assert header_check('https://evil.invalid')==403
        assert header_check(ORIGIN)==201
        result['checks'].append('cross-origin-write-denied-forwarded-spoof-does-not-change-authority')
        statuses={}
        for path in ['/admin/socket','/v1/admin/socket','/state/server.sqlite','/etc/passwd','/var/lib/vaultsyncd-trial-credentials/owner.password','/v1/not-real']:
            status=page.evaluate('async path => (await fetch(path)).status',path)
            assert status==404,(path,status);statuses[path]=status
        result['path_status']=statuses
        result['checks'].append('no-management-socket-database-arbitrary-file-or-HTML-API-fallback')
        page.evaluate('window.s097.controller.abort()')
        page.get_by_role('link',name='Vault 目录',exact=True).click()
        page.screenshot(path=str(EVIDENCE/f'{LABEL}.png'),full_page=True)
        api('/v1/auth/logout',{})
        result.update(passed=True,origin=ORIGIN,browser=browser.version,server_id=SERVER,committed=committed,sha256=digest)
        (EVIDENCE/f'{LABEL}.json').write_text(json.dumps(result,indent=2,ensure_ascii=False))
        print(json.dumps(result,indent=2,ensure_ascii=False))
    finally:
        # Revoke this browser's own session even when an assertion fails.
        try:page.evaluate("fetch('/v1/auth/logout',{method:'POST',headers:{'content-type':'application/json'},body:'{}'}).then(r=>r.status)")
        except Error:pass
        context.close()


def main():
    result={'passed':False,'label':LABEL,'checks':[]}
    runtime=EVIDENCE/f'browser-runtime-{os.getpid()}'
    runtime.mkdir(mode=0o700)
    try:
        with sync_playwright() as p:
            if CDP:
                browser=p.chromium.connect_over_cdp(CDP)
                try:run(browser,result)
                finally:browser.close()
            else:
                env={**os.environ,'HOME':str(runtime)}
                browser=p.chromium.launch(headless=True,env=env,args=['--no-proxy-server'])
                try:
                    page=browser.new_page()
                    try:page.goto(ORIGIN,timeout=10000)
                    except Error as e:assert 'CERT_AUTHORITY_INVALID' in str(e),str(e)
                    else:raise AssertionError('fresh browser unexpectedly trusted private CA')
                    result['checks'].append('fresh-browser-untrusted-private-CA-fails-closed')
                finally:browser.close()
                nss=runtime/'.pki/nssdb';nss.mkdir(parents=True)
                command([TOOLS/'bin/certutil','-N','-d','sql:'+str(nss),'--empty-password'])
                command([TOOLS/'bin/certutil','-A','-d','sql:'+str(nss),'-n','vaultsyncd S097 explicit development CA','-t','C,,','-i',CA])
                browser=p.chromium.launch(headless=True,env=env,args=['--no-proxy-server'])
                try:run(browser,result)
                finally:browser.close()
    finally:
        shutil.rmtree(runtime)

if __name__=='__main__':
    try:main()
    except Exception as error:
        # Playwright's formatted exceptions may contain Cookie/Authorization.
        # Retain code locations only; never serialize its request call log.
        frames=[{'file':Path(f.filename).name,'line':f.lineno,'function':f.name} for f in traceback.extract_tb(error.__traceback__) if 'browser-acceptance.py' in f.filename]
        print(json.dumps({'passed':False,'error_type':type(error).__name__,'frames':frames}))
        sys.exit(1)
