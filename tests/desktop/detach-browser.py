#!/usr/bin/env python3
"""S192 desktop list projection; native IPC/data safety is tested by S082."""
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
OUTPUT = Path(os.environ["VAULTSYNCD_UI_ARTIFACTS"]) / "detach"
BINDING = "1" * 32
OTHER = "9" * 32
FIXTURE = r"""() => {
  const original = window.__TAURI_INTERNALS__.invoke;
  const f = window.detachFixture = { detached: sessionStorage.getItem('S192-fixture-detached') === 'true', fail: false, calls: 0, reads: [], pending: [], hold: false };
  window.__TAURI_INTERNALS__.invoke = async (command, args) => {
    if (command === 'agent_get_status') {
      const result = structuredClone(await original(command, args));
      const bindings = result.management.bindings;
      bindings.push({ ...bindings[0], binding_id: '9'.repeat(32), local_path: 'C:\\S192\\保留目录' });
      bindings.push({ ...bindings[0], binding_id: 'a'.repeat(32), local_path: 'C:\\S192\\历史已解除', detached: true });
      bindings[0].detached = f.detached;
      return result;
    }
    if (command === 'agent_detach_binding') {
      f.calls++;
      if (f.fail) throw { message: 'S192: 解除绑定失败，数据保留' };
      f.detached = true;
      sessionStorage.setItem('S192-fixture-detached', 'true');
      return { unconfirmed: [{ path: 'pending.txt' }], incomplete: [{ path: 'cloud.txt' }] };
    }
    if (command === 'agent_list_activity') return { activity: [] };
    if (command === 'agent_list_files') {
      f.reads.push(args.bindingId);
      if (f.hold) await new Promise(resolve => f.pending.push(resolve));
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
            page.goto(origin + "/#/bindings")
            table = page.get_by_role("table", name="本地 Binding")
            row = table.get_by_role("row").filter(has=page.get_by_role("cell", name="C:\\S152\\同步目录", exact=True))
            other = table.get_by_role("row").filter(has=page.get_by_role("cell", name="C:\\S192\\保留目录", exact=True))
            expect(row).to_be_visible()
            expect(other).to_be_visible()
            expect(table.get_by_text("C:\\S192\\历史已解除", exact=True)).to_have_count(0)
            checks.append("previously detached records hidden on startup")

            row.get_by_role("button", name="管理与恢复", exact=True).click()
            page.get_by_role("button", name="解除绑定（保留数据）", exact=True).click()
            page.get_by_role("dialog", name="确认解除绑定", exact=True).press("Escape")
            expect(row).to_be_visible()
            assert page.evaluate("detachFixture.calls") == 0
            checks.append("cancel leaves binding unchanged")

            page.evaluate("detachFixture.fail=true")
            page.get_by_role("button", name="解除绑定（保留数据）", exact=True).click()
            page.get_by_role("button", name="确认保留数据并解除", exact=True).click()
            expect(page.locator("[data-agent-feedback]")).to_have_text("S192: 解除绑定失败，数据保留")
            expect(row).to_be_visible()
            checks.append("rejected detach does not remove binding")
            page.evaluate("detachFixture.fail=false")
            page.get_by_role("button", name="解除绑定（保留数据）", exact=True).click()
            page.get_by_role("button", name="确认保留数据并解除", exact=True).click()
            expect(row).to_have_count(0)
            expect(other).to_be_visible()
            expect(page.get_by_role("heading", name="绑定管理：C:\\S152\\同步目录", exact=True)).to_have_count(0)
            warning = page.get_by_text("已停止管理，保留未确认 1 项；非完整内容 1 项仍不能保证离线读取。", exact=True)
            expect(warning).to_be_visible()
            checks.append("confirmed detach removes row and management card but preserves report and other binding")
            while page.locator(".mantine-Notification-root").count():
                notification = page.locator(".mantine-Notification-root").first
                notification.get_by_role("button").click()
                page.wait_for_timeout(250)

            routes = (("/files", "files_binding"), ("/history", "history_binding"), ("/activity", "activity_binding"))
            for route, name in routes:
                page.evaluate("path=>location.hash=path", route)
                selector = page.locator(f'[name="{name}"]')
                expect(selector.locator(f'option[value="{BINDING}"]')).to_have_count(0)
                expect(selector.locator(f'option[value="{OTHER}"]')).to_have_count(1)
            page.evaluate("location.hash='/bindings'")
            page.get_by_role("button", name="检查本机连接", exact=True).click()
            expect(row).to_have_count(0)
            page.reload()
            expect(other).to_be_visible()
            expect(row).to_have_count(0)
            checks.append("all related selectors, navigation, status refresh and reload exclude detached bindings")

            # Simulate a detach from another local IPC client while a page has an active selection.
            for route, name in routes:
                page.evaluate("detachFixture.detached=false;location.hash='/bindings'")
                page.get_by_role("button", name="检查本机连接", exact=True).click()
                expect(row).to_be_visible()
                page.evaluate("path=>location.hash=path", route)
                selector = page.locator(f'[name="{name}"]')
                selector.select_option(BINDING)
                if route == "/files":
                    expect(page.get_by_role("cell", name="clean.txt", exact=True)).to_be_visible()
                    page.evaluate("detachFixture.hold=true")
                    page.wait_for_function("() => detachFixture.pending.length>0")
                    # Polling, not an action, observes the external detach while the file read waits.
                page.evaluate("detachFixture.detached=true")
                expect(selector.locator(f'option[value="{BINDING}"]')).to_have_count(0)
                expect(selector).to_have_value("")
                if route == "/files":
                    page.evaluate("detachFixture.hold=false;detachFixture.pending.splice(0).forEach(resolve=>resolve())")
                    expect(page.get_by_role("cell", name="clean.txt", exact=True)).to_have_count(0)
                    expect(page.get_by_role("button", name="刷新文件状态", exact=True)).to_be_disabled()
                elif route == "/history":
                    expect(page.get_by_role("button", name="读取文件当前页", exact=True)).to_be_disabled()
                else:
                    expect(page.get_by_role("button", name="刷新记录", exact=True)).to_be_disabled()
            checks.append("external detach clears every active selection; late file reads cannot repopulate removed scope")
            assert not errors, errors
            assert not page.evaluate("window.csp")
            checks.append("no page errors or CSP violations")
            page.screenshot(path=str(OUTPUT / "detached.png"), full_page=True)
            (OUTPUT / "result.json").write_text(json.dumps({"ok": True, "checks": checks}, ensure_ascii=False, indent=2))
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
    print(f"S192 detach browser passed ({len(checks)} groups): {OUTPUT}")


if __name__ == "__main__":
    main()
