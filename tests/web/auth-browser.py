#!/usr/bin/env python3
"""S077: real Chromium + React + isolated btrfs server; HTTP loopback test only.

Run after the Web build and Rust server/admin build:
nix develop .#web-tests --command python3 tests/web/auth-browser.py
No online SQL, browser mocks, global dependency installation or Windows VM changes.
"""
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
RUN = Path("/data") / f"vs-s077-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


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
    password, new_password = secrets.token_urlsafe(24), secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", state], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[roots]\nallowed_bases=["{roots}"]\n[log]\nformat="json"\nlevel="info"\n')
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
            cookies = context.cookies()
            assert len(cookies) == 2 and all(c["httpOnly"] and c["secure"] and c["sameSite"] == "Strict" for c in cookies)
            assert page.evaluate("document.cookie") == ""
            assert page.evaluate("localStorage.length") == 0
            protected = page.evaluate("fetch('/v1/auth/account').then(async r => ({status:r.status, body:await r.text(), cache:r.headers.get('cache-control')}))")
            assert protected["status"] == 200 and "token" not in protected["body"] and protected["cache"] == "no-store"
            # Second browser/agent session must be revoked by account password changes too.
            second = browser.new_context()
            result = second.request.post(origin + "/v1/auth/browser/login", data={"login":"owner", "password":password, "device_label":"second browser"}, headers={"origin":origin})
            assert result.status == 200 and "token" not in result.text()
            old_second_cookie = second.cookies()

            # Expire only access cookie: reload must refresh using HttpOnly cookie, then restore account.
            access = next(c for c in cookies if c["name"] == "vaultsyncd_session")
            context.add_cookies([{**access, "expires": time.time() - 10}])
            page.reload(); expect(page.get_by_text("已登录：owner", exact=True)).to_be_visible()
            assert next(c for c in context.cookies() if c["name"] == "vaultsyncd_session")["value"] != access["value"]

            # Same-site different origin can send Strict cookies; server still rejects CSRF.
            other = context.new_page(); other.goto(f"http://localhost:{evil.server_port}/")
            with other.expect_event("requestfailed", predicate=lambda request: request.url == origin + "/v1/auth/revoke-all"):
                other.evaluate("url => { fetch(url, {method:'POST', credentials:'include'}).catch(() => {}); }", origin + "/v1/auth/revoke-all")
            assert ("/v1/auth/revoke-all", 403, f"http://localhost:{evil.server_port}") in proxy_results
            other.close()

            page.get_by_label("当前密码", exact=True).fill(password)
            page.get_by_label("新密码", exact=True).fill(new_password)
            page.get_by_role("button", name="修改密码并撤销会话", exact=True).click()
            expect(page.get_by_text("未登录", exact=True)).to_be_visible()
            assert second.request.get(origin + "/v1/auth/account", headers={"origin":origin}).status == 401
            assert second.request.post(origin + "/v1/auth/browser/refresh", headers={"origin":origin}).status == 401
            page.get_by_label("登录名", exact=True).fill("owner")
            page.get_by_label("密码", exact=True).fill(password)
            page.get_by_role("button", name="登录", exact=True).click()
            expect(page.get_by_role("alert")).to_contain_text("密码不正确")
            login(new_password)
            page.get_by_role("button", name="退出当前会话", exact=True).click()
            expect(page.get_by_text("未登录", exact=True)).to_be_visible()
            assert page.evaluate("fetch('/v1/auth/account').then(r => r.status)") == 401
            assert page.evaluate("fetch('/v1/auth/browser/refresh',{method:'POST'}).then(r => r.status)") == 401
            login(new_password)
            page.get_by_role("button", name="撤销全部会话", exact=True).click()
            expect(page.get_by_text("未登录", exact=True)).to_be_visible()
            assert page.evaluate("fetch('/v1/auth/browser/refresh',{method:'POST'}).then(r => r.status)") == 401
            assert page.evaluate("localStorage.length") == 0
            combined = "\n".join(console) + (RUN / "server.log").read_text()
            assert password not in combined and new_password not in combined
            assert all(cookie["value"] not in combined for cookie in cookies + old_second_cookie)
            page.screenshot(path=str(RUN / "account.png"))
            second.close(); context.close(); browser.close()
            (RUN / "result.json").write_text(json.dumps({"passed":True, "browser":playwright.chromium.name, "checks":["login", "httponly-secure-strict", "no-token-response", "no-store", "refresh-expired-access", "csrf-cross-origin", "password-revokes-all", "old-password-rejected", "logout", "revoke-all", "no-secret-logs-storage"], "scope":"HTTP loopback test; production HTTPS is S097"}, ensure_ascii=False, indent=2))
        print(f"S077 browser acceptance passed: {RUN}")
    finally:
        for listener in [proxy, evil]:
            if listener: listener.shutdown(); listener.server_close()
        if server.poll() is None:
            server.terminate()
            try: server.wait(timeout=15)
            except subprocess.TimeoutExpired: server.kill(); server.wait()
        log.close()


if __name__ == "__main__": main()
