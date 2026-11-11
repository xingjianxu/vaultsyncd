#!/usr/bin/env python3
"""S196: real desktop controls preserve extra-rule text; IPC is an explicit fixture."""
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
OUTPUT = Path(os.environ["VAULTSYNCD_UI_ARTIFACTS"]) / "exclusion-rules"
BINDING = "1" * 32
FIXTURE = r"""() => {
  const original = window.__TAURI_INTERNALS__.invoke;
  const f = window.rulesFixture = {
    rules: '# 用户附加规则\n工作草稿/**\n!~$保留.docx\n',
    pending: [], reads: [], writes: [], malformed: false, failRead: false, failWrite: false,
  };
  window.__TAURI_INTERNALS__.invoke = async (command, args) => {
    if (command === 'agent_get_rules') {
      f.reads.push(args.bindingId);
      await new Promise(resolve => f.pending.push(resolve));
      if (f.failRead) throw { message: 'S196: 读取额外规则失败' };
      return f.malformed ? {} : { rules: f.rules };
    }
    if (command === 'agent_set_rules') {
      f.writes.push(structuredClone(args));
      if (f.failWrite) throw { message: 'S196: 保存额外规则失败' };
      f.rules = args.rules;
      return {};
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
            page.goto(f"http://localhost:{server.server_port}/#/bindings")
            page.get_by_role("button", name="管理与恢复", exact=True).click()
            rules = page.get_by_role("textbox", name="额外排除规则", exact=True)
            read = page.get_by_role("button", name="读取排除规则", exact=True)
            save = page.get_by_role("button", name="保存排除规则", exact=True)
            feedback = page.locator("[data-agent-feedback]")
            expect(rules).to_be_disabled()
            expect(save).to_be_disabled()
            expect(rules).to_have_value("")
            expect(page.get_by_label("完整排除清单", exact=True)).to_have_count(0)
            expect(page.get_by_text("自动排除常见临时文件（含 ~$*）；留空仅使用默认规则，可用 ! 重新包含默认排除的文件。排除不删除两端内容，取消排除后先重新核对。", exact=True)).to_be_visible()
            checks.append("extra-rule label and default/negation guidance; unread text cannot be saved")

            def start_read():
                read.click()
                page.wait_for_function("rulesFixture.pending.length===1")
                expect(rules).to_be_disabled()
                expect(save).to_be_disabled()

            def resolve_read():
                page.evaluate("rulesFixture.pending.shift()()")
                expect(read).to_be_enabled()

            page.evaluate("rulesFixture.failRead=true")
            start_read()
            resolve_read()
            expect(feedback).to_have_text("S196: 读取额外规则失败")
            expect(rules).to_have_value("")
            expect(rules).to_be_disabled()
            expect(save).to_be_disabled()
            page.evaluate("rulesFixture.failRead=false;rulesFixture.malformed=true")
            start_read()
            resolve_read()
            expect(feedback).to_have_text("未取得当前额外排除规则")
            expect(save).to_be_disabled()
            assert page.evaluate("rulesFixture.writes") == []
            checks.append("failed or missing initial result cannot masquerade as empty rules")

            page.evaluate("rulesFixture.malformed=false")
            original = page.evaluate("rulesFixture.rules")
            start_read()
            resolve_read()
            expect(rules).to_be_enabled()
            expect(rules).to_have_value(original)
            assert page.evaluate("rulesFixture.reads") == [BINDING] * 3
            extra = "# 保留用户文本与末尾换行\n临时工作/**\n!~$保留.docx\n"
            rules.fill(extra)
            save.click()
            expect(feedback).to_have_text("操作已由 agent 确认。")
            expect(rules).to_have_value(extra)
            first = page.evaluate("rulesFixture.writes[0]")
            assert first["bindingId"] == BINDING
            assert first["rules"] == extra
            assert len(first["rulesVersion"]) == 32 and all(char in "0123456789abcdef" for char in first["rulesVersion"])
            checks.append("reads only extra text and writes comments, Unicode, negation and trailing newline verbatim")

            draft = extra + "未保存的修改.txt\n"
            rules.fill(draft)
            page.evaluate("rulesFixture.failRead=true")
            start_read()
            expect(rules).to_have_value(draft)
            resolve_read()
            expect(feedback).to_have_text("S196: 读取额外规则失败")
            expect(rules).to_have_value(draft)
            expect(rules).to_be_enabled()
            page.evaluate("rulesFixture.failRead=false;rulesFixture.malformed=true")
            start_read()
            expect(rules).to_have_value(draft)
            resolve_read()
            expect(feedback).to_have_text("未取得当前额外排除规则")
            expect(rules).to_have_value(draft)
            expect(save).to_be_enabled()
            page.evaluate("rulesFixture.malformed=false;rulesFixture.failWrite=true")
            save.click()
            expect(feedback).to_have_text("S196: 保存额外规则失败")
            expect(rules).to_have_value(draft)
            assert page.evaluate("rulesFixture.rules") == extra
            checks.append("pending reads, read errors, missing fields and save errors preserve unsaved edits")

            page.evaluate("rulesFixture.failWrite=false")
            rules.fill("")
            save.click()
            expect(feedback).to_have_text("操作已由 agent 确认。")
            empty = page.evaluate("rulesFixture.writes.at(-1)")
            assert empty["rules"] == "" and empty["bindingId"] == BINDING
            assert empty["rulesVersion"] != first["rulesVersion"]
            start_read()
            resolve_read()
            expect(rules).to_have_value("")
            expect(save).to_be_enabled()
            checks.append("clearing sends empty extra text with fresh version; empty read succeeds without copying defaults")

            # A fresh management card must accept a genuinely empty first read as loaded.
            page.reload()
            page.get_by_role("button", name="管理与恢复", exact=True).click()
            page.evaluate("rulesFixture.rules=''")
            start_read()
            resolve_read()
            expect(rules).to_be_enabled()
            expect(rules).to_have_value("")
            expect(save).to_be_enabled()
            checks.append("initial empty read is valid, not a loading or error sentinel")
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({"width": width, "height": 1000})
                    expect(rules).to_be_visible()
                    assert page.evaluate("document.documentElement.scrollWidth<=innerWidth")
                    page.screenshot(path=str(OUTPUT / f"rules-{scheme}-{width}.png"), full_page=True)
            assert not errors, errors
            assert not page.evaluate("window.csp")
            checks.append("light/dark 320/1440 layout, no page errors or CSP violations")
            (OUTPUT / "result.json").write_text(json.dumps({"ok": True, "checks": checks}, ensure_ascii=False, indent=2))
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
    print(f"S196 extra exclusion rules browser passed ({len(checks)} groups): {OUTPUT}")


if __name__ == "__main__":
    main()
