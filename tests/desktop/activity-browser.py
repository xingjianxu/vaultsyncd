#!/usr/bin/env python3
"""S202: built desktop UI live-refresh contract in Chromium (explicit IPC mock).

Run after building apps/desktop, using nix develop .#web-tests --command
python3 tests/desktop/activity-browser.py. Set VAULTSYNCD_UI_ARTIFACTS to the
isolated task root. Native IPC/TLS evidence remains s172-activity.js, not this mock.
"""
from datetime import datetime, timezone
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
OUTPUT = Path(os.environ["VAULTSYNCD_UI_ARTIFACTS"]) / "activity-live"
A, B = "1" * 32, "2" * 32
FIXTURE = r"""() => {
  const binding = (id, server) => ({ binding_id: id.repeat(32), server_id: server.repeat(32), vault_id: id.repeat(32), local_path: `C:\\S202\\${id}`, mode: 'full', detached: false, paused: false });
  const connections = [7,8].map(id => ({server_id:String(id).repeat(32),server_url:`https://${id}.example.invalid`,authenticated:true}));
  const bindings = [binding('1','7'),binding('2','8')];
  const f = window.fixture = {calls:[], connections, bindings, hidden:false, csp:[], emptyDuringRefresh:false};
  Object.defineProperty(document,'visibilityState',{get:()=>f.hidden?'hidden':'visible'});
  Object.defineProperty(document,'hidden',{get:()=>f.hidden});
  document.addEventListener('securitypolicyviolation', e => f.csp.push(e.effectiveDirective));
  f.visibility = hidden => { f.hidden=hidden; document.dispatchEvent(new Event('visibilitychange')); };
  f.reply = (index, error=false, marker) => {
    const call=f.calls[index];
    if (!call || call.finished !== undefined) throw new Error(`not pending: ${index}`);
    call.finished=Date.now();
    if (error) call.reject({message:'S202 activity temporarily unavailable'});
    else {
      if (marker) call.result.activity[0].path=marker;
      call.resolve(call.result);
    }
  };
  window.__TAURI_INTERNALS__ = {invoke: async (command,args) => {
    if (command==='agent_get_status') return {bindings:[],management:{connections:structuredClone(connections),bindings:structuredClone(bindings),problems:[],notices:[]}};
    if (command==='agent_list_activity') {
      const direction=args.direction || 'download';
      const second=!!args.cursor;
      const row = (id, path) => ({id, vault_id:args.bindingId, occurred_at:'1794300000',direction,
        action:direction==='download'?'download':'sync_commit',status:direction==='download'?'served':'committed',
        path,client_hostname:'S202 fixture client',version_id:'9'.repeat(32),offset:'0',bytes:'8',size:'8'});
      const call={args:structuredClone(args),started:Date.now(),result:{activity:[row(second?'49':'100',second?'second-page.txt':'first-page.txt'),row(second?'48':'99','retained.txt')],...(second?{}:{next_cursor:'50'})}};
      if (direction==='operation') {
        call.result.activity=['create_directory','move','delete','restore_version','restore_deleted'].map((action,index)=>({...row(String(100-index),`operation-${action}`),action,status:'committed',version_id:undefined,bytes:undefined,offset:undefined,size:undefined}));
        delete call.result.next_cursor;
      }
      f.calls.push(call);
      return new Promise((resolve,reject)=>Object.assign(call,{resolve,reject}));
    }
    throw {message:`unexpected IPC ${command}`};
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
            page.clock.install(time=datetime(2026, 11, 11, tzinfo=timezone.utc))
            page.clock.pause_at(datetime(2026, 11, 11, 0, 1, tzinfo=timezone.utc))
            origin = f"http://localhost:{server.server_port}"
            page.goto(origin + "/#/activity")
            table = page.get_by_role("table", name="同步与传输记录当前页", exact=True)
            binding = page.locator('[name="activity_binding"]')
            direction = page.locator('[name="activity_direction"]')
            dialog = page.get_by_role("dialog", name="记录详情", exact=True)
            first = page.get_by_role("button", name="第一页", exact=True)
            refresh = page.get_by_role("button", name="刷新记录", exact=True)

            def count():
                return page.evaluate("fixture.calls.length")

            def calls(number):
                page.wait_for_function("n=>fixture.calls.length===n", arg=number)

            def reply(index, *, error=False, marker=None):
                page.evaluate("([index,error,marker])=>fixture.reply(index,error,marker)", [index, error, marker])

            def tick(milliseconds):
                page.clock.run_for(milliseconds)

            def rows():
                return table.locator("tbody").inner_text()

            def monitor():
                page.evaluate("""() => {
                  fixture.emptyDuringRefresh=false;
                  fixture.observer=new MutationObserver(()=>{
                    const rows=[...document.querySelectorAll('table[aria-label="同步与传输记录当前页"] tbody tr')];
                    if (!rows.some(row=>row.children.length===7)) fixture.emptyDuringRefresh=true;
                  });
                  fixture.observer.observe(document.querySelector('main'),{subtree:true,childList:true});
                }""")

            def stop_monitor():
                assert page.evaluate("!fixture.emptyDuringRefresh"), "loaded rows flashed empty"
                page.evaluate("fixture.observer.disconnect()")

            # No clock advancement: the visible mount must load immediately.
            calls(1)
            expect(binding).to_have_value(A)
            reply(0)
            expect(table.get_by_text("first-page.txt", exact=True)).to_be_visible()
            tick(1999)
            assert count() == 1
            tick(1)
            calls(2)
            monitor()
            tick(6500)
            page.evaluate("window.dispatchEvent(new Event('focus'))")
            assert count() == 2, "timer/focus overlapped a pending IPC query"
            expect(table.get_by_text("first-page.txt", exact=True)).to_be_visible()
            reply(1, marker="newly-observed.txt")
            expect(table.get_by_text("newly-observed.txt", exact=True)).to_be_visible()
            tick(1999)
            assert count() == 2, "next poll was based on request start"
            tick(1)
            calls(3)
            assert page.evaluate("fixture.calls[2].started-fixture.calls[1].finished") == 2000
            reply(2)
            stop_monitor()
            checks.append("visible mount immediate; serial completion+2000ms; no overlap; new records without click; no flash")

            page.get_by_role("button", name="下一页", exact=True).click()
            calls(4)
            assert page.evaluate("fixture.calls[3].args.cursor") == "50"
            reply(3)
            expect(table.get_by_text("second-page.txt", exact=True)).to_be_visible()
            saved_rows = rows()
            table.get_by_role("button", name="查看详情", exact=True).first.click()
            tick(250)  # Mantine mounts the modal on an animation frame.
            expect(dialog).to_be_visible()
            detail = dialog.text_content()
            monitor()
            tick(2000)
            calls(5)
            assert page.evaluate("fixture.calls[4].args.cursor") == "50"
            expect(dialog).to_have_text(detail)
            assert rows() == saved_rows
            reply(4)
            expect(dialog).to_have_text(detail)
            assert rows() == saved_rows
            stop_monitor()
            checks.append("automatic refresh retains current cursor, loaded rows and record detail")

            tick(2000)
            calls(6)
            reply(5, error=True)
            expect(page.get_by_role("alert")).to_contain_text("S202 activity temporarily unavailable")
            assert rows() == saved_rows
            expect(dialog).to_have_text(detail)
            tick(2000)
            calls(7)
            assert page.evaluate("fixture.calls[6].args.cursor") == "50"
            reply(6)
            expect(page.get_by_role("alert")).to_have_count(0)
            assert rows() == saved_rows
            expect(dialog).to_have_text(detail)
            page.keyboard.press("Escape")
            tick(250)
            expect(first).to_be_enabled()
            refresh.click()
            calls(8)
            assert page.evaluate("fixture.calls[7].args.cursor===undefined")
            reply(7)
            expect(table.get_by_text("first-page.txt", exact=True)).to_be_visible()
            expect(first).to_be_disabled()
            checks.append("automatic failure retains page/detail; automatic success clears error; manual refresh resets first page")

            page.evaluate("fixture.visibility(true)")
            tick(8000)
            assert count() == 8, "hidden page polled"
            page.evaluate("fixture.visibility(false)")
            calls(9)  # No clock advance: visibility must refresh immediately.
            reply(8)
            page.evaluate("window.dispatchEvent(new Event('focus'))")
            calls(10)
            tick(4000)
            assert count() == 10
            reply(9)
            checks.append("hidden pauses; visibility/focus immediately refresh; repeated focus never overlaps")

            # Each context change starts a new generation; pending replies from the
            # old one are deliberately delivered *after* the new result.
            tick(2000)
            calls(11)
            direction.select_option("upload")
            calls(12)
            reply(11, marker="current-upload.txt")
            expect(table.get_by_text("current-upload.txt", exact=True)).to_be_visible()
            reply(10, marker="late-download.txt")
            expect(table.get_by_text("late-download.txt", exact=True)).to_have_count(0)
            expect(table.get_by_role("img", name="上传", exact=True)).to_have_count(2)

            tick(2000)
            calls(13)
            binding.select_option(B)
            calls(14)
            reply(13, marker="binding-b.txt")
            expect(table.get_by_text("binding-b.txt", exact=True)).to_be_visible()
            reply(12, marker="late-binding-a.txt")
            expect(table.get_by_text("late-binding-a.txt", exact=True)).to_have_count(0)
            checks.append("direction and binding changes discard delayed old responses")

            # Auth is received through the existing App status IPC, not component
            # internals. A stale authenticated reply must not restore private rows.
            binding.select_option(A)
            calls(15)
            reply(14)
            tick(2000)
            calls(16)
            page.evaluate("fixture.connections[0].authenticated=false")
            page.get_by_role("button", name="检查本机连接", exact=True).click()
            expect(page.get_by_text("请先登录此绑定的服务器。", exact=True)).to_be_visible()
            reply(15, marker="late-authenticated.txt")
            expect(table.get_by_text("late-authenticated.txt", exact=True)).to_have_count(0)
            assert table.get_by_role("button", name="查看详情", exact=True).count() == 0
            previous = count()
            tick(6000)
            assert count() == previous
            page.evaluate("fixture.connections[0].authenticated=true")
            page.get_by_role("button", name="检查本机连接", exact=True).click()
            calls(17)
            reply(16)
            expect(table.get_by_text("first-page.txt", exact=True)).to_be_visible()
            checks.append("logout clears rows and invalidates pending response; no polling unauthenticated; login loads immediately")

            tick(2000)
            calls(18)
            page.evaluate("fixture.connections[0].server_url='https://changed.example.invalid'")
            page.get_by_role("button", name="检查本机连接", exact=True).click()
            calls(19)
            reply(18, marker="new-endpoint.txt")
            expect(table.get_by_text("new-endpoint.txt", exact=True)).to_be_visible()
            reply(17, marker="late-old-endpoint.txt")
            expect(table.get_by_text("late-old-endpoint.txt", exact=True)).to_have_count(0)
            checks.append("connection endpoint change discards pending old-server response")

            tick(2000)
            calls(20)
            page.evaluate("location.hash='/problems'")
            expect(page.get_by_role("heading", name="问题与恢复", exact=True)).to_be_visible()
            reply(19, marker="late-unmounted.txt")
            previous = count()
            tick(6000)
            assert count() == previous, "unmounted route kept polling"
            page.evaluate("location.hash='/activity'")
            calls(21)
            reply(20, marker="fresh-route.txt")
            expect(table.get_by_text("fresh-route.txt", exact=True)).to_be_visible()
            expect(table.get_by_text("late-unmounted.txt", exact=True)).to_have_count(0)
            checks.append("route leave stops polling and invalidates pending responses; remount loads immediately")
            direction.select_option("operation")
            calls(22)
            assert page.evaluate("fixture.calls[21].args.direction") == "operation"
            reply(21)
            expect(table.locator("tbody tr")).to_have_count(5)
            for label in ["新建目录", "移动/重命名", "删除", "恢复历史版本", "从回收站恢复"]:
                expect(table.get_by_role("img", name=label, exact=True)).to_have_count(1)
            for result in ["目录已创建", "移动/重命名已提交", "删除已提交", "历史版本已恢复", "回收站条目已恢复"]:
                expect(table.get_by_text(result, exact=True)).to_have_count(1)
            expect(table.get_by_role("img", name="上传", exact=True)).to_have_count(0)
            expect(table.get_by_role("img", name="下载", exact=True)).to_have_count(0)
            table.get_by_role("row").filter(has=page.get_by_text("operation-delete", exact=True)).get_by_role("button", name="查看详情").click()
            tick(250)
            expect(dialog.get_by_text("operation-delete", exact=True)).to_be_visible()
            expect(dialog.get_by_text("删除已提交", exact=True)).to_be_visible()
            expect(dialog.get_by_text("上传会话", exact=True)).to_have_count(0)
            page.keyboard.press("Escape")
            tick(250)
            for scheme in ["light", "dark"]:
                page.emulate_media(color_scheme=scheme)
                for width in [320, 1440]:
                    page.set_viewport_size({"width": width, "height": 1000})
                    page.screenshot(path=str(OUTPUT / f"operations-{scheme}-{width}.png"))
            tick(2000)
            calls(23)
            direction.select_option("upload")
            calls(24)
            reply(23)
            reply(22, marker="late-operation.txt")
            expect(table.get_by_text("late-operation.txt", exact=True)).to_have_count(0)
            expect(table.get_by_role("img", name="删除", exact=True)).to_have_count(0)
            expect(table.get_by_role("img", name="上传", exact=True)).to_have_count(2)
            checks.append("five committed file operations use distinct actions/results; operation IPC filter/detail; no upload/download masquerade; late operation response discarded; light/dark 320/1440")
            assert errors == [], errors
            assert page.evaluate("fixture.csp") == []
            page.screenshot(path=str(OUTPUT / "activity-live.png"))
            (OUTPUT / "result.json").write_text(json.dumps({"ok": True, "checks": checks, "activity_calls": count()}, ensure_ascii=False, indent=2))
            browser.close()
            print(f"S202 desktop activity browser PASS: {OUTPUT}")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
