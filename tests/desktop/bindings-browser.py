#!/usr/bin/env python3
"""Authorized Vault selection and cross-server response isolation in Chromium."""
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
OUTPUT = Path(os.environ["VAULTSYNCD_UI_ARTIFACTS"]) / "bindings"
SERVER_A, SERVER_B = "7" * 32, "8" * 32
FIXTURE = r"""() => {
  const a = '7'.repeat(32), b = '8'.repeat(32);
  const connections = [
    { server_id: a, server_url: 'https://one.example.invalid', authenticated: true },
    { server_id: b, server_url: 'https://two.example.invalid', authenticated: true },
  ];
  const bindings = [];
  window.fixture = { calls: [], items: [{ id: '2'.repeat(32), name: '共享资料' }], fail: false, pending: [], connections };
  const f = window.fixture;
  window.__TAURI_INTERNALS__ = { invoke: async (command, args) => {
    if (command === 'agent_get_status') return { bindings: [], management: { connections: structuredClone(connections), bindings: structuredClone(bindings) } };
    if (command === 'agent_list_vaults') {
      f.calls.push(args.serverId);
      await new Promise(resolve => f.pending.push({ serverId: args.serverId, resolve }));
      if (args.serverId === b) return { items: [{ id: '4'.repeat(32), name: '服务器二的 Vault' }] };
      if (f.fail) throw { message: 'UNAVAILABLE: 读取失败' };
      return f.malformed ? {} : { items: f.items };
    }
    if (command === 'select_local_directory') return 'C:\\S178\\同步目录';
    if (command === 'agent_create_binding') {
      const { bindingId, serverId, vaultId, localPath, mode } = f.created = args.input;
      bindings.push({ binding_id: bindingId, server_id: serverId, vault_id: vaultId, local_path: localPath, mode });
      return {};
    }
    if (command === 'agent_list_activity') { f.activityBinding = args.bindingId; return { activity: [] }; }
    throw { message: `unexpected command ${command}` };
  }};
}"""


