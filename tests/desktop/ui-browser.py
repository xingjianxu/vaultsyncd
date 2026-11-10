#!/usr/bin/env python3
"""S152: built desktop UI in Chromium, with strict nonce CSP.

The disconnected layout uses no IPC substitute. The explicit presentation
fixture below exercises safety affordances and modal focus only; real Windows
IPC, TLS, CFAPI and notification evidence comes from run-native-*.sh.
Run: nix develop .#web-tests --command python3 tests/desktop/ui-browser.py
"""
import functools
import http.server
import io
import json
import os
from pathlib import Path
import re
import secrets
import threading
import time

from playwright.sync_api import expect, sync_playwright

REPO = Path(__file__).resolve().parents[2]
OUTPUT = Path(os.environ.get("VAULTSYNCD_UI_ARTIFACTS", "/data/vaultsyncd-dev/vaultsyncd-temp/S152-desktop-mantine/evidence")) / f"desktop-ui-{time.time_ns()}"
CONFIG = json.loads((REPO / "apps/desktop/src-tauri/tauri.conf.json").read_text())
POLICY = CONFIG["app"]["security"]["csp"]
ROUTES = {"/connections": "连接设置", "/bindings": "同步目录", "/files": "文件状态与操作", "/history": "历史与回收站", "/problems": "问题与恢复"}


class DesktopStaticFiles(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send_head(self):
        self.policy = POLICY
        if self.path.split("?", 1)[0] in ("/", "/index.html"):
            nonce = secrets.token_hex(16)
            self.policy = re.sub(r"style-src[^;]*", f"style-src 'self' 'nonce-{nonce}'", self.policy)
            html = (Path(self.directory) / "index.html").read_text()
            meta = f'<meta name="csp-nonce" content="{nonce}">'
            if re.search(r'<meta\s+name="csp-nonce"[^>]*>', html):
                html = re.sub(r'<meta\s+name="csp-nonce"[^>]*>', meta, html, count=1)
            else:
                html = html.replace("</head>", meta + "</head>", 1)
            body = html.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            return io.BytesIO(body)
        return super().send_head()

    def end_headers(self):
        self.send_header("Content-Security-Policy", self.policy)
        super().end_headers()


PRESENTATION_FIXTURE = r"""() => {
  const binding = { binding_id: '1'.repeat(32), vault_id: '2'.repeat(32), cache_path: 'C:\\S152\\cache', local_path: 'C:\\S152\\同步目录', mode: 'on_demand', paused: false, detached: false };
  const state = { binding_id: binding.binding_id, detached: false, paused: false, synchronized: false, dirty: '1', pending: '1', event_lag: '0', failed: '0', intents: '0', materialized: '1', metadata_only: '1', mode_pending: false, offline: '0', partial: '0', recovery: '0', transfers: '0', unknown: '0' };
  const clean = { availability: 'online', busy: false, dirty: false, entry_id: '3'.repeat(32), path: 'clean.txt', pending: false, release_allowed: true, release_reason: '远端已确认，本地内容可手动释放', remote_apply: 'current', residency: 'materialized', status: 'downloaded', synchronized: true, transfer: 'idle' };
  const dirty = { ...clean, entry_id: '4'.repeat(32), path: 'pending.txt', pending: true, dirty: true, release_allowed: false, release_reason: '本地内容尚未确认', synchronized: false, status: 'local_changes' };
  const longName = { ...clean, entry_id: '8'.repeat(32), path: 'longfilename'.repeat(20) + '.txt' };
  const item = { id: '5'.repeat(32), entry_id: clean.entry_id, name: 'clean.txt', revision: '1', version_id: '6'.repeat(32), size: '16', expires_at: '2000000000' };
  window.__S152_CALLS__ = [];
  window.__TAURI_INTERNALS__ = { invoke: async (command, args) => {
    window.__S152_CALLS__.push({ command, args });
    switch (command) {
      case 'agent_discover_connection': return { connection: { authenticated: true, server_id: '7'.repeat(32), server_url: 'https://example.invalid' } };
      case 'agent_get_status': return { agent_version: 'S152 presentation fixture', ipc_protocol: { major: 2, minor: 7 }, snapshot_version: 'fixture:1', bindings: [state], management: { connection: { authenticated: true, server_id: '7'.repeat(32), server_url: 'https://example.invalid' }, bindings: [binding], problems: [], notices: [] } };
      case 'agent_list_files': if (window.__S152_FAIL_FILES__) throw { message: 'S152 presentation: file read failed' }; return { files: [clean, dirty, longName] };
      case 'agent_list_history': case 'agent_list_trash': return { items: [item] };
      case 'agent_restore_entry': throw { message: 'PENDING: S152 presentation restore not committed' };
      case 'agent_list_problems': return { problems: [] };
      case 'agent_notification_settings': return { enabled: false, delivered: 0, blocked: 0 };
      default: throw { message: `S152 presentation fixture rejects mutation: ${command}` };
    }
  }};
}"""


def nonce_and_layout(page, scheme, width, route):
    if page.locator(".mantine-AppShell-root").count():
        expect(page.get_by_role("heading", level=1, name=ROUTES[route], exact=True)).to_be_visible()
    else:
        expect(page.get_by_role("dialog", name="登录", exact=True)).to_be_visible()
        expect(page.get_by_role("button", name="重试", exact=True)).to_be_visible()
        assert page.get_by_role("navigation").count() == 0
    expect(page.locator("html")).to_have_attribute("data-mantine-color-scheme", scheme)
    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width, route, "document overflow")
    if page.locator(".mantine-AppShell-header").count():
        assert page.locator(".mantine-AppShell-header").bounding_box()["height"] == 48
    nonce = page.locator('meta[name="csp-nonce"]').get_attribute("content")
    assert re.fullmatch(r"[0-9a-f]{32}", nonce), nonce
    assert page.evaluate("nonce => [...document.querySelectorAll('style')].every(style => style.nonce === nonce)", nonce)


