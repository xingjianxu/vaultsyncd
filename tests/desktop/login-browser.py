#!/usr/bin/env python3
"""S176 presentation/interaction checks. Real session persistence is tested on Windows."""
import functools
import http.server
import importlib.util
import json
import os
from pathlib import Path
import threading
from playwright.sync_api import expect, sync_playwright

spec = importlib.util.spec_from_file_location('desktop_ui', Path(__file__).with_name('ui-browser.py'))
ui = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ui)
OUTPUT = Path(os.environ['VAULTSYNCD_UI_ARTIFACTS']) / 'login'
FIXTURE = r'''() => {
  window.fixture = { auth: false, ready: false, fail: true, calls: [], username: undefined };
  const state = window.fixture;
  const connection = () => ({ server_id: '7'.repeat(32), server_url: 'https://example.invalid', authenticated: state.auth, username: state.username });
  window.__TAURI_INTERNALS__ = { invoke: async (command, args) => {
    state.calls.push(command);
    if (command === 'agent_get_status') {
      while (!state.ready) await new Promise(resolve => setTimeout(resolve, 50));
      return { bindings: [], management: { connection: connection(), bindings: [], problems: [], notices: [] } };
    }
    if (command === 'agent_discover_connection') return { connection: { ...connection(), server_url: args.input.serverUrl, ca_file: args.input.caFile } };
    if (command === 'agent_set_connection') return { connection: connection() };
    if (command === 'agent_login') {
      if (state.fail) throw { message: '用户名或密码错误' };
      state.auth = true; state.username = args.username;
      return { connection: connection() };
    }
    if (command === 'agent_logout') { state.auth = false; return { connection: connection() }; }
    throw { message: `unexpected command ${command}` };
  }};
}'''

def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(ui.DesktopStaticFiles, directory=str(ui.REPO / 'apps/desktop/dist')))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = f'http://localhost:{server.server_port}'
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            page = browser.new_page(viewport={'width': 390, 'height': 900})
            errors = []
            page.on('pageerror', lambda e: errors.append(str(e)))
            page.add_init_script('window.csp=[]; document.addEventListener("securitypolicyviolation", e=>window.csp.push(e.effectiveDirective));')
            page.goto(origin + '/#/files')
            modal = page.get_by_role('dialog', name='登录', exact=True)
            expect(modal).to_be_visible()
            expect(page.get_by_role('button', name='重试', exact=True)).to_be_visible()
            expect(page.locator('.mantine-AppShell-root')).to_have_count(0)
            page.keyboard.press('Escape')
            expect(modal).to_be_visible()
            page.add_init_script('(' + FIXTURE + ')()')
            page.goto(origin + '/#/bindings')
            page.reload()
            expect(modal.get_by_role('status')).to_have_text('正在读取登录状态…')
            expect(page.locator('.mantine-AppShell-root')).to_have_count(0)
            assert page.evaluate('fixture.calls.every(c=>c==="agent_get_status")')
            page.evaluate('fixture.ready=true')
            expect(page.locator('[name="username"]')).to_be_visible()
            expect(page.locator('[name="server_id"]')).to_have_value('7' * 32)
            for _ in range(12):
                page.keyboard.press('Tab')
                assert modal.evaluate('e=>e.contains(document.activeElement)')
            page.locator('[name="username"]').fill('用户甲')
            page.locator('[name="password"]').fill('test-password')
            page.get_by_role('button', name='登录', exact=True).click()
            expect(modal.get_by_role('alert')).to_have_text('用户名或密码错误')
            expect(page.locator('[name="password"]')).to_have_value('')
            expect(page.locator('.mantine-AppShell-root')).to_have_count(0)
            page.evaluate('fixture.fail=false')
            page.locator('[name="password"]').fill('test-password')
            page.get_by_role('button', name='登录', exact=True).click()
            expect(modal).to_have_count(0)
            expect(page.locator('.mantine-AppShell-root')).to_be_visible()
            assert page.evaluate('location.hash') == '#/bindings'
            page.evaluate('location.hash="/connections"')
            page.get_by_role('button', name='退出登录', exact=True).click()
            expect(modal).to_be_visible()
            expect(page.locator('[name="username"]')).to_have_value('用户甲')
            expect(page.locator('[name="password"]')).to_have_value('')
            expect(page.locator('.mantine-AppShell-root')).to_have_count(0)
            # A saved session skips login, then a confirmed reauth status removes the whole shell.
            page.evaluate('fixture.auth=true')
            expect(page.locator('.mantine-AppShell-root')).to_be_visible()
            page.evaluate('fixture.auth=false')
            expect(modal).to_be_visible()
            expect(page.locator('.mantine-AppShell-root')).to_have_count(0)
            for scheme in ('light', 'dark'):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({'width': width, 'height': 900})
                    assert page.evaluate('document.documentElement.scrollWidth<=innerWidth')
                    page.screenshot(path=str(OUTPUT / f'login-{scheme}-{width}.png'), full_page=True)
            assert not errors, errors
            assert not page.evaluate('window.csp'), page.evaluate('window.csp')
            assert page.evaluate('Object.keys(localStorage).every(k=>k==="mantine-color-scheme-value") && sessionStorage.length===0')
            (OUTPUT / 'result.json').write_text(json.dumps({'ok': True, 'checks': ['IPC failure', 'pending status gate', 'focus trap', 'wrong password', 'successful login', 'deep link', 'logout', 'saved session', 'reauth gate', '320/1440 light/dark', 'CSP', 'no credential storage']}))
            browser.close()
    finally:
        server.shutdown()
    print(f'S176 login browser passed: {OUTPUT}')

if __name__ == '__main__':
    main()
