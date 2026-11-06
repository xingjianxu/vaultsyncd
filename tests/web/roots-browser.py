#!/usr/bin/env python3
"""S078: real Chromium + React + isolated btrfs server; HTTP loopback test only.

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

from playwright.sync_api import sync_playwright, expect

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug"))
RUN = Path("/data") / f"vs-s078-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


def checked(args, **kwargs):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, **kwargs)
    if result.returncode:
        raise AssertionError(f"command failed: {args[0]} ({result.returncode}): {result.stderr}")
    return result.stdout


def main():
    checked(["sudo", "-n", "mkdir", "-m", "755", RUN])
    checked(["sudo", "-n", "chown", f"{os.getuid()}:{os.getgid()}", RUN])
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    state, roots = RUN / "state", RUN / "roots"
    state.mkdir(mode=0o700); roots.mkdir()
    candidate = roots / "documents"; candidate.mkdir(); (candidate / "child").mkdir()
    sample = candidate / "keep.txt"; sample.write_bytes(b"S078 confirmed content is preserved\n")
    original_hash = hashlib.sha256(sample.read_bytes()).hexdigest()
    unsupported = Path(tempfile.mkdtemp(prefix="vs-s078-unsupported-"))
    alternate = RUN / "alternate"; alternate.mkdir()
    mounted = False
    password, new_password = secrets.token_urlsafe(24), secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", state], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[roots]\nallowed_bases=["{roots}","{unsupported}"]\n[log]\nformat="json"\nlevel="info"\n')
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

        class Handler(http.server.SimpleHTTPRequestHandler):
            def log_message(self, *_): pass

            def do_POST(self): self.forward()

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
            page.goto(origin + "/#/account")
            expect(page.get_by_text("未登录", exact=True)).to_be_visible()

            def login(secret):
                page.get_by_label("登录名", exact=True).fill("owner")
                page.get_by_label("密码", exact=True).fill(secret)
                page.get_by_role("button", name="登录", exact=True).click()
                expect(page.get_by_text("已登录：owner", exact=True)).to_be_visible()

            login(password)
            page.get_by_role("link", name="Root 目录", exact=True).click()
            expect(page.get_by_text(str(roots), exact=True)).to_be_visible()
            expect(page.get_by_text("尚未注册 Root；这不表示服务器目录为空或客户端已同步。", exact=True)).to_be_visible()
            def register(path):
                page.get_by_label("服务器目录绝对路径", exact=True).fill(str(path))
                page.get_by_role("button", name="校验并注册 Root", exact=True).click()
            register(candidate)
            expect(page.get_by_text("已注册 Root", exact=False).filter(has_text="等待初始采集")).to_be_visible()
            expect(page.get_by_text(str(candidate), exact=True)).to_be_visible()
            assert hashlib.sha256(sample.read_bytes()).hexdigest() == original_hash
            ordinary = page.evaluate("fetch('/v1/roots').then(r => r.json())")
            assert str(candidate) not in json.dumps(ordinary)
            managed = page.evaluate("fetch('/v1/admin/roots').then(r => r.json())")
            assert len(managed["roots"]) == 1 and managed["roots"][0]["health"] == "healthy"
            assert managed["roots"][0]["collection"]["state"] in ["not_scanned", "scanning", "pending", "complete"]
            register(candidate / "child")
            expect(page.get_by_role("alert")).to_contain_text("重叠")
            register(unsupported)
            expect(page.get_by_role("alert")).to_contain_text("btrfs")
            register("/etc")
            expect(page.get_by_role("alert")).to_contain_text("允许基目录")
            for _ in range(30):
                page.get_by_role("button", name="刷新目录状态", exact=True).click()
                try:
                    expect(page.get_by_text("本进程初始采集完成", exact=True)).to_be_visible(timeout=500)
                    break
                except AssertionError: time.sleep(.2)
            else: raise AssertionError("observer never provided completed initial import evidence")
            managed = page.evaluate("fetch('/v1/admin/roots').then(r => r.json())")
            assert int(managed["roots"][0]["root_seq"]) > 0
            # Actual mount replacement, scoped only to this run; API must detect mount drift.
            checked(["sudo", "-n", "mount", "--bind", alternate, candidate]); mounted = True
            page.get_by_role("button", name="刷新目录状态", exact=True).click()
            expect(page.get_by_text("Root 不可用", exact=True)).to_be_visible()
            expect(page.get_by_text("恢复原挂载", exact=False)).to_be_visible()
            unavailable = page.evaluate("fetch('/v1/admin/roots').then(r => r.json())")["roots"][0]
            assert unavailable["health"] == "unavailable" and unavailable["collection"]["state"] == "blocked"
            assert unavailable["reason"]
            checked(["sudo", "-n", "umount", candidate]); mounted = False
            page.get_by_role("button", name="刷新目录状态", exact=True).click()
            expect(page.get_by_text("挂载核对通过", exact=True)).to_be_visible()
            assert hashlib.sha256(sample.read_bytes()).hexdigest() == original_hash
            page.screenshot(path=str(RUN / "roots.png"), full_page=True)
            assert page.evaluate("localStorage.length") == 0
            context.close(); browser.close()
            (RUN / "result.json").write_text(json.dumps({"passed":True,"checks":["owner-login", "allowed-bases", "empty-is-not-synced", "register-btrfs", "existing-hash-preserved", "ordinary-list-no-path", "overlap-rejected", "unsupported-filesystem-rejected", "outside-bases-rejected", "actual-observer-progress", "real-bind-mount-drift", "restore-mount-recovers-health", "source-hash-retained"], "mount_failure":unavailable, "hash":original_hash, "scope":"HTTP loopback browser test; HTTPS deployment S097"}, ensure_ascii=False, indent=2))
        print(f"S078 browser acceptance passed: {RUN}")
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
