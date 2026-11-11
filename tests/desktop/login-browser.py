#!/usr/bin/env python3
"""Desktop startup, server discovery and independent logins with a browser IPC fixture."""
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
OUTPUT = Path(os.environ["VAULTSYNCD_UI_ARTIFACTS"]) / "login"
SERVER_A, SERVER_B = "7" * 32, "8" * 32
FIXTURE = r"""() => {
  const fixture = window.fixture = { ready: false, calls: [], connections: [], fail: true };
  const get = id => fixture.connections.find(connection => connection.server_id === id);
  window.__TAURI_INTERNALS__ = { invoke: async (command, args) => {
    fixture.calls.push({ command, serverId: args?.serverId ?? args?.input?.serverId });
    if (command === 'agent_get_status') {
      while (!fixture.ready) await new Promise(resolve => setTimeout(resolve, 25));
      return { bindings: [], management: { connections: structuredClone(fixture.connections), bindings: [], problems: [], notices: [] } };
    }
    if (command === 'agent_discover_connection') {
      const url = args.input.serverUrl;
      return { connection: { server_id: (url.includes('one') ? '7' : '8').repeat(32), server_url: url, ca_file: args.input.caFile, authenticated: false } };
    }
    if (command === 'agent_set_connection') {
      const input = args.input;
      let connection = get(input.serverId);
      if (!connection) {
        connection = { server_id: input.serverId, authenticated: false };
        fixture.connections.push(connection);
      }
      connection.server_url = input.serverUrl;
      connection.ca_file = input.caFile;
      return { connection };
    }
    if (command === 'agent_login') {
      if (fixture.fail) throw { message: '用户名或密码错误' };
      const connection = get(args.serverId);
      connection.authenticated = true;
      connection.username = args.username;
      return { connection };
    }
    if (command === 'agent_logout') {
      const connection = get(args.serverId);
      connection.authenticated = false;
      return { connection };
    }
    throw { message: `unexpected command ${command}` };
  }};
}"""


