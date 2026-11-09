#!/usr/bin/env python3
"""S077/S145/S148: real Chromium + isolated btrfs auth and health lifecycle.

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

from csp_support import CspStaticFiles, field_label
from playwright.sync_api import sync_playwright, expect

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug"))
RUN = Path(os.environ.get("VAULTSYNCD_WEB_RUN_ROOT", "/data/vaultsyncd-dev/vaultsyncd-temp")) / f"S148-auth-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


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
    password, new_password = secrets.token_urlsafe(24), secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", state, "--login", "owner"], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[vaults]\nallowed_bases=["{vaults}"]\n[log]\nformat="json"\nlevel="info"\n')
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
        account_gate = threading.Event()
        network = {"offline": False, "health_offline": False}
        health_requests = []
        health_delay = {"gate": None}

        class Handler(CspStaticFiles):
            def log_message(self, *_): pass

            def do_POST(self): self.forward()

            def do_GET(self):
                if self.path.startswith("/v1/"): self.forward()
                else: super().do_GET()

            def forward(self):
                if self.path == "/v1/health":
                    health_requests.append(self.path)
                    gate = health_delay["gate"]
                    if gate is not None:
                        gate.wait(timeout=10)
                    if network["health_offline"]:
                        self.close_connection = True
                        return
                if self.path == "/v1/auth/account":
                    account_gate.wait(timeout=10)
                if network["offline"]:
                    self.close_connection = True
                    return
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
                    try:
                        self.wfile.write(payload)
                    except (BrokenPipeError, ConnectionResetError):
                        if self.path != "/v1/health": raise
                finally: backend.close()

        factory = functools.partial(Handler, directory=str(REPO / "apps/web/dist"))
        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), factory)
        # The attacker's page controls its own policy; using our connect-src here
        # would block the request before it can exercise the server's CSRF gate.
        class Attacker(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_): pass
            def do_GET(self):
                body = b"<!doctype html><title>Cross-origin attack test</title>"
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
        evil = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Attacker)
        for listener in [proxy, evil]: threading.Thread(target=listener.serve_forever, daemon=True).start()
        origin = f"http://localhost:{proxy.server_port}"
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            context = browser.new_context()
            page = context.new_page(); console = []
            page.on("console", lambda message: console.append(message.text))
            page.add_init_script("window.__csp=[];document.addEventListener('securitypolicyviolation',e=>window.__csp.push(e.effectiveDirective));")
            deep_link = "/files/" + "ab" * 16 + "/nested%20folder"
            page.goto(origin + "/#" + deep_link)
            # Delay the real account request: no console or login flash during verification.
            expect(page.locator(".mantine-AppShell-root")).to_have_count(0)
            expect(page.get_by_label(field_label("登录名"))).to_have_count(0)
            account_gate.set()
            notifications = page.locator(".mantine-Notification-root")

            def signed_out():
                expect(page.get_by_label(field_label("登录名"))).to_be_visible()
                expect(page.locator(".mantine-AppShell-root")).to_have_count(0)
                expect(page.get_by_role("navigation", name="主要导航")).to_have_count(0)
                expect(page.get_by_role("button", name="检查服务连接", exact=True)).to_have_count(0)

            signed_out()
            assert page.evaluate("location.hash") == "#" + deep_link
            for route in ["/overview", "/vaults", deep_link, "/recovery", "/backup", "/clients", "/account", "/missing"]:
                page.evaluate("route => { location.hash = route; }", route)
                signed_out()
            assert not any(path.startswith("/v1/vaults") for path, *_ in proxy_results)
            assert health_requests == [], "anonymous gate started a health check"
            # Transport failure must not expose the console; retry the real backend.
            network["offline"] = True
            page.reload()
            signed_out()
            expect(page.get_by_role("button", name="重新核验会话", exact=True)).to_be_visible()
            network["offline"] = False
            page.get_by_role("button", name="重新核验会话", exact=True).click()
            signed_out()
            page.evaluate("location.hash = '/account'")

            def login(secret):
                page.get_by_label(field_label("登录名")).fill("owner")
                page.get_by_label(field_label("密码")).fill(secret)
                page.get_by_role("button", name="登录", exact=True).click()
                expect(page.get_by_text("已登录：owner", exact=True)).to_be_visible()

            login(password)
            expect(notifications.filter(has_text="登录成功")).to_have_attribute("role", "status")
            connection = page.locator(".app-topbar [role='status']")
            expect(connection).to_have_text("服务可用")
            assert len(health_requests) == 1, "login did not check health exactly once"
            page.evaluate("location.hash = '/overview'")
            check = page.get_by_role("button", name="检查服务连接", exact=True)
            expect(check).to_be_enabled()
            assert len(health_requests) == 1, "navigation repeated automatic health check"
            network["health_offline"] = True
            check.click()
            expect(connection).to_have_text("未连接")
            expect(check).to_be_enabled()
            expect(page.locator(".mantine-AppShell-root")).to_be_visible()
            network["health_offline"] = False
            check.click()
            expect(connection).to_have_text("服务可用")
            expect(check).to_be_enabled()
            page.evaluate("location.hash = '/account'")
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
            before_reload = len(health_requests)
            page.reload(); expect(page.get_by_text("已登录：owner", exact=True)).to_be_visible()
            expect(connection).to_have_text("服务可用")
            assert len(health_requests) == before_reload + 1
            assert next(c for c in context.cookies() if c["name"] == "vaultsyncd_session")["value"] != access["value"]

            # Same-site different origin can send Strict cookies; server still rejects CSRF.
            other = context.new_page(); other.goto(f"http://localhost:{evil.server_port}/")
            with other.expect_event("requestfailed", predicate=lambda request: request.url == origin + "/v1/auth/revoke-all"):
                other.evaluate("url => { fetch(url, {method:'POST', credentials:'include'}).catch(() => {}); }", origin + "/v1/auth/revoke-all")
            assert ("/v1/auth/revoke-all", 403, f"http://localhost:{evil.server_port}") in proxy_results
            other.close()

            page.get_by_label(field_label("当前密码")).fill(password)
            page.get_by_label(field_label("新密码")).fill(new_password)
            page.get_by_role("button", name="修改密码并撤销会话", exact=True).click()
            signed_out()
            assert second.request.get(origin + "/v1/auth/account", headers={"origin":origin}).status == 401
            assert second.request.post(origin + "/v1/auth/browser/refresh", headers={"origin":origin}).status == 401
            page.get_by_label(field_label("登录名")).fill("owner")
            page.get_by_label(field_label("密码")).fill(password)
            page.get_by_role("button", name="登录", exact=True).click()
            password_error = notifications.filter(has_text="密码不正确")
            expect(password_error).to_have_attribute("role", "alert")
            login(new_password)
            expect(password_error).to_have_count(0)
            expect(connection).to_have_text("服务可用")
            # Logout cancels the old shell's in-flight health check.
            pending_health = threading.Event()
            health_delay["gate"] = pending_health
            page.evaluate("location.hash = '/overview'")
            check.click()
            expect(connection).to_have_text("正在检查")
            expect(page.get_by_role("button", name="检查中…", exact=True)).to_be_disabled()
            page.evaluate("location.hash = '/account'")
            with page.expect_event("requestfailed", predicate=lambda request: request.url.endswith("/v1/health")):
                page.get_by_role("button", name="退出当前会话", exact=True).click()
            signed_out()
            assert page.evaluate("fetch('/v1/auth/account').then(r => r.status)") == 401
            assert page.evaluate("fetch('/v1/auth/browser/refresh',{method:'POST'}).then(r => r.status)") == 401
            health_delay["gate"] = None
            login(new_password)
            expect(connection).to_have_text("服务可用")
            pending_health.set()
            page.get_by_role("button", name="撤销全部会话", exact=True).click()
            signed_out()
            assert page.evaluate("fetch('/v1/auth/browser/refresh',{method:'POST'}).then(r => r.status)") == 401
            assert page.evaluate("localStorage.length") == 0
            # Login restores the requested route; feature-level revocation closes the whole shell.
            page.evaluate("location.hash = '/vaults'")
            page.get_by_label(field_label("登录名")).fill("owner")
            page.get_by_label(field_label("密码")).fill(new_password)
            page.get_by_role("button", name="登录", exact=True).click()
            expect(page.get_by_role("heading", name="Vault 目录", level=1, exact=True)).to_be_visible()
            assert page.evaluate("location.hash") == "#/vaults"
            network["health_offline"] = True
            page.reload()
            expect(page.get_by_role("heading", name="Vault 目录", level=1, exact=True)).to_be_visible()
            expect(connection).to_have_text("未连接")
            assert page.evaluate("location.hash") == "#/vaults"
            network["health_offline"] = False
            page.evaluate("location.hash = '/overview'")
            check.click()
            expect(connection).to_have_text("服务可用")
            page.screenshot(path=str(RUN / "connected.png"))
            assert page.evaluate("fetch('/v1/auth/revoke-all',{method:'POST'}).then(r => r.status)") == 200
            page.evaluate("location.hash = '/backup'")
            signed_out()
            assert page.evaluate("location.hash") == "#/backup"
            assert page.evaluate("fetch('/v1/vaults').then(r => r.status)") == 401
            combined = "\n".join(console) + (RUN / "server.log").read_text()
            assert password not in combined and new_password not in combined
            assert all(cookie["value"] not in combined for cookie in cookies + old_second_cookie)
            page.screenshot(path=str(RUN / "account.png"))
            assert page.evaluate("window.__csp") == [], page.evaluate("window.__csp")
            second.close(); context.close(); browser.close()
            (RUN / "result.json").write_text(json.dumps({"passed":True, "browser":playwright.chromium.name, "checks":["login", "httponly-secure-strict", "no-token-response", "no-store", "refresh-expired-access", "csrf-cross-origin", "password-revokes-all", "old-password-rejected", "logout", "revoke-all", "no-secret-logs-storage", "notifications/login-success/auth-error-clears-on-retry", "CSP"], "scope":"HTTP loopback isolated real server; unified login gate S145 and automatic health S148", "connection_checks":["no-anonymous-health", "login-check-once", "navigation-no-repeat", "manual-network-failure-retry", "refreshed-session-auto-check", "in-flight-disabled", "logout-aborts-health", "relogin-new-health", "restored-session-health-failure-preserves-auth", "CSP"], "gate_checks":["initial-check-no-console-flash", "all-routes-anonymous-blocked", "deep-link-preserved", "no-anonymous-feature-requests", "offline-check-retry", "server-revocation-closes-console", "authenticated-reload"]}, ensure_ascii=False, indent=2))
        print(f"S148 auth/automatic health browser acceptance passed: {RUN}")
    finally:
        for listener in [proxy, evil]:
            if listener: listener.shutdown(); listener.server_close()
        if server.poll() is None:
            server.terminate()
            try: server.wait(timeout=15)
            except subprocess.TimeoutExpired: server.kill(); server.wait()
        log.close()


if __name__ == "__main__": main()
