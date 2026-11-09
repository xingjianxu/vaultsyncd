#!/usr/bin/env python3
"""S150: built Web + Chromium + real isolated btrfs service.

Two immutable versions come from real external writes and the observer. A test
proxy shortens the first real history page to one item to exercise pagination;
only empty/error/delay cases use controlled responses. No online SQL writes.
"""
import functools
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import secrets
import subprocess
import threading
import time

from playwright.sync_api import expect, sync_playwright
from csp_support import CspStaticFiles, field_label

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug"))
RUN = Path(os.environ.get("VAULTSYNCD_WEB_RUN_ROOT", "/data/vaultsyncd-dev/vaultsyncd-temp")) / f"S150-history-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


def checked(args, **kwargs):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, **kwargs)
    assert result.returncode == 0, f"{args[0]} failed: {result.stderr}"
    return result.stdout


def main():
    assert RUN.parent.is_dir() and os.access(RUN.parent, os.W_OK)
    RUN.mkdir(mode=0o700)
    evidence = RUN / "evidence"; evidence.mkdir()
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    print(f"S150 artifacts: {RUN}", flush=True)
    state, vaults = RUN / "state", RUN / "vaults"
    state.mkdir(mode=0o700); vaults.mkdir()
    documents, archive = vaults / "documents", vaults / "archive"
    documents.mkdir(); archive.mkdir()
    sample = documents / "report & '文档'.txt"
    sample.write_bytes(b"S150 original committed content\n")
    (documents / "child").mkdir(); (documents / "child" / "leaf.txt").write_bytes(b"leaf\n")
    (archive / "other.txt").write_bytes(b"other vault\n")
    old_hash = hashlib.sha256(sample.read_bytes()).hexdigest()
    password = secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", state, "--login", "owner"], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[vaults]\nallowed_bases=["{vaults}"]\n[log]\nformat="json"\nlevel="info"\n')
    log = (evidence / "server.log").open("w")
    service = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log)
    proxy = None
    faults = {"mode": None}
    delayed, release = threading.Event(), threading.Event()
    requests = []
    try:
        address = None
        for _ in range(150):
            for line in (evidence / "server.log").read_text().splitlines():
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
                backend = http.client.HTTPConnection(address, timeout=20)
                headers = {key: value for key, value in self.headers.items() if key.lower() not in ("connection", "transfer-encoding")}
                body = self.rfile.read(int(self.headers.get("content-length", "0")))
                mode = faults["mode"]
                history = "/history" in self.path
                try:
                    backend.request(self.command, self.path, body=body, headers=headers)
                    response = backend.getresponse(); payload = response.read(); status = response.status
                    if history and status == 200:
                        if mode == "page" and "after=" not in self.path:
                            data = json.loads(payload)
                            assert len(data["items"]) == 2
                            payload = json.dumps({"items": data["items"][:1], "next_cursor": data["items"][0]["id"]}).encode()
                        elif mode == "empty": payload = b'{"items":[]}'
                        elif mode in ("403", "404", "500"):
                            status = int(mode)
                            payload = json.dumps({"code": "forbidden" if mode == "403" else "not_found" if mode == "404" else "internal", "message": f"history test {mode}", "retryable": False, "request_id": "S150"}).encode()
                        elif mode == "delay": delayed.set(); release.wait(timeout=20)
                    requests.append({"method": self.command, "path": self.path, "status": status, "mode": mode})
                    self.send_response(status)
                    for key, value in response.getheaders():
                        if key.lower() not in ("connection", "transfer-encoding", "content-length"): self.send_header(key, value)
                    self.send_header("content-length", str(len(payload))); self.end_headers(); self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # Closing the dialog intentionally aborts the browser request.
                finally: backend.close()

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=str(REPO / "apps/web/dist")))
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        origin = f"http://localhost:{proxy.server_port}"
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            context = browser.new_context(viewport={"width": 1440, "height": 900})
            page = context.new_page(); errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.add_init_script("window.__csp=[];document.addEventListener('securitypolicyviolation',e=>window.__csp.push(e.effectiveDirective));")
            page.goto(origin + "/#/files")
            page.get_by_label(field_label("登录名")).fill("owner")
            page.get_by_label(field_label("密码")).fill(password)
            page.get_by_role("button", name="登录", exact=True).click()
            expect(page.get_by_text("尚无授权 Vault，请先在 Vault 目录页面注册目录。", exact=True)).to_be_visible()

            def api(path, body=None):
                result = page.evaluate("""async ([path, body]) => {const r=await fetch(path,body===null?{}:{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});return {status:r.status,body:await r.json()}}""", [path, body])
                assert result["status"] in (200, 201), result
                return result["body"]

            vault = api("/v1/admin/vaults", {"path": str(documents)})["vault_id"]
            other = api("/v1/admin/vaults", {"path": str(archive)})["vault_id"]

            def wait_for(read):
                for _ in range(180):
                    value = read()
                    if value: return value
                    page.wait_for_timeout(250)
                raise AssertionError("real observer did not produce expected metadata")

            def entries(): return api(f"/v1/vaults/{vault}/recovery")["items"]
            entry = wait_for(lambda: next((item for item in entries() if item["name"] == sample.name), None))
            history_url = f"/v1/vaults/{vault}/entries/{entry['entry_id']}/history"
            assert api(history_url)["items"][0]["hash"] == old_hash
            sample.write_bytes(b"S150 second committed version with changed bytes\n")
            new_hash = hashlib.sha256(sample.read_bytes()).hexdigest()
            versions = wait_for(lambda: (data if len(data := api(history_url)["items"]) == 2 else None))
            assert {item["hash"] for item in versions} == {old_hash, new_hash}
            (evidence / "real-history.json").write_text(json.dumps(versions, indent=2))
            page.goto(origin + f"/#/files/{vault}")
            page.reload()  # Refresh the Vault list mounted before registration.
            table = page.get_by_role("table", name="文件与目录", exact=True)
            link = table.get_by_role("button", name=sample.name, exact=True)
            expect(link).to_be_visible()
            dialog = page.get_by_role("dialog", name="版本历史", exact=True)

            def open_history(keyboard=False):
                if keyboard:
                    link.focus(); page.keyboard.press("Shift+F10")
                else: link.click(button="right")
                page.get_by_role("menuitem", name="版本历史", exact=True).click()
                expect(dialog).to_be_visible()

            def close_history():
                dialog.get_by_role("button", name="关闭", exact=True).click()
                expect(dialog).not_to_be_visible()

            baseline = page.url
            open_history()
            history_table = dialog.get_by_role("table", name="文件版本历史")
            expect(history_table.locator("tbody tr")).to_have_count(2)
            for item in versions:
                expect(dialog.get_by_text(item["version_id"], exact=True)).to_be_visible()
                expect(dialog.get_by_text(item["hash"], exact=True)).to_be_visible()
                row = history_table.get_by_role("row").filter(has_text=item["version_id"])
                expect(row.get_by_text(f"{item['size']} B", exact=True)).to_be_visible()
            expect(dialog.get_by_text("当前版本", exact=True)).to_be_visible()
            expect(dialog.get_by_text(sample.name, exact=True)).to_be_visible()
            expect(dialog.locator("img")).to_have_count(0)
            assert page.url == baseline
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({"width": width, "height": 900})
                    page.screenshot(path=str(evidence / f"history-{scheme}-{width}.png"))
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), page.evaluate("[...document.querySelectorAll('body *')].filter(e=>e.getBoundingClientRect().right>innerWidth+1).map(e=>[e.tagName,e.className,e.getBoundingClientRect().width]).slice(-25)")
                    box = dialog.bounding_box()
                    assert box["x"] >= 0 and box["x"] + box["width"] <= width + 1, box
                    page.screenshot(path=str(evidence / f"history-{scheme}-{width}.png"))
            page.set_viewport_size({"width": 1440, "height": 900})
            close_history(); open_history(keyboard=True)
            expect(history_table.locator("tbody tr")).to_have_count(2)
            page.keyboard.press("Escape"); expect(dialog).not_to_be_visible()

            # Only page size is reduced; the next cursor goes to the real server.
            faults["mode"] = "page"; open_history()
            expect(history_table.locator("tbody tr")).to_have_count(1)
            expect(dialog.get_by_text(versions[0]["version_id"], exact=True)).to_be_visible()
            dialog.get_by_role("button", name="下一页", exact=True).click()
            expect(dialog.get_by_text(versions[1]["version_id"], exact=True)).to_be_visible()
            expect(dialog.get_by_role("button", name="下一页", exact=True)).to_be_disabled()
            dialog.get_by_role("button", name="上一页", exact=True).click()
            expect(dialog.get_by_text(versions[0]["version_id"], exact=True)).to_be_visible()
            faults["mode"] = None
            dialog.get_by_role("button", name="刷新", exact=True).click()
            expect(history_table.locator("tbody tr")).to_have_count(2)
            expect(dialog.get_by_role("button", name="上一页", exact=True)).to_be_disabled()
            close_history()

            faults["mode"] = "empty"; open_history()
            expect(dialog.get_by_text("暂无版本记录。", exact=True)).to_be_visible()
            close_history()
            for mode in ("403", "404", "500"):
                faults["mode"] = mode; open_history()
                expect(dialog.get_by_role("alert")).to_be_visible()
                expect(dialog.get_by_text("暂无版本记录。", exact=True)).to_have_count(0)
                faults["mode"] = None
                dialog.get_by_role("button", name="重试", exact=True).click()
                expect(history_table.locator("tbody tr")).to_have_count(2)
                close_history()

            faults["mode"] = "delay"; open_history()
            wait_for(delayed.is_set)
            expect(dialog.get_by_role("status")).to_be_visible()
            expect(dialog.get_by_role("button", name="刷新", exact=True)).to_be_disabled()
            close_history()
            faults["mode"] = None
            # Late first response must never enter this different file's dialog.
            child_location = origin + f"/#/files/{vault}/child"
            page.goto(child_location)
            leaf = table.get_by_role("button", name="leaf.txt", exact=True)
            leaf.click(button="right"); page.get_by_role("menuitem", name="版本历史", exact=True).click()
            expect(dialog.get_by_text("leaf.txt", exact=True)).to_be_visible()
            expect(history_table.locator("tbody tr")).to_have_count(1)
            release.set(); page.wait_for_timeout(250)
            expect(dialog.get_by_text(sample.name, exact=True)).to_have_count(0)
            expect(history_table.locator("tbody tr")).to_have_count(1)
            # Route changes close the dialog without a manual close action.
            page.evaluate("path => { location.hash = path; }", f"/files/{other}")
            expect(dialog).not_to_be_visible()
            expect(table.get_by_role("button", name="other.txt", exact=True)).to_be_visible()
            page.goto(origin + f"/#/files/{vault}")
            table.get_by_role("button", name="child", exact=True).click(button="right")
            expect(page.get_by_role("menuitem", name="版本历史", exact=True)).to_have_count(0)
            page.get_by_role("textbox", name="目录地址", exact=True).click()
            expect(page.get_by_role("menu")).not_to_be_visible()
            open_history(); expect(history_table.locator("tbody tr")).to_have_count(2)
            page.evaluate("path => { location.hash = path; }", f"/files/{vault}/child")
            expect(dialog).not_to_be_visible()
            expect(table.get_by_role("button", name="leaf.txt", exact=True)).to_be_visible()

            assert errors == [], errors
            assert page.evaluate("window.__csp") == [], page.evaluate("window.__csp")
            assert not [request for request in requests if request["method"] != "GET" and "/operations" in request["path"]]
            context.close(); browser.close()
            (evidence / "requests.json").write_text(json.dumps(requests, indent=2))
            (evidence / "result.json").write_text(json.dumps({"passed": True, "binary": str(BIN), "checks": ["real-two-versions", "immutable-hashes", "in-place-modal", "special-name-escaped", "keyboard-menu-escape", "real-after-with-one-item-test-page", "previous-next-refresh", "controlled-empty", "controlled-403-404-500-retry", "close-aborts-and-late-response-isolated", "vault-and-directory-navigation-close", "directories-have-no-history", "light-dark-320-1440", "no-history-writes", "CSP"]}, ensure_ascii=False, indent=2))
        print(f"S150 history browser acceptance passed: {RUN}", flush=True)
    finally:
        release.set()
        if proxy: proxy.shutdown(); proxy.server_close()
        if service.poll() is None:
            service.terminate()
            try: service.wait(timeout=15)
            except subprocess.TimeoutExpired: service.kill(); service.wait()
        log.close()


if __name__ == "__main__": main()