def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(ui.DesktopStaticFiles, directory=str(ui.REPO / "apps/desktop/dist")))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = f"http://localhost:{server.server_port}"
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            page = browser.new_page(viewport={"width": 1440, "height": 900})
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.add_init_script("window.csp=[]; document.addEventListener('securitypolicyviolation', e=>window.csp.push(e.effectiveDirective));")
            page.goto(origin + "/#/files")
            expect(page.get_by_role("heading", name="文件状态与操作", exact=True)).to_be_visible()
            expect(page.get_by_role("dialog")).to_have_count(0)
            expect(page.locator("[data-agent-feedback]")).to_contain_text("未连接本机 agent")
            version = json.loads((ui.REPO / "apps/desktop/package.json").read_text())["version"]
            expect(page.locator("[data-client-version]")).to_have_text(f"vaultsyncd {version}")
            page.add_init_script(f"({FIXTURE})()")
            page.goto(origin + "/")
            page.reload()
            expect(page.get_by_role("heading", name="同步目录", exact=True)).to_be_visible()
            expect(page.locator('[name="password"]')).to_have_count(0)
            expect(page.get_by_role("dialog")).to_have_count(0)
            assert page.evaluate('fixture.calls.every(call=>call.command==="agent_get_status")')
            page.evaluate("fixture.ready=true")
            expect(page.get_by_text("尚无同步目录。", exact=True)).to_be_visible()

            def add_server(url, server_id):
                page.get_by_role("link", name="添加服务器", exact=True).click()
                expect(page.locator('[name="password"]')).to_have_count(0)
                page.get_by_role("textbox", name="服务器地址", exact=True).fill(url)
                expect(page.locator('textarea[name="server_id"]')).to_have_value(server_id)
                page.get_by_role("button", name="确认并保存连接", exact=True).click()
                expect(page.locator('[name="username"]')).to_be_visible()
                expect(page.get_by_role("dialog")).to_have_count(0)
                return page.locator("section").filter(has=page.get_by_role("heading", name=url, exact=True))

            def login(card, username):
                card.locator('[name="username"]').fill(username)
                card.locator('[name="password"]').fill("test-password")
                card.get_by_role("button", name="登录", exact=True).click()

            one = add_server("https://one.example.invalid", SERVER_A)
            login(one, "用户甲")
            expect(page.locator("[data-agent-feedback]")).to_have_text("用户名或密码错误")
            expect(one.locator('[name="password"]')).to_have_value("")
            expect(one.locator('[name="username"]')).to_have_value("用户甲")
            page.evaluate("fixture.fail=false")
            login(one, "用户甲")
            expect(one.get_by_text("已登录", exact=True)).to_be_visible()
            expect(one.locator('[name="password"]')).to_have_count(0)

            two = add_server("https://two.example.invalid", SERVER_B)
            login(two, "用户乙")
            expect(two.get_by_text("已登录", exact=True)).to_be_visible()
            expect(one.get_by_text("已登录", exact=True)).to_be_visible()

            one.get_by_role("button", name="修改地址", exact=True).click()
            one.get_by_role("textbox", name="服务器地址", exact=True).fill("https://two.example.invalid")
            expect(one.get_by_role("alert")).to_contain_text("不同的 Server ID")
            expect(one.get_by_role("button", name="确认并保存连接", exact=True)).to_be_disabled()
            one.get_by_role("textbox", name="服务器地址", exact=True).fill("https://one-renamed.example.invalid")
            one.get_by_role("button", name="确认并保存连接", exact=True).click()
            renamed = page.locator("section").filter(has=page.get_by_role("heading", name="https://one-renamed.example.invalid", exact=True))
            expect(renamed.get_by_text("已登录", exact=True)).to_be_visible()
            renamed.get_by_role("button", name="退出登录", exact=True).click()
            expect(renamed.get_by_text("未登录", exact=True)).to_be_visible()
            expect(two.get_by_text("已登录", exact=True)).to_be_visible()
            expect(page.locator('[name="password"]')).to_have_count(0)

            renamed.get_by_role("button", name="登录", exact=True).click()
            expect(renamed.locator('[name="username"]')).to_have_value("用户甲")
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({"width": width, "height": 900})
                    assert page.evaluate("document.documentElement.scrollWidth<=innerWidth")
                    page.screenshot(path=str(OUTPUT / f"login-{scheme}-{width}.png"), full_page=True)
            renamed.get_by_role("button", name="取消", exact=True).click()
            expect(page.locator('[name="password"]')).to_have_count(0)
            assert page.evaluate("fixture.calls.filter(c=>c.command==='agent_login').map(c=>c.serverId)") == [SERVER_A, SERVER_A, SERVER_B]
            assert page.evaluate("fixture.calls.filter(c=>c.command==='agent_logout').map(c=>c.serverId)") == [SERVER_A]
            assert page.evaluate("fixture.calls.filter(c=>c.command==='agent_set_connection').map(c=>c.serverId)") == [SERVER_A, SERVER_B, SERVER_A]
            assert not errors, errors
            assert not page.evaluate("window.csp"), page.evaluate("window.csp")
            assert page.evaluate('Object.keys(localStorage).every(k=>k==="mantine-color-scheme-value") && sessionStorage.length===0')
            checks = [
                "disconnected deep link", "shell while reading status", "fresh empty bindings", "explicit server discovery",
                "wrong password cleared", "independent logins", "identity pin on edit", "same identity endpoint edit",
                "scoped logout", "explicit relogin and cancel", "320/1440 light/dark", "CSP", "no credential storage",
            ]
            (OUTPUT / "result.json").write_text(json.dumps({"ok": True, "checks": checks}))
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
    print(f"Multi-server login browser passed: {OUTPUT}")


if __name__ == "__main__":
    main()
