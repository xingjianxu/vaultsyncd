#!/usr/bin/env python3
"""S221 binding modal and ID-only directory bridge in Chromium.

Explicit IPC doubles verify React behavior and command shape only, not Explorer,
Windows path validation, named-pipe access or CFAPI. Native evidence uses S081.
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
OUTPUT = Path(os.environ["VAULTSYNCD_UI_ARTIFACTS"]) / "bindings"
SERVER_A, SERVER_B = "7" * 32, "8" * 32
LOCAL_PATH = "C:\\S221\\同步目录"
FIXTURE = r"""() => {
  const a = '7'.repeat(32), b = '8'.repeat(32);
  const connections = [
    { server_id: a, server_url: 'https://one.example.invalid', authenticated: true },
    { server_id: b, server_url: 'https://two.example.invalid', authenticated: true },
  ];
  const bindings = [];
  const f = window.fixture = {
    calls: [], items: [{ id: '2'.repeat(32), name: '共享资料' }, { id: '5'.repeat(32), name: '另一个 Vault' }],
    fail: false, pending: [], connections, creates: [], createPending: [], holdCreate: false, failCreate: false,
    opens: [], failOpen: false,
  };
  window.__TAURI_INTERNALS__ = { invoke: async (command, args) => {
    if (command === 'agent_get_status') {
      if (f.failStatus) throw { message: 'S221: 本机 IPC 已断开' };
      return { bindings: [], management: { connections: structuredClone(connections), bindings: structuredClone(bindings) } };
    }
    if (command === 'agent_list_vaults') {
      f.calls.push(args.serverId);
      await new Promise(resolve => f.pending.push({ serverId: args.serverId, resolve }));
      if (args.serverId === b) return { items: [{ id: '4'.repeat(32), name: '服务器二的 Vault' }] };
      if (f.fail) throw { message: 'UNAVAILABLE: 读取失败' };
      return f.malformed ? {} : { items: f.items };
    }
    if (command === 'select_local_directory') return 'C:\\S221\\同步目录';
    if (command === 'agent_create_binding') {
      f.creates.push(structuredClone(args.input));
      if (f.holdCreate) await new Promise(resolve => f.createPending.push(resolve));
      if (f.failCreate) throw { message: 'S221: 创建失败，保留输入' };
      const { bindingId, serverId, vaultId, localPath, mode } = f.created = args.input;
      bindings.push({ binding_id: bindingId, server_id: serverId, vault_id: vaultId, local_path: localPath, mode });
      return {};
    }
    if (command === 'open_binding_directory') {
      f.opens.push(structuredClone(args));
      if (f.failOpen) throw { message: 'S221: 无法打开绑定目录' };
      return;
    }
    if (command === 'agent_list_activity') { f.activityBinding = args.bindingId; return { activity: [] }; }
    throw { message: `unexpected command ${command}` };
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
            page.on("pageerror", lambda e: errors.append(str(e)))
            page.add_init_script(f"({FIXTURE})()")
            page.add_init_script("window.csp=[];document.addEventListener('securitypolicyviolation',e=>csp.push(e.effectiveDirective))")
            page.goto(f"http://localhost:{server.server_port}/#/bindings")
            trigger = page.get_by_role("button", name="添加同步目录", exact=True)
            modal = page.get_by_role("dialog", name="添加同步目录", exact=True)
            picker = modal.get_by_role("combobox", name="远端 Vault")
            submit = modal.get_by_role("button", name="创建绑定并开始合并", exact=True)
            server_select = modal.get_by_label("服务器", exact=True)
            path = modal.get_by_role("textbox", name="本地目录", exact=True)
            mode = modal.get_by_label("模式", exact=True)
            expect(trigger).to_be_enabled()
            expect(modal).to_have_count(0)
            expect(page.locator('[name="vault_id"], [name="local_path"], [name="server_id"]')).to_have_count(0)
            page.wait_for_timeout(350)
            assert page.evaluate("fixture.calls") == []
            checks.append("default table mounts no form and reads no Vault")

            def resolve():
                page.evaluate("fixture.pending.splice(0).forEach(request=>request.resolve())")
                expect(picker).to_have_attribute("aria-busy", "false")

            def open_dialog():
                count = page.evaluate("fixture.calls.length")
                trigger.click()
                expect(modal).to_be_visible()
                page.wait_for_function("count=>fixture.calls.length===count+1", arg=count)
                expect(server_select).to_have_value(SERVER_A)
                expect(path).to_have_value("")
                expect(mode).to_have_value("full")
                expect(submit).to_be_disabled()
                resolve()
                expect(picker).to_have_value("共享资料")

            def refresh(keyboard=False):
                count = page.evaluate("fixture.calls.length")
                if keyboard:
                    picker.focus()
                    picker.press("ArrowDown")
                else:
                    picker.click()
                page.wait_for_function("count=>fixture.calls.length===count+1", arg=count)
                expect(picker).to_have_attribute("aria-busy", "true")
                expect(submit).to_be_disabled()

            def choose_directory():
                modal.get_by_role("button", name="选择目录…", exact=True).click()
                expect(path).to_have_value(LOCAL_PATH)
                expect(submit).to_be_enabled()

            # All non-busy exits unmount the form, restore focus and discard drafts.
            for close in ("cancel", "x", "escape"):
                open_dialog()
                choose_directory()
                mode.select_option("on_demand")
                for _ in range(10):
                    page.keyboard.press("Tab")
                    assert modal.evaluate("element=>element.contains(document.activeElement)")
                if close == "cancel":
                    modal.get_by_role("button", name="取消", exact=True).click()
                elif close == "x":
                    modal.locator(".mantine-Modal-close").click()
                else:
                    modal.press("Escape")
                expect(modal).to_have_count(0)
                expect(trigger).to_be_focused()
                assert page.evaluate("fixture.creates") == []
            checks.append("cancel/X/Escape trap focus, send no create, unmount, and reset on reopen")

            open_dialog()
            expect(page.get_by_role("button", name="读取授权 Vault", exact=True)).to_have_count(0)
            refresh()
            expect(page.get_by_text("正在读取 Vault…", exact=True)).to_be_visible()
            modal.locator(".mantine-Modal-title").click()  # Dismiss the picker outside its overlapping option area.
            expect(modal).to_be_visible()
            count = page.evaluate("fixture.calls.length")
            picker.click()
            assert page.evaluate("fixture.calls.length") == count
            resolve()
            page.get_by_role("option", name="另一个 Vault", exact=True).click()
            choose_directory()
            refresh(keyboard=True)
            resolve()
            expect(picker).to_have_value("另一个 Vault")
            modal.locator(".mantine-Modal-title").click()
            page.evaluate("fixture.items=[{ id:'3'.repeat(32), name:'新增 Vault' }]")
            refresh()
            resolve()
            expect(picker).to_have_value("新增 Vault")
            expect(page.get_by_role("option", name="共享资料", exact=True)).to_have_count(0)
            page.get_by_role("option", name="新增 Vault", exact=True).click()
            page.evaluate("fixture.fail=true")
            refresh()
            resolve()
            expect(modal.get_by_text("UNAVAILABLE: 读取失败", exact=True)).to_be_visible()
            expect(page.locator('[data-combobox-option]')).to_have_count(0)
            expect(submit).to_be_disabled()
            modal.locator(".mantine-Modal-title").click()
            page.evaluate("fixture.fail=false;fixture.items=[]")
            refresh()
            resolve()
            expect(page.get_by_text("此服务器没有已授权的 Vault。", exact=True)).to_be_visible()
            expect(picker).to_have_value("")
            expect(submit).to_be_disabled()
            modal.locator(".mantine-Modal-title").click()
            page.evaluate("fixture.malformed=true")
            refresh()
            resolve()
            expect(modal.get_by_text("agent 未返回 Vault 清单。", exact=True)).to_be_visible()
            expect(submit).to_be_disabled()
            modal.locator(".mantine-Modal-title").click()
            page.evaluate("fixture.malformed=false;fixture.items=[{id:'3'.repeat(32),name:'新增 Vault'}]")
            refresh()
            resolve()
            page.get_by_role("option", name="新增 Vault", exact=True).click()
            checks.append("automatic first selection, mouse/keyboard refresh, pending deduplication, retained/revoked selection, read failure/empty/malformed/retry")

            mode.select_option("on_demand")
            page.evaluate("fixture.holdCreate=true;fixture.failCreate=true")
            submit.click()
            page.wait_for_function("fixture.createPending.length===1")
            expect(submit).to_be_disabled()
            expect(trigger).to_be_disabled()
            expect(modal.get_by_role("button", name="取消", exact=True)).to_be_disabled()
            close_button = modal.locator(".mantine-Modal-close")
            if close_button.count():
                expect(close_button).to_be_disabled()  # Hiding X while busy is equally safe.
            modal.press("Escape")
            page.locator(".mantine-Modal-overlay").click(position={"x": 2, "y": 2}, force=True)
            expect(modal).to_be_visible()
            assert page.evaluate("fixture.creates.length") == 1
            page.evaluate("fixture.createPending.shift()()")
            expect(page.locator("[data-agent-feedback]")).to_have_text("S221: 创建失败，保留输入")
            expect(page.locator(".mantine-Notification-root").get_by_text("S221: 创建失败，保留输入", exact=True)).to_be_visible()
            expect(modal).to_be_visible()
            expect(server_select).to_have_value(SERVER_A)
            expect(picker).to_have_value("新增 Vault")
            expect(path).to_have_value(LOCAL_PATH)
            expect(mode).to_have_value("on_demand")
            expect(submit).to_be_enabled()
            page.evaluate("fixture.holdCreate=false;fixture.failStatus=true")
            submit.click()
            expect(modal.get_by_role("alert")).to_have_text("本机 agent 未连接，已保留填写内容。")
            expect(submit).to_be_disabled()
            expect(server_select).to_be_disabled()
            expect(path).to_have_value(LOCAL_PATH)
            expect(picker).to_have_value("新增 Vault")
            expect(mode).to_have_value("on_demand")
            page.evaluate("fixture.failStatus=false")
            modal.get_by_role("button", name="检查本机连接", exact=True).click()
            expect(modal.get_by_role("alert")).to_have_count(0)
            expect(path).to_have_value(LOCAL_PATH)
            expect(mode).to_have_value("on_demand")
            expect(submit).to_be_enabled()
            checks.append("IPC loss preserves modal/draft, gates actions, and reconnect restores the same form")
            page.evaluate("fixture.failCreate=false")
            submit.click()
            expect(modal).to_have_count(0)
            expect(trigger).to_be_enabled()
            created = page.evaluate("fixture.created")
            assert {key: created[key] for key in ("serverId", "vaultId", "localPath", "mode")} == {
                "serverId": SERVER_A, "vaultId": "3" * 32, "localPath": LOCAL_PATH, "mode": "on_demand",
            }
            binding_a = created["bindingId"]
            count = page.evaluate("fixture.calls.length")
            page.wait_for_timeout(350)
            assert page.evaluate("fixture.calls.length") == count
            checks.append("busy prevents close/reopen/duplicate create; rejection preserves all input and notifies; success closes without another Vault read")

            page.evaluate("fixture.items=[{id:'2'.repeat(32),name:'共享资料'}]")
            open_dialog()
            # Closing during an unresolved read must not repopulate the next mount.
            refresh()
            modal.get_by_role("button", name="取消", exact=True).click()
            expect(modal).to_have_count(0)
            count = page.evaluate("fixture.calls.length")
            trigger.click()
            page.wait_for_function("count=>fixture.calls.length===count+1", arg=count)
            page.evaluate("fixture.pending.shift().resolve()")
            expect(picker).to_have_attribute("aria-busy", "true")
            resolve()
            expect(picker).to_have_value("共享资料")
            refresh()
            count = page.evaluate("fixture.calls.length")
            server_select.select_option(SERVER_B)
            expect(picker).to_have_value("")
            page.wait_for_function("count=>fixture.calls.length===count+1", arg=count)
            picker.click()
            assert page.evaluate("fixture.calls.slice(-2)") == [SERVER_A, SERVER_B]
            page.evaluate("fixture.pending.pop().resolve()")
            expect(page.get_by_role("option", name="服务器二的 Vault", exact=True)).to_be_visible()
            page.evaluate("fixture.pending.pop().resolve()")
            expect(page.get_by_role("option", name="共享资料", exact=True)).to_have_count(0)
            page.get_by_role("option", name="服务器二的 Vault", exact=True).click()
            choose_directory()
            submit.click()
            expect(modal).to_have_count(0)
            binding_b = page.evaluate("fixture.created.bindingId")
            assert page.evaluate("fixture.created.serverId") == SERVER_B
            assert page.evaluate("fixture.created.vaultId") == "4" * 32
            checks.append("reopen resets server/form, ignores prior mount response and isolates cross-server responses")

            table = page.get_by_role("table", name="本地 Binding")
            for url, vault, binding_id in (("https://one.example.invalid", "3" * 32, binding_a), ("https://two.example.invalid", "4" * 32, binding_b)):
                row = table.get_by_role("row").filter(has=page.get_by_role("cell", name=url, exact=True))
                expect(row.get_by_role("cell", name=vault, exact=True)).to_be_visible()
                directory = row.get_by_role("button", name=LOCAL_PATH, exact=True)
                expect(directory).to_have_js_property("tagName", "BUTTON")
                directory.click()
                expect(directory).to_be_enabled()
                directory.focus()
                directory.press("Enter")
                expect(directory).to_be_enabled()
                directory.press("Space")
                expect(directory).to_be_enabled()
                assert page.evaluate("fixture.opens.slice(-3)") == [{"bindingId": binding_id}] * 3
            # Earlier persistent rejection notices can occupy all three visible slots.
            # Dismiss them and any queued success notices before testing a new error.
            for _ in range(20):
                page.locator(".mantine-Notification-closeButton").evaluate_all("buttons=>buttons.forEach(button=>button.click())")
                page.wait_for_timeout(100)
            page.evaluate("fixture.failOpen=true")
            directory.click()
            expect(page.locator("[data-agent-feedback]")).to_have_text("S221: 无法打开绑定目录")
            expect(page.locator(".mantine-Notification-root").get_by_text("S221: 无法打开绑定目录", exact=True)).to_be_visible()
            expect(directory).to_be_enabled()
            page.evaluate("fixture.failOpen=false")
            directory.click()
            expect(page.locator("[data-agent-feedback]")).to_have_text("操作已由 agent 确认。")
            assert page.evaluate("fixture.opens.every(args=>Object.keys(args).length===1 && typeof args.bindingId==='string')")
            checks.append("two servers display correct Vault IDs; directory is a keyboard button; mouse/Enter/Space send only bindingId; open errors notify and retry")

            open_dialog()
            count = page.evaluate("fixture.calls.length")
            page.evaluate("fixture.connections[1].authenticated=false")
            # Wait for status polling before selecting the now-unauthenticated server.
            expect(server_select.locator(f'option[value="{SERVER_B}"]')).to_contain_text("未登录")
            server_select.select_option(SERVER_B)
            expect(picker).to_have_count(0)
            expect(modal.get_by_text("登录此服务器后可选择 Vault。", exact=True)).to_be_visible()
            assert page.evaluate("fixture.calls.length") == count
            modal.get_by_role("button", name="取消", exact=True).click()
            page.evaluate("location.hash='/activity'")
            selector = page.locator('[name="activity_binding"]')
            selector.select_option(binding_b)
            expect(page.get_by_text("请先登录此绑定的服务器。", exact=True)).to_be_visible()
            selector.select_option(binding_a)
            expect(page.get_by_role("button", name="刷新记录", exact=True)).to_be_enabled()
            page.wait_for_function("id=>fixture.activityBinding===id", arg=binding_a)
            page.evaluate("location.hash='/bindings'")
            expect(trigger).to_be_visible()
            expect(modal).to_have_count(0)
            checks.append("per-server auth gates Vault and activity; route return stays closed")
            open_dialog()
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({"width": width, "height": 900})
                    page.wait_for_timeout(250)
                    expect(modal).to_be_visible()
                    page.screenshot(path=str(OUTPUT / f"bindings-{scheme}-{width}.png"), full_page=True)
                    assert page.evaluate("document.documentElement.scrollWidth<=innerWidth")
            modal.press("Escape")
            assert not errors, errors
            assert not page.evaluate("window.csp")
            checks.append("light/dark 320/1440 modal layout, no page errors or CSP violations")
            (OUTPUT / "result.json").write_text(json.dumps({"ok": True, "scope": "Chromium presentation and ID-only command shape; not Windows Explorer evidence", "checks": checks}, ensure_ascii=False, indent=2))
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
    print(f"S221 bindings browser passed ({len(checks)} groups): {OUTPUT}")


if __name__ == "__main__":
    main()
