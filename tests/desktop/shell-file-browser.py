#!/usr/bin/env python3
"""S204: built file-action window in Chromium; native Explorer/CFAPI is tested separately."""
import functools
import http.server
import importlib.util
import json
import os
from pathlib import Path
import sys
import threading

from playwright.sync_api import expect, sync_playwright

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("desktop_ui", Path(__file__).with_name("ui-browser.py"))
ui = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ui)
OUTPUT = Path(os.environ["VAULTSYNCD_UI_ARTIFACTS"]) / "shell-file"
FIXTURE = r"""options => {
  window.csp = [];
  document.addEventListener('securitypolicyviolation', e => csp.push(e.effectiveDirective));
  const binding = {
    binding_id:'8'.repeat(32), server_id:'7'.repeat(32), vault_id:'2'.repeat(32),
    local_path:'C:\\S204\\同步目录', cache_path:'C:\\S204\\cache', mode:'on_demand', paused:false, detached:false,
    ...options.binding,
  };
  const target = { action:options.action || 'history', binding_id:binding.binding_id, entry_id:'b'.repeat(32), path:'嵌套目录/中文 & 100% !.txt', ...options.target };
  const file = {
    entry_id:target.entry_id, path:target.path, availability:'online', busy:false, dirty:false, pending:false,
    release_allowed:true, release_reason:'', remote_apply:'current', residency:'materialized', status:'downloaded', synchronized:true, transfer:'idle',
    ...options.file,
  };
  const f = window.shellFixture = {
    calls:[], target, binding, file, failHistory:false, malformedHistory:false, failStatus:false,
    failAction:false, completeThenLoseResponse:false, completed:[], holdAction:false, pending:[], ...options.flags,
  };
  if (options.noNative) return;
  window.__TAURI_INTERNALS__ = { invoke: async (command, args) => {
    f.calls.push({command, args:args || {}});
    if (command === 'get_shell_target') return options.malformedTarget ? {} : structuredClone(f.target);
    if (command === 'shell_file_status') {
      if (f.failStatus) throw {message:'S204: 文件状态读取失败'};
      return {file:structuredClone(f.file), binding:structuredClone(f.binding)};
    }
    if (command === 'agent_list_history') {
      if (f.failHistory) throw {message:'S204: 历史读取失败，保留当前页'};
      if (f.malformedHistory) return {};
      if (options.emptyHistory) return {items:[]};
      return args.cursor ? {items:[{id:'version-older', entry_id:target.entry_id, name:'历史第二页', version_id:'v-old', size:'7', hash:'a'.repeat(64)}]}
        : {items:[{id:'version-current', entry_id:target.entry_id, name:'历史第一页', version_id:'v-current', size:'9007199254740993', hash:'b'.repeat(64)}], next_cursor:'opaque-next'};
    }
    if (command === 'shell_file_action') {
      if (f.holdAction) await new Promise(resolve => f.pending.push(resolve));
      if (f.failAction) throw {message:'S204: IPC 应答丢失，结果未知'};
      if (target.action === 'release') { f.file.release_allowed = false; f.file.residency = 'metadata_only'; f.file.status = 'cloud_only'; }
      if (f.completeThenLoseResponse && !f.completed.includes(args.operationId)) {
        f.completed.push(args.operationId);
        throw {message:'S204: 已释放但 IPC 应答丢失，结果未知'};
      }
      return {operation_id:args.operationId};
    }
    throw {message:`unexpected command ${command}`};
  }};
}"""


