#!/usr/bin/env python3
"""S193 desktop server removal; native IPC/Credential Manager evidence comes from Windows suites.

The fixture below is an explicit presentation double: it records the command shape the GUI
sends and refuses to fake success for anything except the requested removal response.
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
OUTPUT = Path(os.environ["VAULTSYNCD_UI_ARTIFACTS"]) / "remove-server"
FIRST = "7" * 32
SECOND = "9" * 32
THIRD = "b" * 32
FIXTURE = r"""() => {
  const original = window.__TAURI_INTERNALS__.invoke;
  const mark = id => sessionStorage.getItem('S193-removed-' + id) === 'true';
  const f = window.removeFixture = {
    removed: { '77777777777777777777777777777777': mark('77777777777777777777777777777777'), '99999999999999999999999999999999': mark('99999999999999999999999999999999'), 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb': mark('bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb') },
    calls: [], fail: false, revoked: true
  };
  window.__TAURI_INTERNALS__.invoke = async (command, args) => {
    if (command === 'agent_get_status') {
      const result = structuredClone(await original(command, args));
      const base = result.management.connections[0];
      result.management.connections = [
        base,
        { ...base, server_id: '9'.repeat(32), server_url: 'https://second.invalid', username: undefined },
        { ...base, server_id: 'b'.repeat(32), server_url: 'https://third.invalid', username: undefined },
      ].filter(connection => !f.removed[connection.server_id]);
      return result;
    }
    if (command === 'agent_remove_connection') {
      f.calls.push(args.serverId);
      if (f.fail) throw { message: '该服务器仍有正在管理的本地绑定（CONNECTION_IN_USE）；请先在「同步目录」解除绑定并确认保留的数据，再移除服务器。' };
      f.removed[args.serverId] = true;
      sessionStorage.setItem('S193-removed-' + args.serverId, 'true');
      return { session_revoked: f.revoked };
    }
    return original(command, args);
  };
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
            page.add_init_script(f"({ui.PRESENTATION_FIXTURE})();({FIXTURE})()")
            page.add_init_script("window.csp=[];document.addEventListener('securitypolicyviolation',e=>csp.push(e.effectiveDirective))")
            origin = f"http://localhost:{server.server_port}"
            page.goto(origin + "/#/connections")

            def card(url):
                return page.locator("section").filter(has=page.get_by_role("heading", level=2, name=url, exact=True))

            first, second, third = card("https://example.invalid"), card("https://second.invalid"), card("https://third.invalid")
            expect(first).to_be_visible()
            expect(second).to_be_visible()
            expect(third).to_be_visible()
            expect(first.get_by_role("button", name="移除服务器", exact=True)).to_be_enabled()
            checks.append("every saved server offers removal")

            trigger = second.get_by_role("button", name="移除服务器", exact=True)
            ui.trap_and_cancel(page, trigger, "确认移除服务器")
            assert page.evaluate("removeFixture.calls.length") == 0
            expect(second).to_be_visible()
            trigger.click()
            page.get_by_role("dialog", name="确认移除服务器", exact=True).get_by_role("button", name="取消", exact=True).click()
            expect(page.get_by_role("dialog", name="确认移除服务器", exact=True)).to_be_hidden()
            assert page.evaluate("removeFixture.calls.length") == 0
            expect(second).to_be_visible()
            checks.append("Escape and cancel send no removal and keep the card")

            page.evaluate("removeFixture.fail=true")
            trigger.click()
            page.get_by_role("dialog", name="确认移除服务器", exact=True).get_by_role("button", name="移除服务器", exact=True).click()
            expect(page.locator("[data-agent-feedback]")).to_have_text("该服务器仍有正在管理的本地绑定（CONNECTION_IN_USE）；请先在「同步目录」解除绑定并确认保留的数据，再移除服务器。")
            expect(second).to_be_visible()
            expect(second.get_by_role("button", name="移除服务器", exact=True)).to_be_enabled()
            assert page.evaluate("removeFixture.calls") == [SECOND]
            checks.append("a refused removal keeps the saved server and reports the binding precondition")
            page.evaluate("removeFixture.fail=false")

            # Confirmed revocation: card disappears, other servers stay, success is agent-confirmed.
            third.get_by_role("button", name="移除服务器", exact=True).click()
            page.get_by_role("dialog", name="确认移除服务器", exact=True).get_by_role("button", name="移除服务器", exact=True).click()
            expect(third).to_have_count(0)
            expect(first).to_be_visible()
            expect(second).to_be_visible()
            expect(page.locator("[data-agent-feedback]")).to_have_text("操作已由 agent 确认。")
            assert page.evaluate("removeFixture.calls").count(THIRD) == 1
            checks.append("confirmed removal removes only that server after agent acknowledgment")

            # A server that did not confirm revocation keeps an explicit, non-expiring notice.
            page.evaluate("removeFixture.revoked=false")
            second.get_by_role("button", name="移除服务器", exact=True).click()
            page.get_by_role("dialog", name="确认移除服务器", exact=True).get_by_role("button", name="移除服务器", exact=True).click()
            expect(second).to_have_count(0)
            notice = page.get_by_text("已移除本地连接、登录资料与凭据；服务器未确认会话已撤销。", exact=True)
            expect(notice).to_be_visible()
            # The unconfirmed revocation must not disappear like a transient success toast.
            page.wait_for_timeout(4500)
            expect(notice).to_be_visible()
            checks.append("unconfirmed remote revocation stays reported instead of pretending success")

            page.reload()
            expect(first).to_be_visible()
            expect(second).to_have_count(0)
            expect(third).to_have_count(0)
            expect(page.get_by_role("link", name="添加服务器", exact=True)).to_be_visible()
            checks.append("removed servers stay gone after a status reload")
            removals = page.evaluate("removeFixture.calls")
            page.evaluate("location.hash='/bindings'")
            trigger = page.get_by_role("button", name="添加同步目录", exact=True)
            expect(trigger).to_be_enabled()
            expect(page.locator('[name="server_id"], [name="vault_id"]')).to_have_count(0)
            assert page.evaluate("!window.__S152_CALLS__.some(call=>call.command==='agent_list_vaults')")
            trigger.click()
            modal = page.get_by_role("dialog", name="添加同步目录", exact=True)
            selector = modal.get_by_label("服务器", exact=True)
            expect(selector).to_have_value(FIRST)
            expect(selector.locator("option")).to_have_count(1)
            expect(selector.locator(f'option[value="{SECOND}"], option[value="{THIRD}"]')).to_have_count(0)
            modal.get_by_role("button", name="取消", exact=True).click()
            expect(modal).to_have_count(0)
            expect(trigger).to_be_focused()
            assert page.evaluate("removeFixture.calls") == removals
            checks.append("lazy add-binding dialog excludes removed servers and cancel sends no extra removal")
            assert not errors, errors
            assert not page.evaluate("window.csp")
            checks.append("no page errors or CSP violations")
            page.screenshot(path=str(OUTPUT / "removed.png"), full_page=True)
            (OUTPUT / "result.json").write_text(json.dumps({"ok": True, "checks": checks}, ensure_ascii=False, indent=2))
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
    print(f"S193 remove-server browser passed ({len(checks)} groups): {OUTPUT}")


if __name__ == "__main__":
    main()
