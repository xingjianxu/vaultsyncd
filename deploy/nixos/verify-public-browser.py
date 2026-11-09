#!/usr/bin/env python3
"""Strict public TLS, browser login and SSE acceptance. Never export cookies/HAR."""
import json
import re
from pathlib import Path
import subprocess
import sys
import traceback
from playwright.sync_api import sync_playwright, expect


def main():
    origin, evidence, receipt = sys.argv[1:]
    evidence = Path(evidence).resolve(strict=True)
    vault = json.loads(Path(receipt).read_text())['vault_id']
    result = {'origin': origin, 'checks': []}
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True, args=['--no-proxy-server'])
        context = browser.new_context()
        page = context.new_page()
        logged_in = False
        try:
            response = page.goto(origin + '/#/account')
            assert response.status == 200
            assert page.evaluate('window.isSecureContext')
            result['tls'] = response.security_details()
            assert result['tls'] and result['tls']['issuer']
            password = subprocess.check_output(['sudo', '-n', 'cat', '/var/lib/vaultsyncd-credentials/owner.password'], text=True).strip()
            page.get_by_label(re.compile(r'^登录名(?:\s*\*)?$')).fill('owner')
            page.get_by_label(re.compile(r'^密码(?:\s*\*)?$')).fill(password)
            del password
            page.get_by_role('button', name='登录', exact=True).click()
            expect(page.get_by_text('已登录：owner', exact=True)).to_be_visible(timeout=20000)
            logged_in = True
            session = [c for c in context.cookies() if c['name'] in ('vaultsyncd_session', 'vaultsyncd_refresh')]
            assert len(session) == 2
            assert all(c['secure'] and c['httpOnly'] and c['sameSite'] == 'Strict' for c in session)
            assert 'vaultsyncd_session' not in page.evaluate('document.cookie')
            result['cookies'] = [{k: c[k] for k in ('name', 'secure', 'httpOnly', 'sameSite', 'path')} for c in session]
            result['checks'].append('public-CA-strict-browser-TLS-and-ui-login-secure-cookie')
            page.evaluate('''vault => {
              window.acceptance={controller:new AbortController(),text:'',failed:false};
              fetch(`/v1/vaults/${vault}/events`,{signal:window.acceptance.controller.signal}).then(async r=>{
                if(!r.ok||!r.headers.get('content-type').startsWith('text/event-stream'))throw Error('SSE headers');
                const reader=r.body.getReader(),decoder=new TextDecoder();
                while(true){const {value,done}=await reader.read();if(done)break;
                  window.acceptance.text+=decoder.decode(value,{stream:true});
                  if(window.acceptance.text.length>65536)throw Error('SSE bound');}
              }).catch(e=>{if(e.name!=='AbortError')window.acceptance.failed=true});
            }''', vault)
            page.wait_for_function("window.acceptance.text.includes('event: seq')", timeout=10000)
            page.wait_for_timeout(16000)
            assert page.evaluate("() => /(^|\\n):/.test(window.acceptance.text)")
            assert not page.evaluate('window.acceptance.failed')
            page.evaluate('window.acceptance.controller.abort()')
            result['checks'].append('authenticated-same-origin-SSE-initial-seq-and-heartbeat')
            for path in ('/admin/socket', '/server.sqlite', '/etc/passwd', '/var/lib/vaultsyncd-credentials/owner.password'):
                assert page.request.get(origin + path).status == 404
            result['checks'].append('no-management-socket-database-or-filesystem-exposure')
            result['passed'] = True
            (evidence / 'browser.json').write_text(json.dumps(result, indent=2))
            print(json.dumps(result, indent=2))
        finally:
            if logged_in:
                response = page.evaluate("async () => (await fetch('/v1/auth/logout',{method:'POST',headers:{'content-type':'application/json'},body:'{}'})).status")
                assert response < 300
            context.close()
            browser.close()


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        frames = [{'file': Path(f.filename).name, 'line': f.lineno} for f in traceback.extract_tb(error.__traceback__)
                  if f.filename == __file__]
        print(json.dumps({'passed': False, 'error_type': type(error).__name__, 'frames': frames}))
        sys.exit(1)