def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(ui.DesktopStaticFiles, directory=str(ui.REPO / "apps/desktop/dist")))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    checks, errors, pages = [], [], []
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)

            def open_window(options=None):
                page = browser.new_page(viewport={"width": 1000, "height": 800})
                pages.append(page)
                page.on("pageerror", lambda error: errors.append(str(error)))
                page.add_init_script(f"({FIXTURE})({json.dumps(options or {}, ensure_ascii=False)})")
                page.goto(f"http://localhost:{server.server_port}/#/shell")
                expect(page.locator("[data-shell-file]")).to_be_visible()
                return page

            page = open_window()
            expect(page.get_by_role("heading", name="查看文件历史", exact=True)).to_be_visible()
            expect(page.get_by_text("历史第一页", exact=True)).to_be_visible()
            expect(page.locator("[data-shell-path]")).to_have_text(r"C:\S204\同步目录\嵌套目录\中文 & 100% !.txt")
            expect(page.get_by_role("combobox")).to_have_count(0)
            expect(page.get_by_role("navigation")).to_have_count(0)
            calls = page.evaluate("shellFixture.calls.filter(c=>c.command==='agent_list_history')")
            assert calls and all(c["args"] == {"bindingId": "8" * 32, "entryId": "b" * 32, "cursor": None} for c in calls), calls
            expect(page.get_by_text("9007199254740993 字节", exact=True)).to_be_visible()
            page.get_by_role("button", name="下一页", exact=True).click()
            expect(page.get_by_text("历史第二页", exact=True)).to_be_visible()
            assert page.evaluate("shellFixture.calls.at(-1).args") == {"bindingId": "8" * 32, "entryId": "b" * 32, "cursor": "opaque-next"}
            page.evaluate("shellFixture.failHistory=true")
            page.get_by_role("button", name="第一页", exact=True).click()
            expect(page.get_by_role("alert")).to_contain_text("S204: 历史读取失败")
            expect(page.get_by_text("历史第二页", exact=True)).to_be_visible()
            expect(page.get_by_text("当前页没有历史版本。", exact=True)).to_have_count(0)
            page.evaluate("shellFixture.failHistory=false;shellFixture.malformedHistory=true")
            page.get_by_role("button", name="刷新", exact=True).click()
            expect(page.get_by_role("alert")).to_have_text("agent 未返回文件历史。")
            page.evaluate("shellFixture.malformedHistory=false")
            page.get_by_role("button", name="第一页", exact=True).click()
            expect(page.get_by_text("历史第一页", exact=True)).to_be_visible()
            checks.append("history resolves original binding/EntryId directly, exact large byte count, opaque cursor and errors preserve current history")

            release = open_window({"action": "release"})
            request = release.get_by_role("button", name="释放空间", exact=True)
            expect(request).to_be_enabled()
            assert release.evaluate("shellFixture.calls.filter(c=>c.command==='shell_file_action').length") == 0
            request.click()
            dialog = release.get_by_role("dialog", name="确认释放本地内容")
            expect(dialog).to_contain_text("远端数据保留；离线将不可读取")
            dialog.get_by_role("button", name="关闭", exact=True).click()
            assert release.evaluate("shellFixture.calls.filter(c=>c.command==='shell_file_action').length") == 0
            release.evaluate("shellFixture.holdAction=true")
            request.click()
            dialog.get_by_role("button", name="确认释放", exact=True).click()
            release.wait_for_function("shellFixture.pending.length===1")
            expect(release.get_by_role("button", name="核对原操作", exact=True)).to_be_disabled()
            expect(release.get_by_role("button", name="刷新", exact=True)).to_be_disabled()
            calls = release.evaluate("shellFixture.calls.filter(c=>c.command==='shell_file_action')")
            assert len(calls) == 1 and set(calls[0]["args"]) == {"operationId"}
            assert len(calls[0]["args"]["operationId"]) == 32
            release.evaluate("shellFixture.pending.shift()();shellFixture.holdAction=false")
            expect(release.get_by_role("status")).to_have_text("释放请求已由 agent 确认。")
            expect(release.get_by_role("button", name="释放空间", exact=True)).to_be_disabled()
            expect(release.get_by_text("仅云端", exact=True)).to_be_visible()
            checks.append("release never starts on mount; confirmation/cancel, one in-flight operation, no caller-controlled path/action and explicit acknowledgement")

            for name, options in [
                ("dirty", {"file": {"dirty": True}}), ("busy", {"file": {"busy": True}}),
                ("pending", {"file": {"pending": True}}),
                ("full", {"binding": {"mode": "full"}, "file": {"release_allowed": False, "release_reason": "完整模式保留本地内容"}}),
                ("release-denied", {"file": {"release_allowed": False, "release_reason": "尚未确认服务器固定版本"}}),
                ("status-offline", {"flags": {"failStatus": True}}),
            ]:
                denied = open_window({"action": "release", **options})
                expect(denied.get_by_role("button", name="释放空间", exact=True)).to_be_disabled()
                assert denied.evaluate("shellFixture.calls.filter(c=>c.command==='shell_file_action').length") == 0, name
                if name == "status-offline":
                    expect(denied.get_by_role("alert")).to_have_text("S204: 文件状态读取失败")
            checks.append("dirty/busy/pending/full/forbidden release and unreadable state all prevent mutation")

            for options in [
                {"file": {"path": "已移动.txt"}}, {"file": {"entry_id": "c" * 32}},
                {"binding": {"detached": True}},
            ]:
                changed = open_window({"action": "release", **options})
                expect(changed.get_by_role("alert")).to_contain_text("文件位置或绑定身份已变化")
                expect(changed.get_by_role("button", name="释放空间", exact=True)).to_be_disabled()
            changed = open_window({"action": "release"})
            expect(changed.get_by_role("button", name="释放空间", exact=True)).to_be_enabled()
            changed.evaluate("shellFixture.file.path='打开窗口后改名.txt'")
            changed.get_by_role("button", name="刷新", exact=True).click()
            expect(changed.get_by_role("alert")).to_contain_text("文件位置或绑定身份已变化")
            expect(changed.get_by_role("button", name="释放空间", exact=True)).to_be_disabled()
            assert changed.evaluate("shellFixture.calls.filter(c=>c.command==='shell_file_action').length") == 0
            checks.append("renamed path, replaced EntryId and detached binding reject stale selection")

            retry = open_window({"action": "release", "flags": {"failAction": True}})
            retry.get_by_role("button", name="释放空间", exact=True).click()
            retry.get_by_role("button", name="确认释放", exact=True).click()
            expect(retry.get_by_role("alert")).to_contain_text("结果未知")
            expect(retry.get_by_role("button", name="核对原操作", exact=True)).to_be_disabled()
            retry.evaluate("shellFixture.failAction=false")
            retry.get_by_role("button", name="刷新", exact=True).click()
            expect(retry.get_by_role("button", name="核对原操作", exact=True)).to_be_enabled()
            retry.get_by_role("button", name="核对原操作", exact=True).click()
            retry.get_by_role("button", name="确认释放", exact=True).click()
            expect(retry.get_by_role("status")).to_have_text("释放请求已由 agent 确认。")
            attempts = retry.evaluate("shellFixture.calls.filter(c=>c.command==='shell_file_action').map(c=>c.args)")
            assert len(attempts) == 2 and attempts[0] == attempts[1], attempts
            completed = open_window({"action": "release", "flags": {"completeThenLoseResponse": True}})
            completed.get_by_role("button", name="释放空间", exact=True).click()
            completed.get_by_role("button", name="确认释放", exact=True).click()
            expect(completed.get_by_role("alert")).to_contain_text("已释放但 IPC 应答丢失")
            expect(completed.get_by_role("button", name="核对原操作", exact=True)).to_be_disabled()
            assert completed.evaluate("shellFixture.file.release_allowed") is False
            completed.get_by_role("button", name="刷新", exact=True).click()
            expect(completed.get_by_text("仅云端", exact=True)).to_be_visible()
            expect(completed.get_by_role("button", name="核对原操作", exact=True)).to_be_enabled()
            completed.get_by_role("button", name="核对原操作", exact=True).click()
            completed.get_by_role("button", name="确认释放", exact=True).click()
            expect(completed.get_by_role("status")).to_have_text("释放请求已由 agent 确认。")
            attempts = completed.evaluate("shellFixture.calls.filter(c=>c.command==='shell_file_action').map(c=>c.args)")
            assert len(attempts) == 2 and attempts[0] == attempts[1], attempts
            assert completed.evaluate("shellFixture.completed.length") == 1
            checks.append("lost action response clears writable state; explicit refresh/reconfirm retries identical identity even after release completed and release_allowed became false")

            download = open_window({"action": "download", "file": {"status": "cloud_only", "residency": "metadata_only"}})
            expect(download.get_by_role("button", name="下载到本地", exact=True)).to_be_enabled()
            assert download.evaluate("shellFixture.calls.filter(c=>c.command==='shell_file_action').length") == 0
            download.get_by_role("button", name="下载到本地", exact=True).click()
            expect(download.get_by_role("status")).to_have_text("下载请求已由 agent 接收，请刷新查看文件状态。")
            expect(download.get_by_role("button", name="下载到本地", exact=True)).to_be_disabled()
            expect(download.get_by_text("下载完成", exact=True)).to_have_count(0)
            paused = open_window({"action": "download", "binding": {"paused": True}})
            expect(paused.get_by_role("button", name="下载到本地", exact=True)).to_be_disabled()
            checks.append("download is explicit, accepted does not claim completion, paused binding cannot start")

            missing = open_window({"noNative": True})
            expect(missing.get_by_role("alert")).to_have_text("请从 Windows 文件右键菜单打开此窗口。")
            malformed = open_window({"malformedTarget": True})
            expect(malformed.get_by_role("alert")).to_have_text("右键操作目标无效，请重新打开右键菜单。")
            empty = open_window({"emptyHistory": True})
            expect(empty.get_by_text("当前页没有历史版本。", exact=True)).to_be_visible()
            expect(empty.get_by_role("alert")).to_have_count(0)
            checks.append("missing native invocation and malformed target are errors; only successful empty history is shown as empty")

            # Reopen with a long native selection so the immutable target is preserved.
            layout = open_window({"target": {"path": "嵌套/" + "长文件名" * 30 + " & %.txt"}})
            expect(layout.get_by_text("历史第一页", exact=True)).to_be_visible()
            for scheme in ("light", "dark"):
                layout.emulate_media(color_scheme=scheme)
                for width in (320, 390, 1440):
                    layout.set_viewport_size({"width": width, "height": 900})
                    expect(layout.get_by_role("heading", name="查看文件历史", exact=True)).to_be_visible()
                    assert layout.evaluate("document.documentElement.scrollWidth<=innerWidth"), (scheme, width)
                    layout.screenshot(path=str(OUTPUT / f"shell-history-{scheme}-{width}.png"), full_page=True)
            for item in pages:
                assert not item.evaluate("window.csp"), item.evaluate("window.csp")
            assert not errors, errors
            checks.append("long Unicode/special paths, both themes at 320/390/1440, no document overflow, CSP violation or JavaScript error")
            (OUTPUT / "result.json").write_text(json.dumps({"ok": True, "checks": checks}, ensure_ascii=False, indent=2))
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
    print(f"S204 shell file browser passed ({len(checks)} groups): {OUTPUT}")


if __name__ == "__main__":
    main()