def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(ui.DesktopStaticFiles, directory=str(ui.REPO / "apps/desktop/dist")))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            page = browser.new_page()
            errors = []
            page.on("pageerror", lambda e: errors.append(str(e)))
            page.add_init_script(f"({FIXTURE})()")
            page.add_init_script("window.csp=[];document.addEventListener('securitypolicyviolation',e=>csp.push(e.effectiveDirective))")
            page.goto(f"http://localhost:{server.server_port}/#/bindings")
            picker = page.get_by_role("combobox", name="远端 Vault")
            submit = page.get_by_role("button", name="创建绑定并开始合并")
            server_select = page.get_by_label("服务器", exact=True)
            expect(server_select).to_be_visible()
            expect(picker).to_have_count(0)
            server_select.select_option(SERVER_A)
            expect(picker).to_be_visible()
            expect(page.get_by_role("button", name="读取授权 Vault", exact=True)).to_have_count(0)
            assert page.evaluate("fixture.calls.length") == 0

            def open_picker(count, keyboard=False):
                if keyboard:
                    picker.focus()
                    picker.press("ArrowDown")
                else:
                    picker.click()
                page.wait_for_function("count=>fixture.calls.length===count", arg=count)
                expect(picker).to_have_attribute("aria-busy", "true")
                expect(submit).to_be_disabled()

            def resolve():
                page.evaluate("fixture.pending.splice(0).forEach(request=>request.resolve())")
                expect(picker).to_have_attribute("aria-busy", "false")

            open_picker(1)
            expect(page.get_by_text("正在读取 Vault…", exact=True)).to_be_visible()
            picker.press("Escape")
            picker.click()
            assert page.evaluate("fixture.calls.length") == 1  # reopen while pending shares the request
            resolve()
            page.get_by_role("option", name="共享资料", exact=True).click()
            page.get_by_role("button", name="选择目录…", exact=True).click()
            expect(submit).to_be_enabled()

            open_picker(2, keyboard=True)
            resolve()
            expect(picker).to_have_value("共享资料")
            picker.press("Escape")

            page.evaluate("fixture.items=[{ id:'3'.repeat(32), name:'新增 Vault' }]")
            open_picker(3)
            resolve()
            expect(picker).to_have_value("")
            expect(submit).to_be_disabled()
            expect(page.get_by_role("option", name="共享资料", exact=True)).to_have_count(0)
            page.get_by_role("option", name="新增 Vault", exact=True).click()

            page.evaluate("fixture.fail=true")
            open_picker(4)
            resolve()
            expect(page.get_by_text("UNAVAILABLE: 读取失败", exact=True)).to_be_visible()
            expect(page.locator('[role="option"]')).to_have_count(0)
            expect(submit).to_be_disabled()
            picker.press("Escape")
            page.evaluate("fixture.fail=false")
            open_picker(5)
            resolve()
            page.get_by_role("option", name="新增 Vault", exact=True).click()
            expect(submit).to_be_enabled()
            submit.click()
            page.wait_for_function("fixture.created !== undefined")
            assert page.evaluate("fixture.created.vaultId") == "3" * 32
            assert page.evaluate("fixture.created.serverId") == SERVER_A

            page.evaluate("fixture.items=[]")
            open_picker(6)
            resolve()
            expect(page.get_by_text("此服务器没有已授权的 Vault。", exact=True)).to_be_visible()
            expect(picker).to_have_value("")
            expect(submit).to_be_disabled()
            picker.press("Escape")
            page.evaluate("fixture.malformed=true")
            open_picker(7)
            resolve()
            expect(page.get_by_text("agent 未返回 Vault 清单。", exact=True)).to_be_visible()
            expect(submit).to_be_disabled()
            picker.press("Escape")

            page.evaluate("fixture.malformed=false;fixture.items=[{id:'4'.repeat(32),name:'回到页面的新清单'}]")
            open_picker(8)
            page.evaluate("location.hash='/connections'")
            expect(picker).to_have_count(0)
            resolve_pending = "fixture.pending.splice(0).forEach(request=>request.resolve())"
            page.evaluate(resolve_pending)
            page.evaluate("location.hash='/bindings'")
            server_select.select_option(SERVER_A)
            open_picker(9)
            resolve()
            expect(page.get_by_role("option", name="回到页面的新清单", exact=True)).to_be_visible()
            picker.press("Escape")
            open_picker(10)
            server_select.select_option(SERVER_B)
            expect(picker).to_have_value("")
            open_picker(11)
            assert page.evaluate("fixture.calls.slice(-2)") == [SERVER_A, SERVER_B]
            page.evaluate("fixture.pending.pop().resolve()")
            option = page.get_by_role("option", name="服务器二的 Vault", exact=True)
            expect(option).to_be_visible()
            page.evaluate("fixture.pending.pop().resolve()")
            expect(page.get_by_role("option", name="回到页面的新清单", exact=True)).to_have_count(0)
            option.click()
            page.get_by_role("button", name="选择目录…", exact=True).click()
            submit.click()
            page.wait_for_function("id=>fixture.created.serverId===id", arg=SERVER_B)
            assert page.evaluate("fixture.created.vaultId") == "4" * 32
            table = page.get_by_role("table", name="本地 Binding")
            for url, vault in (("https://one.example.invalid", "3" * 32), ("https://two.example.invalid", "4" * 32)):
                row = table.get_by_role("row").filter(has=page.get_by_role("cell", name=url, exact=True))
                expect(row.get_by_role("cell", name=vault, exact=True)).to_be_visible()
            page.evaluate("fixture.connections[1].authenticated=false")
            expect(picker).to_have_count(0)
            expect(page.get_by_text("登录此服务器后可选择 Vault。", exact=True)).to_be_visible()
            binding_b = page.evaluate("fixture.created.bindingId")
            page.evaluate("location.hash='/activity'")
            selector = page.locator('[name="activity_binding"]')
            selector.select_option(binding_b)
            expect(page.get_by_text("请先登录此绑定的服务器。", exact=True)).to_be_visible()
            assert page.evaluate("fixture.activityBinding") is None
            selector.select_option(index=1)
            expect(page.get_by_role("button", name="刷新记录", exact=True)).to_be_enabled()
            page.wait_for_function("fixture.activityBinding !== undefined")
            assert page.evaluate("fixture.activityBinding") != binding_b
            page.evaluate("location.hash='/bindings'")
            server_select.select_option(SERVER_A)
            expect(picker).to_be_visible()
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({"width": width, "height": 900})
                    page.wait_for_timeout(250)
                    page.screenshot(path=str(OUTPUT / f"bindings-{scheme}-{width}.png"), full_page=True)
                    assert page.evaluate("document.documentElement.scrollWidth<=innerWidth"), page.evaluate("[...document.querySelectorAll('*')].filter(e=>e.getBoundingClientRect().right>innerWidth).map(e=>({tag:e.tagName,cls:e.className,right:e.getBoundingClientRect().right})).slice(-10)")
            assert not errors, errors
            assert not page.evaluate("window.csp")
            (OUTPUT / "result.json").write_text(json.dumps({"ok": True, "checks": ["mouse open", "keyboard open", "deduplicated pending reopen", "selection retained", "revoked selection cleared", "failure blocks stale options", "retry", "binding submits selected identity", "empty", "malformed", "unmount", "fresh mount", "stale response after server switch", "two servers create and display", "per-server auth gate", "activity auth follows binding", "light/dark 320/1440", "CSP"]}))
            browser.close()
    finally:
        server.shutdown()
    print(f"Multi-server bindings browser passed: {OUTPUT}")


if __name__ == "__main__":
    main()
