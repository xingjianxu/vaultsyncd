#!/usr/bin/env python3
"""S079: real Chromium + React + isolated btrfs server; HTTP loopback test only.

Run after the Web build and Rust server/admin build:
nix develop .#web-tests --command python3 tests/web/auth-browser.py
No online SQL, browser mocks, global dependency installation or Windows VM changes.
"""
import hashlib
import tempfile
import functools
import http.client
import http.server
import json
import os
from pathlib import Path
import secrets
import subprocess
import threading
import time

from csp_support import CspStaticFiles, field_label
from playwright.sync_api import sync_playwright, expect

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug"))
RUN = Path(os.environ.get("VAULTSYNCD_WEB_RUN_ROOT", "/data/vaultsyncd-dev/vaultsyncd-temp")) / f"vs-s079-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


def checked(args, **kwargs):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, **kwargs)
    if result.returncode:
        raise AssertionError(f"command failed: {args[0]} ({result.returncode}): {result.stderr}")
    return result.stdout


def main():
    checked(["sudo", "-n", "mkdir", "-m", "755", RUN])
    checked(["sudo", "-n", "chown", f"{os.getuid()}:{os.getgid()}", RUN])
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    state, vaults = RUN / "state", RUN / "vaults"
    state.mkdir(mode=0o700); vaults.mkdir()
    candidate = vaults / "documents"; candidate.mkdir(); (candidate / "child").mkdir()
    sample = candidate / "keep.txt"; sample.write_bytes(b"S079 confirmed content is preserved\n")
    original_hash = hashlib.sha256(sample.read_bytes()).hexdigest()
    unsupported = Path(tempfile.mkdtemp(prefix="vs-s079-unsupported-", dir=os.environ.get("VAULTSYNCD_WEB_UNSUPPORTED_ROOT")))
    alternate = RUN / "alternate"; alternate.mkdir()
    mounted = False
    password, new_password = secrets.token_urlsafe(24), secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", state, "--login", "owner"], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[vaults]\nallowed_bases=["{vaults}","{unsupported}"]\n[log]\nformat="json"\nlevel="info"\n')
    log = (RUN / "server.log").open("w")
    server = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log)
    proxy = evil = None
    try:
        address = None
        for _ in range(150):
            for line in (RUN / "server.log").read_text().splitlines():
                fields = json.loads(line).get("fields", {})
                if fields.get("message") == "服务端开始监听": address = fields["listen"]
            if address: break
            assert server.poll() is None, "server exited"
            time.sleep(.2)
        assert address, "server did not listen"
        proxy_results = []
        drop_restore = [False]

        class Handler(CspStaticFiles):
            def log_message(self, *_): pass

            def do_POST(self): self.forward()

            def do_PUT(self): self.forward()

            def do_GET(self):
                if self.path.startswith("/v1/"): self.forward()
                else: super().do_GET()

            def forward(self):
                backend = http.client.HTTPConnection(address, timeout=10)
                headers = {key: value for key, value in self.headers.items() if key.lower() not in ("connection", "transfer-encoding")}
                body = self.rfile.read(int(self.headers.get("content-length", "0")))
                try:
                    backend.request(self.command, self.path, body=body, headers=headers)
                    response = backend.getresponse(); payload = response.read()
                    proxy_results.append((self.path, response.status, self.headers.get("origin")))
                    self.send_response(response.status)
                    for key, value in response.getheaders():
                        if key.lower() not in ("connection", "transfer-encoding", "content-length"):
                            self.send_header(key, value)
                    self.send_header("content-length", str(len(payload))); self.end_headers()
                    if self.path.endswith("/operations") and self.command == "POST" and drop_restore[0]:
                        drop_restore[0] = False
                        self.close_connection = True
                        return
                    self.wfile.write(payload)
                finally: backend.close()

        factory = functools.partial(Handler, directory=str(REPO / "apps/web/dist"))
        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), factory)
        evil = http.server.ThreadingHTTPServer(("127.0.0.1", 0), factory)
        for listener in [proxy, evil]: threading.Thread(target=listener.serve_forever, daemon=True).start()
        origin = f"http://localhost:{proxy.server_port}"
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            context = browser.new_context()
            page = context.new_page(); console = []
            page.on("console", lambda message: console.append(message.text))
            page.add_init_script("window.__csp=[];document.addEventListener('securitypolicyviolation',e=>window.__csp.push(e.effectiveDirective));")
            page.goto(origin + "/#/recovery")
            notifications = page.locator(".mantine-Notification-root")
            operation = page.locator(".mantine-Paper-root").filter(has=page.get_by_role("heading", name="恢复操作", exact=True))
            expect(page.get_by_label(field_label("登录名"))).to_be_visible()
            expect(page.get_by_role("navigation", name="主要导航")).to_have_count(0)

            def login(secret):
                page.get_by_label(field_label("登录名")).fill("owner")
                page.get_by_label(field_label("密码")).fill(secret)
                page.get_by_role("button", name="登录", exact=True).click()
                expect(page.get_by_role("heading", name="历史与恢复", level=1, exact=True)).to_be_visible()
                assert page.url == origin + "/#/recovery"


            login(password)
            def api(path, body=None, method=None):
                return page.evaluate("""async ([path,body,method]) => {
                  const r=await fetch(path,{method:method||(body?'POST':'GET'),headers:body?{'content-type':'application/json'}:undefined,body:body?JSON.stringify(body):undefined});
                  const value=await r.json(); if(!r.ok) throw new Error(JSON.stringify(value)); return value;
                }""",[path,body,method])
            vault=api("/v1/admin/vaults",{"path":str(candidate)})["vault_id"]
            def entries(endpoint="recovery"):
                items=[]; after=""
                while True:
                    batch=api(f"/v1/vaults/{vault}/{endpoint}"+("?after="+after if after else ""))
                    items+=batch["items"]
                    after=batch.get("next_cursor")
                    if not after: return items
            def wait_for(predicate):
                for _ in range(150):
                    result=predicate()
                    if result: return result
                    time.sleep(.2)
                raise AssertionError("server facts did not converge")
            original=wait_for(lambda:next((x for x in entries() if x["name"]=="keep.txt"),None))
            old_hash=original["hash"]
            sample.write_bytes(b"S079 newer version")
            head=wait_for(lambda:next((x for x in entries() if x["entry_id"]==original["entry_id"] and x["hash"]!=old_hash),None))
            history=api(f"/v1/vaults/{vault}/entries/{head['entry_id']}/history")["items"]
            assert len(history)==2 and any(x.get("expires_at") for x in history)
            # Produce a durable conflict mapping through the production put endpoint.
            content=b"S079 late independent edit"
            digest=hashlib.sha256(content).hexdigest()
            upload=api(f"/v1/vaults/{vault}/uploads",{"total_size":str(len(content)),"hash":digest,"chunk_size":"0"})
            page.evaluate("""async ([url,content,hash]) => {
              const r=await fetch(url,{method:'PUT',headers:{'content-type':'application/octet-stream','x-part-sha256':hash},body:new Uint8Array(content)});
              if(!r.ok)throw new Error(await r.text());
            }""",[f"/v1/vaults/{vault}/uploads/{upload['upload_id']}/parts/0",list(content),digest])
            api(f"/v1/vaults/{vault}/uploads/{upload['upload_id']}/finalize",{})
            epoch=api(f"/v1/vaults/{vault}/snapshots",{})["server_epoch"]
            conflict=api(f"/v1/vaults/{vault}/operations",{"kind":"put_content","op_id":secrets.token_hex(16),"device_id":secrets.token_hex(16),"server_epoch":epoch,"entry_id":head["entry_id"],"base_revision":original["revision"],"upload_id":upload["upload_id"]})
            assert conflict["state"]=="committed" and conflict["conflict"]
            copies=entries("conflicts"); assert len(copies)==1 and copies[0]["conflict_source"]==head["entry_id"]
            # Login now opens this route before the fixtures are registered;
            # reload the preserved deep link to read the newly collected Vault.
            page.reload()
            expect(page.get_by_role("button",name="查看历史").first).to_be_visible()
            page.get_by_role("button",name="冲突副本",exact=True).click()
            expect(page.get_by_text("冲突源：",exact=False)).to_be_visible()
            page.get_by_role("button",name="关闭提示",exact=True).click()
            assert len(entries("conflicts"))==1
            # Real deletion, followed by a new entry at the same path.
            sample.unlink()
            deleted=wait_for(lambda:next((x for x in entries("trash") if x["entry_id"]==head["entry_id"]),None))
            sample.write_bytes(b"S079 new target must survive")
            target=wait_for(lambda:next((x for x in entries() if x["name"]=="keep.txt" and x["entry_id"]!=head["entry_id"]),None))
            page.get_by_role("button",name="回收站",exact=True).click()
            expect(page.get_by_role("button",name="恢复此项",exact=True)).to_be_visible()
            drop_restore[0]=True
            page.get_by_role("button",name="恢复此项",exact=True).click()
            expect(page.get_by_text("结果未知，尚未确认恢复",exact=True)).to_be_visible()
            stored=page.evaluate("Object.values(sessionStorage).map(JSON.parse)")
            op=stored[0]["request"]["op_id"]
            page.reload()
            expect(page.get_by_text("结果未知，尚未确认恢复",exact=True)).to_be_visible()
            page.get_by_role("button",name="核对并重试原操作",exact=True).click()
            expect(page.get_by_text("恢复已提交",exact=True)).to_be_visible()
            restored=[x for x in entries() if x["entry_id"]==head["entry_id"]][0]
            assert restored["name"]!="keep.txt" and restored["hash"]==head["hash"]
            expect(operation.get_by_text("实际恢复位置：",exact=False)).to_contain_text(restored["name"])
            expect(notifications.filter(has_text="恢复已提交")).to_contain_text(restored["name"])
            assert sample.read_bytes()==b"S079 new target must survive"
            assert (candidate/restored["name"]).read_bytes()==b"S079 newer version"
            seq=api(f"/v1/admin/vaults")["vaults"][0]["vault_seq"]
            page.get_by_role("button",name="核对并重试原操作",exact=True).click()
            expect(page.get_by_text("恢复已提交",exact=True)).to_be_visible()
            assert api(f"/v1/admin/vaults")["vaults"][0]["vault_seq"]==seq
            assert api(f"/v1/vaults/{vault}/operations/{op}")["state"]=="committed"

            page.get_by_role("button",name="文件与历史",exact=True).click()
            target_row=page.get_by_role("row").filter(has=page.get_by_text(restored["name"],exact=True))
            target_row.get_by_role("button",name="查看历史",exact=True).click()
            old_version=next(x for x in history if x["hash"]==old_hash)["version_id"]
            old_row=page.get_by_role("row").filter(has_text=old_version)
            old_row.get_by_role("button",name="恢复此项",exact=True).click()
            wait_for(lambda:(candidate/restored["name"]).read_bytes()==b"S079 confirmed content is preserved\n")
            expect(page.get_by_text("恢复已提交",exact=True)).to_be_visible()
            # Pagination uses real files, not fabricated wire responses.
            for index in range(103): (candidate/f"page-{index:03}.txt").write_text(str(index))
            wait_for(lambda:len(entries())>=106)
            page.get_by_role("button",name="文件与历史",exact=True).click()
            expect(page.get_by_role("button",name="下一页",exact=True)).to_be_visible()
            page.get_by_role("button",name="下一页",exact=True).click()
            expect(page.get_by_text(entries()[100]["name"],exact=True)).to_be_visible()
            expect(page.get_by_role("button",name="下一页",exact=True)).not_to_be_visible()
            page.evaluate("window.scrollTo(0,0)")
            page.screenshot(path=str(RUN/"recovery.png"),full_page=False)
            assert page.evaluate("localStorage.length")==0
            assert page.evaluate("window.__csp") == [], page.evaluate("window.__csp")
            context.close();browser.close()
            (RUN/"result.json").write_text(json.dumps({"passed":True,"restored_path":restored["name"],"checks":["history-restore-matches-disk","versions-expiry","durable-conflict-copy","dismiss-retains-copy","same-name-target-retained","response-lost-committed","reload-original-operation","retry-no-duplicate","actual-path-matches-disk","real-pagination","notifications/committed-restore-actual-path","CSP"]},ensure_ascii=False,indent=2))
        print(f"S079 browser acceptance passed: {RUN}")
    finally:
        if mounted: checked(["sudo", "-n", "umount", candidate])
        unsupported.rmdir()
        for listener in [proxy, evil]:
            if listener: listener.shutdown(); listener.server_close()
        if server.poll() is None:
            server.terminate()
            try: server.wait(timeout=15)
            except subprocess.TimeoutExpired: server.kill(); server.wait()
        log.close()


if __name__ == "__main__": main()
