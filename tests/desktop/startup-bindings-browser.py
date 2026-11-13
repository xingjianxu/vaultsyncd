#!/usr/bin/env python3
"""S226 main-desktop startup recovery and server HoverCard with explicit IPC doubles.

Windows native shutdown/CFAPI evidence remains in run-native-agent-exit.sh.
"""
import functools
import http.server
import importlib.util
import json
import os
from pathlib import Path
import threading

from playwright.sync_api import expect, sync_playwright

spec = importlib.util.spec_from_file_location("desktop_ui", Path(__file__).with_name("ui-browser.py"))
ui = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ui)
OUTPUT = Path(os.environ["VAULTSYNCD_UI_ARTIFACTS"]) / "startup-bindings"
FIXTURE = r"""() => {
  const server = '7'.repeat(32);
  const bindings = [1,2,3,4,5].map(n => ({
    binding_id: String(n).repeat(32), server_id: server, vault_id: '6'.repeat(32),
    cache_path: `C:\\S226\\cache${n}`, local_path: `C:\\S226\\folder${n}`,
    mode: 'full', paused: n !== 4, detached: n === 5,
  }));
  const f = window.fixture = {bindings, reads: 0, resumes: [], pauses: [], failReads: 1, failResume: true};
  window.__TAURI_INTERNALS__ = {invoke: async (command,args) => {
    if (command === 'agent_get_status') {
      f.reads++;
      if (f.failReads-- > 0) throw {message: 'S226 IPC temporarily unavailable'};
      return {bindings: bindings.map(b => ({binding_id: b.binding_id, paused: b.paused, detached: b.detached,
        synchronized: false, dirty: '1', pending: '0', event_lag: '0', failed: '0', intents: '0',
        materialized: '0', metadata_only: '0', mode_pending: false, offline: '0', partial: '0',
        recovery: '0', transfers: '0', unknown: '0'})), management: {
        connections: [{server_id: server, server_url: 'https://server.example.invalid', authenticated: true}],
        bindings: structuredClone(bindings), problems: [], notices: [],
      }};
    }
    if (command === 'agent_resume_binding') {
      f.resumes.push(args.bindingId);
      if (args.bindingId === '2'.repeat(32) && f.failResume) throw {message: 'S226 binding recovery rejected'};
      bindings.find(b=>b.binding_id===args.bindingId).paused = false;
      return {};
    }
    if (command === 'agent_pause_binding') {
      f.pauses.push(args.bindingId);
      bindings.find(b=>b.binding_id===args.bindingId).paused = true;
      return {};
    }
    if (command === 'agent_list_activity') return {activity: []};
    throw {message: `Unexpected command ${command}`};
  }};
}"""


def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(ui.DesktopStaticFiles, directory=str(ui.REPO / "apps/desktop/dist")))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    checks, errors = [], []
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            page = browser.new_page(viewport={"width": 1440, "height": 1000})
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.add_init_script(f"({FIXTURE})()")
            page.add_init_script("window.csp=[];document.addEventListener('securitypolicyviolation',e=>csp.push(e.effectiveDirective))")
            url = f"http://localhost:{server.server_port}/#/bindings"
            page.goto(url)
            expect(page.get_by_text("S226 IPC temporarily unavailable", exact=True)).to_be_visible()
            page.wait_for_function("() => fixture.resumes.length === 3")
            expect(page.get_by_role("button", name="添加同步目录", exact=True)).to_be_enabled()
            assert page.evaluate("fixture.resumes") == [str(n) * 32 for n in [1, 2, 3]]
            checks.append("transient first IPC failure retries; resumes paused active bindings across individual failure, skips running and detached")
            expect(page.get_by_text("部分同步目录未能自动恢复", exact=True)).to_be_visible()
            expect(page.get_by_text(r"C:\S226\folder2：S226 binding recovery rejected", exact=True)).to_be_visible()
            table = page.get_by_role("table", name="本地 Binding")
            expect(table.get_by_role("columnheader")).to_have_text(["本地目录", "服务器", "模式", "状态与合并进度", "操作"])
            expect(table.locator("tbody tr")).to_have_count(4)
            row = table.locator("tbody tr").first
            expect(row.get_by_role("button", name="暂停", exact=True)).to_be_enabled()
            checks.append("recovery re-reads actual state; five columns and no Vault column")

            target = row.locator("td").nth(1).locator("span[tabindex='0']")
            detail = page.get_by_text("Vault ID：" + "6" * 32, exact=True)
            expect(detail).to_have_count(0)
            target.hover()
            expect(detail).to_be_visible()
            expect(page.get_by_text("Server ID：" + "7" * 32, exact=True)).to_be_visible()
            expect(page.get_by_text("服务器地址：https://server.example.invalid", exact=True)).to_be_visible()
            expect(page.get_by_text("登录状态：已登录", exact=True)).to_be_visible()
            page.screenshot(path=str(OUTPUT / "server-hover.png"))
            page.mouse.move(0, 0)
            expect(detail).not_to_be_visible()
            row.get_by_role("button", name=r"C:\S226\folder1", exact=True).focus()
            page.keyboard.press("Tab")
            expect(target).to_be_focused()
            expect(detail).to_be_visible()
            page.keyboard.press("Escape")
            expect(detail).not_to_be_visible()
            checks.append("HoverCard address, server identity, Vault identity and login; keyboard Tab and Escape")

            row.get_by_role("button", name="暂停", exact=True).click()
            expect(row.get_by_role("button", name="恢复", exact=True)).to_be_enabled()
            page.get_by_role("link", name="连接设置", exact=True).click()
            page.get_by_role("link", name="同步目录", exact=True).click()
            page.wait_for_timeout(4500)
            expect(row.get_by_role("button", name="恢复", exact=True)).to_be_enabled()
            assert len(page.evaluate("fixture.resumes")) == 3
            # A newly added paused binding is not part of the startup snapshot either.
            page.evaluate("fixture.bindings.push({...fixture.bindings[0],binding_id:'a'.repeat(32),local_path:'C:\\\\S226\\\\later',paused:true})")
            page.get_by_role("button", name="检查本机连接", exact=True).click()
            expect(table.locator("tbody tr")).to_have_count(5)
            assert len(page.evaluate("fixture.resumes")) == 3
            checks.append("manual pause, navigation, repeated polls and later binding membership never rerun startup recovery")
            # Unknown server metadata still exposes durable IDs without inventing a URL/auth state.
            page.evaluate("fixture.bindings[0].server_id='9'.repeat(32)")
            page.get_by_role("button", name="检查本机连接", exact=True).click()
            unknown = row.locator("td").nth(1).locator("span[tabindex='0']")
            expect(unknown).to_have_text("9" * 32)
            page.set_viewport_size({"width": 420, "height": 900})
            unknown.hover()
            expect(page.get_by_text("服务器地址：未知", exact=True)).to_be_visible()
            expect(page.get_by_text("登录状态：未知", exact=True)).to_be_visible()
            expect(detail).to_be_visible()
            box = detail.locator("xpath=../..").bounding_box()
            assert box["width"] <= 420
            checks.append("missing connection preserves known IDs, reports unknown address/login and constrains narrow-screen card")
            assert not errors, errors
            assert not page.evaluate("csp"), page.evaluate("csp")
            checks.append("no JavaScript errors or CSP violations")
            browser.close()
    finally:
        server.shutdown()
    (OUTPUT / "result.json").write_text(json.dumps({"ok": True, "checks": checks}, ensure_ascii=False, indent=2))
    print(json.dumps({"ok": True, "checks": checks}, ensure_ascii=False))


if __name__ == "__main__":
    main()
