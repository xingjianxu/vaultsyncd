#!/usr/bin/env python3
"""S200: built settings controls with explicit native preference fixture; no agent dependency."""
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
OUTPUT = Path(os.environ["VAULTSYNCD_UI_ARTIFACTS"]) / "settings"
FIXTURE = r"""() => {
  window.settingsFixture = {
    value: JSON.parse(sessionStorage.getItem('s200-settings') || '{"close_action":"tray","stop_agent_on_exit":true}'),
    writes: [], failRead: false, malformed: false, failWrite: false, holdWrite: false, pending: [],
  };
  window.__TAURI_INTERNALS__ = { invoke: async (command, args) => {
    const f = window.settingsFixture;
    if (command === 'get_desktop_settings') {
      if (f.failRead) throw 'S200: 程序配置损坏';
      return f.malformed ? {} : structuredClone(f.value);
    }
    if (command === 'set_desktop_settings') {
      f.writes.push(structuredClone(args));
      if (f.holdWrite) await new Promise(resolve => f.pending.push(resolve));
      if (f.failWrite) throw {message:'S200: 程序配置保存失败'};
      f.value = structuredClone(args.settings);
      sessionStorage.setItem('s200-settings', JSON.stringify(f.value));
      return structuredClone(f.value);
    }
    throw {message:'S200: agent offline'};
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
            page.goto(f"http://localhost:{server.server_port}/#/settings")
            tray = page.get_by_role("radio", name="最小化到托盘", exact=True)
            exit_app = page.get_by_role("radio", name="完全退出程序", exact=True)
            stop = page.get_by_role("switch", name="退出程序时自动关闭 agent", exact=True)
            save = page.get_by_role("button", name="保存程序配置", exact=True)
            feedback = page.locator("[data-settings-feedback]")
            expect(page.get_by_role("heading", name="程序配置", exact=True)).to_be_visible()
            expect(tray).to_be_checked()
            expect(stop).to_be_checked()
            expect(stop).to_be_disabled()
            expect(save).to_be_disabled()
            expect(page.get_by_role("button", name="检查本机连接", exact=True)).to_have_count(0)
            checks.append("program preferences load despite offline agent; defaults are tray and automatic agent stop")

            exit_app.check()
            expect(stop).to_be_enabled()
            expect(page.get_by_text("关闭 agent 将停止同步；重启后需恢复已暂停的同步目录。", exact=True)).to_be_visible()
            stop.uncheck()
            tray.check()
            expect(stop).not_to_be_checked()
            expect(stop).to_be_disabled()
            exit_app.check()
            expect(stop).not_to_be_checked()
            page.evaluate("settingsFixture.holdWrite=true")
            save.click()
            page.wait_for_function("settingsFixture.pending.length===1")
            expect(exit_app).to_be_disabled()
            expect(stop).to_be_disabled()
            expect(save).to_be_disabled()
            expect(feedback).not_to_have_text("程序配置已保存。")
            assert page.evaluate("settingsFixture.value") == {"close_action": "tray", "stop_agent_on_exit": True}
            page.evaluate("settingsFixture.pending.shift()();settingsFixture.holdWrite=false")
            expect(feedback).to_have_text("程序配置已保存。")
            assert page.evaluate("settingsFixture.writes") == [{"settings": {"close_action": "exit", "stop_agent_on_exit": False}}]
            checks.append("tray keeps independent stop preference; save waits for native acknowledgement and blocks duplicate edits")

            page.reload()
            expect(exit_app).to_be_checked()
            expect(stop).not_to_be_checked()
            stop.check()
            page.evaluate("settingsFixture.failWrite=true")
            save.click()
            expect(page.get_by_role("alert")).to_have_text("S200: 程序配置保存失败")
            expect(stop).to_be_checked()
            expect(save).to_be_enabled()
            assert page.evaluate("settingsFixture.value.stop_agent_on_exit") is False
            page.evaluate("settingsFixture.failWrite=false")
            save.click()
            expect(feedback).to_have_text("程序配置已保存。")
            assert page.evaluate("settingsFixture.value.stop_agent_on_exit") is True
            checks.append("saved native values survive reload; rejected save preserves draft and prior native value for retry")

            def return_to_settings():
                page.evaluate("location.hash='/connections'")
                expect(page.get_by_role("heading", name="连接设置", exact=True)).to_be_visible()
                page.get_by_role("link", name="程序配置", exact=True).click()

            page.evaluate("settingsFixture.failRead=true")
            return_to_settings()
            expect(page.get_by_role("alert")).to_have_text("S200: 程序配置损坏")
            expect(tray).to_be_disabled()
            expect(save).to_be_disabled()
            page.evaluate("settingsFixture.failRead=false;settingsFixture.malformed=true")
            page.get_by_role("button", name="重新读取", exact=True).click()
            expect(page.get_by_role("alert")).to_have_text("程序配置返回无效，请重新读取。")
            expect(save).to_be_disabled()
            page.evaluate("settingsFixture.malformed=false")
            page.get_by_role("button", name="重新读取", exact=True).click()
            expect(exit_app).to_be_checked()
            expect(stop).to_be_checked()
            checks.append("failed or malformed reads do not silently replace native settings; explicit reload recovers")

            page.evaluate("settingsFixture.failRead=true")
            return_to_settings()
            expect(page.get_by_role("alert")).to_be_visible()
            count = page.evaluate("settingsFixture.writes.length")
            page.get_by_role("button", name="使用默认配置", exact=True).click()
            expect(tray).to_be_checked()
            expect(save).to_be_enabled()
            assert page.evaluate("settingsFixture.writes.length") == count
            save.click()
            expect(feedback).to_have_text("程序配置已保存。")
            assert page.evaluate("settingsFixture.value") == {"close_action": "tray", "stop_agent_on_exit": True}
            checks.append("corrupt settings repair requires explicit default selection and save; no write on read failure")

            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({"width": width, "height": 1000})
                    expect(tray).to_be_visible()
                    assert page.evaluate("document.documentElement.scrollWidth<=innerWidth")
                    page.screenshot(path=str(OUTPUT / f"settings-{scheme}-{width}.png"), full_page=True)
            assert not errors, errors
            assert not page.evaluate("window.csp")
            checks.append("light/dark 320/1440 layouts, no page errors or CSP violations")
            (OUTPUT / "result.json").write_text(json.dumps({"ok": True, "checks": checks}, ensure_ascii=False, indent=2))
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
    print(f"S200 settings browser passed ({len(checks)} groups): {OUTPUT}")


if __name__ == "__main__":
    main()