def trap_and_cancel(page, trigger, title):
    trigger.click()
    modal = page.get_by_role("dialog", name=title, exact=True)
    expect(modal).to_be_visible()
    page.wait_for_timeout(250)
    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (title, "modal document overflow")
    for _ in range(10):
        page.keyboard.press("Tab")
        assert modal.evaluate("element => element.contains(document.activeElement)"), (title, "focus escaped modal")
    page.keyboard.press("Escape")
    expect(modal).to_be_hidden()
    expect(trigger).to_be_focused()


def main():
    OUTPUT.mkdir(parents=True)
    print(f"S152 desktop UI evidence: {OUTPUT}", flush=True)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(DesktopStaticFiles, directory=str(REPO / "apps/desktop/dist")))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = f"http://localhost:{server.server_port}"
    errors, cases = [], []
    presentation_only = os.environ.get("VAULTSYNCD_UI_PRESENTATION_ONLY") == "1"
    try:
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            page = browser.new_page(viewport={"width": 1440, "height": 1000})
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.add_init_script("window.__S152_CSP__ = []; document.addEventListener('securitypolicyviolation', e => window.__S152_CSP__.push({directive:e.effectiveDirective, blocked:e.blockedURI}));")
            for scheme in (() if presentation_only else ("light", "dark")):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 390, 768, 1440, 1920):
                    page.set_viewport_size({"width": width, "height": 900})
                    for route in ROUTES:
                        page.goto(origin + "/#" + route)
                        page.reload()
                        nonce_and_layout(page, scheme, width, route)
                        assert not page.locator(".mantine-AppShell-root").count()
                        assert not page.get_by_text("已同步", exact=True).count()
                        assert not page.evaluate("window.__S152_CSP__"), page.evaluate("window.__S152_CSP__")
                        cases.append({"theme": scheme, "width": width, "route": route})
                        page.screenshot(path=str(OUTPUT / f"{route[1:]}-{scheme}-{width}.png"), full_page=True)
            page.set_viewport_size({"width": 1440, "height": 1000})
            page.add_init_script("(" + PRESENTATION_FIXTURE + ")()")
            page.goto(origin + "/#/connections")
            page.reload()
            expect(page.get_by_role("heading", level=1, name="连接设置", exact=True)).to_be_visible()
            page.keyboard.press("Tab")
            expect(page.get_by_role("link", name="跳到主要内容", exact=True)).to_be_focused()
            page.keyboard.press("Enter")
            expect(page.locator("#main-content")).to_be_focused()
            scheme_before = page.locator("html").get_attribute("data-mantine-color-scheme")
            page.get_by_role("button", name="切换主题", exact=True).click()
            scheme_after = "dark" if scheme_before == "light" else "light"
            expect(page.locator("html")).to_have_attribute("data-mantine-color-scheme", scheme_after)
            page.reload()
            expect(page.locator("html")).to_have_attribute("data-mantine-color-scheme", scheme_after)
            expect(page.locator('[name="password"]')).to_have_count(0)
            assert page.evaluate("Object.keys(localStorage).every(key => key === 'mantine-color-scheme-value') && sessionStorage.length === 0")
            page.goto(origin + "/#/missing")
            expect(page.get_by_role("heading", name="页面不存在", exact=True)).to_be_visible()

            # Explicit view fixture, restricted to non-mutating responses.
            fixture = browser.new_page(viewport={"width": 1440, "height": 1000})
            fixture.on("pageerror", lambda error: errors.append(str(error)))
            fixture.add_init_script("(" + PRESENTATION_FIXTURE + ")()")
            fixture.add_init_script("window.__S152_CSP__ = []; document.addEventListener('securitypolicyviolation', e => window.__S152_CSP__.push({directive:e.effectiveDirective, blocked:e.blockedURI}));")
            fixture.goto(origin + "/#/files")
            expect(fixture.locator("[data-agent-feedback]")).to_have_text("本机 agent 状态已读取。")
            expect(fixture.locator(".mantine-AppShell-header").get_by_text("agent 已连接", exact=True)).to_be_visible()
            fixture.locator('[name="files_binding"]').select_option("1" * 32)
            clean = fixture.get_by_role("row").filter(has=fixture.get_by_role("cell", name="clean.txt", exact=True))
            dirty = fixture.get_by_role("row").filter(has=fixture.get_by_role("cell", name="pending.txt", exact=True))
            expect(clean).to_be_visible()
            expect(clean.get_by_text("已下载", exact=True)).to_be_visible()
            expect(dirty.get_by_text("待上传本地修改", exact=True)).to_be_visible()
            expect(dirty.get_by_role("button", name="释放空间", exact=True)).to_be_disabled()
            expect(dirty.get_by_role("button", name="下载", exact=True)).to_be_disabled()
            for scheme, width in (("light", 320), ("dark", 1440)):
                fixture.emulate_media(color_scheme=scheme)
                fixture.set_viewport_size({"width": width, "height": 900})
                nonce_and_layout(fixture, scheme, width, "/files")
                fixture.screenshot(path=str(OUTPUT / f"fixture-files-{scheme}-{width}.png"), full_page=True)
            fixture.set_viewport_size({"width": 1440, "height": 1000})
            trap_and_cancel(fixture, clean.get_by_role("button", name="释放空间", exact=True), "确认释放本地内容")
            fixture.set_viewport_size({"width": 320, "height": 900})
            long_row = fixture.get_by_role("row").filter(has=fixture.get_by_role("cell", name="longfilename" * 20 + ".txt", exact=True))
            trap_and_cancel(fixture, long_row.get_by_role("button", name="释放空间", exact=True), "确认释放本地内容")
            fixture.set_viewport_size({"width": 1440, "height": 1000})
            fixture.evaluate("window.__S152_FAIL_FILES__ = true")
            fixture.get_by_role("button", name="刷新文件状态", exact=True).click()
            expect(fixture.get_by_role("button", name="刷新文件状态", exact=True)).to_be_enabled()
            expect(fixture.get_by_text("S152 presentation: file read failed", exact=True).first).to_be_visible()
            expect(fixture.locator("[data-agent-feedback]")).not_to_have_text("操作已由 agent 确认。")
            expect(clean.get_by_role("button", name="释放空间", exact=True)).to_be_disabled()
            fixture.evaluate("window.__S152_FAIL_FILES__ = false")
            fixture.goto(origin + "/#/bindings")
            fixture.get_by_role("button", name="管理与恢复", exact=True).click()
            trap_and_cancel(fixture, fixture.get_by_role("button", name="解除绑定（保留数据）", exact=True), "确认解除绑定")
            fixture.goto(origin + "/#/history")
            fixture.locator('[name="history_binding"]').select_option("1" * 32)
            fixture.get_by_role("button", name="读取文件当前页", exact=True).click()
            fixture.locator('[name="history_entry"]').select_option("3" * 32)
            fixture.get_by_role("button", name="读取恢复清单", exact=True).click()
            trap_and_cancel(fixture, fixture.get_by_role("button", name="恢复此内容", exact=True), "确认恢复服务器内容")
            assert fixture.evaluate("window.__S152_CALLS__.every(call => !['agent_release_entry','agent_detach_binding','agent_restore_entry'].includes(call.command))")
            while fixture.locator(".mantine-Notification-root").count():
                fixture.locator(".mantine-Notification-root").first.get_by_role("button").click()
                fixture.wait_for_timeout(250)
            fixture.get_by_role("button", name="恢复此内容", exact=True).click()
            fixture.get_by_role("dialog", name="确认恢复服务器内容", exact=True).get_by_role("button", name="确认恢复", exact=True).click()
            expect(fixture.get_by_role("button", name="核对原恢复操作", exact=True)).to_be_enabled()
            expect(fixture.get_by_text("pending：尚未提交，不能显示已恢复", exact=True)).to_be_visible()
            expect(fixture.locator("[data-agent-feedback]")).not_to_have_text("操作已由 agent 确认。")
            assert fixture.locator(".mantine-Notification-root").get_by_text("操作已由 agent 确认。", exact=True).count() == 0
            assert not fixture.evaluate("window.__S152_CSP__"), fixture.evaluate("window.__S152_CSP__")
            assert not errors, errors
            browser.close()
        result = {"passed": True, "scope": "Chromium presentation; native IPC/CFAPI separately verified by Windows suites", "presentation_only": presentation_only, "cases": cases, "checks": (["50 disconnected route/theme/width layouts", "mobile navigation"] if cases else []) + ["48px header", "no document overflow", "strict nonce CSP", "no fabricated sync success", "initial connected badge and feedback agree", "downloaded/local_changes status labels", "theme persistence", "keyboard skip", "release/detach/restore modal focus trap/Escape/return", "dirty/pending action guards", "modal cancellation sends no mutation", "failed file read cannot report success", "pending restore cannot report success", "no browser credential storage", "no page errors"]}
        (OUTPUT / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2))
        print(f"S152 desktop UI passed: {OUTPUT}")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
