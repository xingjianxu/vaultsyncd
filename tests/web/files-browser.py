#!/usr/bin/env python3
"""S124: built Web + real Chromium + isolated btrfs service, no API fixtures.

VAULTSYNCD_TEST_BIN_DIR=... nix develop .#web-tests --command python3 tests/web/files-browser.py
Network faults only affect this run's proxy; no online SQL or production writes.
"""
import functools
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
from urllib.parse import quote
import secrets
import subprocess
import threading
import time

from playwright.sync_api import expect, sync_playwright
from csp_support import CspStaticFiles, field_label

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug"))
RUN = Path("/data") / f"vs-s124-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


def checked(args, **kwargs):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, **kwargs)
    assert result.returncode == 0, f"{args[0]} failed: {result.stderr}"
    return result.stdout


def main():
    checked(["sudo", "-n", "mkdir", "-m", "755", RUN])
    checked(["sudo", "-n", "chown", f"{os.getuid()}:{os.getgid()}", RUN])
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    print(f"S124 artifacts: {RUN}", flush=True)
    state, roots = RUN / "state", RUN / "roots"
    state.mkdir(mode=0o700); roots.mkdir()
    documents, archive = roots / "documents", roots / "archive"
    documents.mkdir(); archive.mkdir()
    nested = documents / "文档" / "子目录"; nested.mkdir(parents=True)
    (documents / "空目录").mkdir()
    sample = nested / "report & '文档'.txt"; sample.write_bytes(b"S124 read-only browser\n")
    digest = hashlib.sha256(sample.read_bytes()).hexdigest()
    special = documents / "URL # % & 空 格" / "子 + %2F"
    special.mkdir(parents=True)
    (special / "deep.txt").write_bytes(b"URL names\n")
    (archive / "archive-only.txt").write_bytes(b"second root\n")
    for index in range(1005): (documents / f"file-{index:04}.txt").write_bytes(b"x")
    long_name = "long-" + "a" * 160 + ".txt"
    (documents / long_name).write_bytes(b"")
    password = secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", state], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[roots]\nallowed_bases=["{roots}"]\n[log]\nformat="json"\nlevel="info"\n')
    log = (RUN / "server.log").open("w")
    service = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log)
    proxy = None
    faults = {"mode": None, "root": None}
    delayed, release = threading.Event(), threading.Event()
    requests = []
    try:
        address = None
        for _ in range(150):
            for line in (RUN / "server.log").read_text().splitlines():
                fields = json.loads(line).get("fields", {})
                if fields.get("message") == "服务端开始监听": address = fields["listen"]
            if address: break
            assert service.poll() is None, "service exited"
            time.sleep(.2)
        assert address, "service did not listen"

        class Handler(CspStaticFiles):
            def log_message(self, *_): pass

            def do_POST(self): self.forward()

            def do_GET(self):
                if self.path.startswith("/v1/"): self.forward()
                else: super().do_GET()

            def forward(self):
                backend = http.client.HTTPConnection(address, timeout=30)
                path = self.path
                is_entries = "/snapshots/" in path and "/entries" in path
                should_delay = is_entries and faults["mode"] == "delay" and faults["root"] in path
                if is_entries and faults["mode"] == "expired":
                    parts = path.split("/"); parts[5] = "0" * 32; path = "/".join(parts)
                headers = {key: value for key, value in self.headers.items() if key.lower() not in ("connection", "transfer-encoding")}
                body = self.rfile.read(int(self.headers.get("content-length", "0")))
                try:
                    backend.request(self.command, path, body=body, headers=headers)
                    response = backend.getresponse(); payload = response.read()
                    requests.append({"path": self.path, "method": self.command, "status": response.status})
                    if should_delay:
                        delayed.set(); release.wait(timeout=30)
                    self.send_response(response.status)
                    for key, value in response.getheaders():
                        if key.lower() not in ("connection", "transfer-encoding", "content-length"): self.send_header(key, value)
                    self.send_header("content-length", str(len(payload))); self.end_headers()
                    self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # Browser cancellation is the intended network fault in the race case.
                finally: backend.close()

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=str(REPO / "apps/web/dist")))
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        origin = f"http://localhost:{proxy.server_port}"
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            context = browser.new_context(viewport={"width": 1440, "height": 1000})
            page = context.new_page(); errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.add_init_script("window.__csp=[];document.addEventListener('securitypolicyviolation',e=>window.__csp.push(e.effectiveDirective));")
            page.goto(origin + "/#/files")
            expect(page.get_by_role("heading", name="请先登录", exact=True)).to_be_visible()
            page.goto(origin + "/#/account")
            page.get_by_label(field_label("登录名")).fill("owner")
            page.get_by_label(field_label("密码")).fill(password)
            page.get_by_role("button", name="登录", exact=True).click()
            expect(page.get_by_text("已登录：owner", exact=True)).to_be_visible()
            page.goto(origin + "/#/files")
            expect(page.get_by_text("尚无授权 Root，请先在 Root 目录页面注册目录。", exact=True)).to_be_visible()

            def api(path, body=None):
                result = page.evaluate("""async ([path, body]) => {const r=await fetch(path,body===null?{}:{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});return {status:r.status,body:await r.json()}}""", [path, body])
                assert result["status"] in (200, 201), result
                return result["body"]

            doc_id = api("/v1/admin/roots", {"path": str(documents)})["root_id"]
            arc_id = api("/v1/admin/roots", {"path": str(archive)})["root_id"]
            for attempt in range(180):
                managed = api("/v1/admin/roots")["roots"]
                if len(managed) == 2 and all(item["collection"]["state"] == "complete" for item in managed): break
                if attempt % 20 == 0: print(f"waiting for real observer: {attempt}", flush=True)
                page.wait_for_timeout(500)
            else: raise AssertionError("observer did not complete initial import")
            # Existing observer imports regular files and their parents, not external
            # empty directories. Seed a committed empty directory through the public
            # operation API, then audit only requests made by the browser feature.
            (documents / "空目录").rmdir()
            seed = api(f"/v1/roots/{doc_id}/snapshots", {})
            cursor, root_entry = None, None
            while True:
                page_data = api(f"/v1/roots/{doc_id}/snapshots/{seed['snapshot_id']}/entries?limit=1000" + (f"&cursor={cursor}" if cursor else ""))
                root_entry = next((entry["entry_id"] for entry in page_data["entries"] if "parent_id" not in entry), root_entry)
                cursor = page_data.get("next_cursor")
                if cursor is None: break
            assert root_entry
            made = api(f"/v1/roots/{doc_id}/operations", {"kind": "create", "op_id": secrets.token_hex(16), "device_id": secrets.token_hex(16), "server_epoch": seed["server_epoch"], "parent_id": root_entry, "name": "空目录", "expected_absent": True})
            assert made["state"] == "committed", made
            browsing_start = len(requests)
            page.reload()

            def choose(name, root_id):
                page.get_by_role("combobox", name="浏览 Root", exact=True).click()
                page.get_by_role("option", name=f"{name} · {root_id[:8]}", exact=True).click()

            choose("documents", doc_id)
            table = page.get_by_role("table", name="文件与目录", exact=True)
            expect(table).to_be_visible(timeout=15000)
            assert len(table.locator("tbody tr").all()) == 100
            expect(page.get_by_role("heading", name="文件浏览器", exact=True)).not_to_be_visible()
            expect(page.get_by_text("切换 Root、浏览目录并查看文件属性。", exact=True)).not_to_be_visible()
            expect(page.get_by_role("button", name="刷新 Root 列表", exact=True)).not_to_be_visible()
            expect(page.get_by_role("tree", name="目录树", exact=True)).to_be_visible()
            panel = page.locator(".files-browser").bounding_box()
            sidebar = page.locator(".files-sidebar").bounding_box()
            toolbar = page.get_by_role("toolbar", name="文件浏览器工具栏").bounding_box()
            listing = page.locator(".files-list").bounding_box()
            assert abs(panel["x"] - sidebar["x"]) < 2 and toolbar["x"] >= sidebar["x"] + sidebar["width"] - 1
            assert listing["y"] >= toolbar["y"] + toolbar["height"] - 1
            assert abs(panel["y"] - 64) < 2 and abs(panel["y"] + panel["height"] - 984) < 2
            # Snapshot has >1000 entries; at least one real API continuation must have been read.
            assert any("cursor=" in request["path"] for request in requests)
            page.get_by_role("button", name="2", exact=True).click()
            assert len(table.locator("tbody tr").all()) == 100
            page.get_by_label("筛选当前目录", exact=True).fill("file-1004")
            expect(table.get_by_role("button", name="file-1004.txt", exact=True)).to_be_visible()
            assert len(table.locator("tbody tr").all()) == 1
            page.get_by_label("筛选当前目录", exact=True).fill("not-found")
            expect(page.get_by_text("当前目录没有匹配的名称。", exact=True)).to_be_visible()
            page.get_by_label("筛选当前目录", exact=True).fill("")
            table.get_by_role("button", name="空目录", exact=True).click()
            expect(page.get_by_text("此目录在已提交快照中为空。", exact=True)).to_be_visible()
            page.get_by_role("button", name="返回上级", exact=True).click()
            table.get_by_role("button", name="文档", exact=True).click()
            table.get_by_role("button", name="子目录", exact=True).click()
            table.get_by_role("button", name=sample.name, exact=True).click()
            dialog = page.get_by_role("dialog", name="文件属性", exact=True)
            expect(dialog).to_contain_text(digest)
            expect(dialog).to_contain_text(f"{sample.stat().st_size} 字节")
            expect(dialog).to_contain_text("/文档/子目录/" + sample.name)
            page.keyboard.press("Escape"); expect(dialog).not_to_be_visible()
            expect(table.get_by_role("button", name=sample.name, exact=True)).to_be_focused()
            assert page.url.endswith(f"#/files/{doc_id}/{quote('文档', safe='')}/{quote('子目录', safe='')}")
            expect(page.get_by_role("textbox", name="目录地址", exact=True)).to_have_value("/文档/子目录")
            page.reload()
            expect(table.get_by_role("button", name=sample.name, exact=True)).to_be_visible()
            expect(page.locator('[role="treeitem"][aria-selected="true"]')).to_have_count(1)
            snapshot_count = sum(request["method"] == "POST" and request["path"].endswith("/snapshots") for request in requests)
            page.get_by_role("button", name="返回上级", exact=True).click()
            expect(page.get_by_role("textbox", name="目录地址", exact=True)).to_have_value("/文档")
            page.get_by_role("button", name="后退", exact=True).click()
            expect(table.get_by_role("button", name=sample.name, exact=True)).to_be_visible()
            page.get_by_role("button", name="前进", exact=True).click()
            expect(table.get_by_role("button", name="子目录", exact=True)).to_be_visible()
            page.go_back()
            expect(table.get_by_role("button", name=sample.name, exact=True)).to_be_visible()
            page.go_forward()
            expect(table.get_by_role("button", name="子目录", exact=True)).to_be_visible()
            assert sum(request["method"] == "POST" and request["path"].endswith("/snapshots") for request in requests) == snapshot_count
            tree = page.get_by_role("tree", name="目录树", exact=True)
            tree.get_by_text("子目录", exact=True).click()
            expect(table.get_by_role("button", name=sample.name, exact=True)).to_be_visible()
            selected_tree = page.locator('[role="treeitem"][aria-selected="true"]')
            selected_tree.focus()
            page.keyboard.press("ArrowLeft")
            page.keyboard.press("Enter")
            expect(page.get_by_role("textbox", name="目录地址", exact=True)).to_have_value("/文档")
            expect(table.get_by_role("button", name="子目录", exact=True)).to_be_visible()
            page.get_by_role("button", name="当前目录属性", exact=True).click()
            expect(dialog).to_contain_text("直接子项")
            expect(dialog).to_contain_text("/文档")
            page.get_by_role("button", name="关闭", exact=True).click()
            page.get_by_role("button", name="返回上级", exact=True).click()
            address_input = page.get_by_role("textbox", name="目录地址", exact=True)
            address_input.fill("/URL # % & 空 格/子 + %2F")
            address_input.press("Enter")
            expect(table.get_by_role("button", name="deep.txt", exact=True)).to_be_visible()
            deep_url = page.url
            assert deep_url.endswith(f"#/files/{doc_id}/" + "/".join(quote(part, safe="") for part in special.relative_to(documents).parts))
            page.reload()
            expect(table.get_by_role("button", name="deep.txt", exact=True)).to_be_visible()
            address_input.fill("../")
            address_input.press("Enter")
            expect(page.get_by_role("alert")).to_contain_text("请输入 Root 内的相对路径")
            address_input.press("Escape")
            expect(address_input).to_have_value("/URL # % & 空 格/子 + %2F")
            expect(page.get_by_role("alert")).not_to_be_visible()
            address_input.fill("/missing")
            page.get_by_role("button", name="转到目录", exact=True).click()
            expect(page.get_by_role("alert")).to_contain_text("目录不存在")
            expect(table).not_to_be_visible()
            page.get_by_role("button", name="返回上级", exact=True).click()
            expect(table).to_be_visible()
            expect(page.get_by_role("button", name="前进", exact=True)).to_be_disabled()
            for bad, message in [(f"{doc_id}/%2F", "目录 URL 无效"), (f"{doc_id}/%zz", "目录 URL 无效"), ("0" * 32, "Root 不存在或未获授权")]:
                page.goto(origin + "/#/files/" + bad)
                expect(page.get_by_role("alert")).to_contain_text(message)
                expect(table).not_to_be_visible()
            page.goto(deep_url)
            expect(table.get_by_role("button", name="deep.txt", exact=True)).to_be_visible()
            address_input.fill("/")
            address_input.press("Enter")
            expect(table).to_be_visible()
            page.get_by_label("筛选当前目录", exact=True).fill("long-")
            table.get_by_role("button", name=long_name, exact=True).click()
            expect(dialog).to_contain_text("0 字节")
            layout_cases = []
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 390, 768, 1440, 1920):
                    page.set_viewport_size({"width": width, "height": 900})
                    page.wait_for_timeout(250)  # AppShell's breakpoint padding transition is 200 ms.
                    page.screenshot(path=str(RUN / f"properties-{scheme}-{width}.png"), full_page=True)
                    if not page.evaluate("document.documentElement.scrollWidth <= innerWidth"):
                        bounds = page.evaluate("[...document.querySelectorAll('body *')].map(e=>({tag:e.tagName,class:e.className,left:e.getBoundingClientRect().left,right:e.getBoundingClientRect().right,width:e.getBoundingClientRect().width})).filter(e=>e.width>innerWidth || e.right>innerWidth+1)")
                        (RUN / "overflow.json").write_text(json.dumps(bounds, ensure_ascii=False, indent=2))
                        raise AssertionError((scheme, width, "dialog", bounds[:15]))
                    layout_cases.append([scheme, width])
            page.get_by_role("button", name="关闭", exact=True).click()
            page.set_viewport_size({"width": 1440, "height": 1000})
            page.get_by_label("筛选当前目录", exact=True).fill("")
            choose("archive", arc_id)
            expect(table.get_by_role("button", name="archive-only.txt", exact=True)).to_be_visible()
            expect(table.get_by_role("button", name="文档", exact=True)).not_to_be_visible()
            expect(page.get_by_role("button", name="返回上级", exact=True)).to_be_disabled()
            page.get_by_role("button", name="后退", exact=True).click()
            expect(table.get_by_role("button", name="文档", exact=True)).to_be_visible()
            page.get_by_role("button", name="前进", exact=True).click()
            expect(table.get_by_role("button", name="archive-only.txt", exact=True)).to_be_visible()
            choose("documents", doc_id)
            expect(table.get_by_role("button", name="文档", exact=True)).to_be_visible()
            faults.update(mode="delay", root=doc_id)
            page.get_by_role("button", name="刷新文件列表", exact=True).click()
            for _ in range(100):
                if delayed.is_set(): break
                page.wait_for_timeout(50)
            assert delayed.is_set()
            choose("archive", arc_id)
            expect(table.get_by_role("button", name="archive-only.txt", exact=True)).to_be_visible()
            faults["mode"] = None; release.set()
            page.wait_for_timeout(500)
            expect(table.get_by_role("button", name="archive-only.txt", exact=True)).to_be_visible()
            expect(table.get_by_role("button", name="文档", exact=True)).not_to_be_visible()
            context.set_offline(True)
            page.get_by_role("button", name="刷新文件列表", exact=True).click()
            expect(page.get_by_role("alert")).to_contain_text("未能读取完整目录")
            expect(table).not_to_be_visible()
            expect(page.get_by_text("此目录在已提交快照中为空。", exact=True)).not_to_be_visible()
            context.set_offline(False)
            faults["mode"] = "expired"
            with page.expect_response(lambda response: response.status == 410 and "/snapshots/" in response.url):
                page.get_by_role("button", name="刷新文件列表", exact=True).click()
            expect(page.get_by_role("alert")).to_contain_text("未能读取完整目录")
            assert any(request["status"] == 410 for request in requests)
            faults["mode"] = None
            page.get_by_role("button", name="刷新文件列表", exact=True).click()
            expect(table.get_by_role("button", name="archive-only.txt", exact=True)).to_be_visible()
            (archive / "new.txt").write_bytes(b"new observation\n")
            for _ in range(60):
                page.get_by_role("button", name="刷新文件列表", exact=True).click()
                expect(table).to_be_visible()
                if table.get_by_role("button", name="new.txt", exact=True).is_visible(): break
                page.wait_for_timeout(500)
            else: raise AssertionError("refresh did not see new committed file")
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 390, 768, 1440, 1920):
                    page.set_viewport_size({"width": width, "height": 900})
                    page.wait_for_timeout(250)
                    page.get_by_role("combobox", name="浏览 Root", exact=True).click()
                    expect(page.get_by_role("option", name=f"documents · {doc_id[:8]}", exact=True)).to_be_visible()
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width, "select")
                    page.keyboard.press("Escape")
                    page.screenshot(path=str(RUN / f"browser-{scheme}-{width}.png"), full_page=True)
            assert not page.evaluate("window.__csp"), page.evaluate("window.__csp")
            assert not errors, errors
            assert not any("/content" in request["path"] or "/operations" in request["path"] for request in requests[browsing_start:])
            assert hashlib.sha256(sample.read_bytes()).hexdigest() == digest
            # Revocation is performed via the real UI; the browser route must discard its data.
            page.goto(origin + "/#/account")
            page.get_by_role("button", name="退出当前会话", exact=True).click()
            expect(page.get_by_text("未登录", exact=True)).to_be_visible()
            page.goto(origin + "/#/files")
            expect(page.get_by_role("heading", name="请先登录", exact=True)).to_be_visible()
            expect(table).not_to_be_visible()
            context.close(); browser.close()
            (RUN / "result.json").write_text(json.dumps({"passed": True, "checks": ["auth/empty-roots", "real-observer", "snapshot-multipage", "directory-pagination/filter", "nested/empty/parent/tree/keyboard", "URL-deep-link/reload/special-names", "address/missing/invalid/unauthorized", "toolbar/native-history/branch/root-history", "split-layout/no-redundant-copy", "properties/hash/size/path", "root-switch", "late-response", "offline-is-not-empty", "real-410", "refresh-observed-change", "light/dark-responsive-select/modal", "CSP", "no-content-or-operation", "source-hash", "logout"], "layout_cases": layout_cases, "requests": requests}, ensure_ascii=False, indent=2))
        print(f"S124 browser acceptance passed: {RUN}", flush=True)
    finally:
        release.set()
        if proxy: proxy.shutdown(); proxy.server_close()
        if service.poll() is None:
            service.terminate()
            try: service.wait(timeout=15)
            except subprocess.TimeoutExpired: service.kill(); service.wait()
        log.close()


if __name__ == "__main__": main()
