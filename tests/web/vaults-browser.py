#!/usr/bin/env python3
"""S154: compact Vault toolbar and real registration/health regression.

Run after the Web build and Rust server/admin build:
nix develop .#web-tests --command python3 tests/web/vaults-browser.py
No online SQL, browser mocks, global dependency installation or Windows VM changes.
"""
import hashlib
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
RUN = Path("/data/vaultsyncd-temp") / f"S154-vaults-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


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
    sample = candidate / "keep.txt"; sample.write_bytes(b"S078 confirmed content is preserved\n")
    original_hash = hashlib.sha256(sample.read_bytes()).hexdigest()
    unsupported = RUN / "unsupported"; unsupported.mkdir()
    alternate = RUN / "alternate"; alternate.mkdir()
    mounted = unsupported_mounted = False
    password, new_password = secrets.token_urlsafe(24), secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", state, "--login", "owner"], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[vaults]\nallowed_bases=["{vaults}","{unsupported}"]\n[log]\nformat="json"\nlevel="info"\n')
    log = (RUN / "server.log").open("w")
    server = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log)
    proxy = evil = None
    try:
        checked(["sudo", "-n", "mount", "-t", "tmpfs", "-o", "size=1m", "tmpfs", unsupported])
        unsupported_mounted = True
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

        class Handler(CspStaticFiles):
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
            page.add_init_script("window.__csp=[];document.addEventListener('securitypolicyviolation',e=>window.__csp.push(e.effectiveDirective));")
            page.goto(origin + "/#/vaults")
            notifications = page.locator(".mantine-Notification-root")
            expect(page.get_by_label(field_label("登录名"))).to_be_visible()
            expect(page.get_by_role("navigation", name="主要导航")).to_have_count(0)

            def login(secret):
                page.get_by_label(field_label("登录名")).fill("owner")
                page.get_by_label(field_label("密码")).fill(secret)
                page.get_by_role("button", name="登录", exact=True).click()
                expect(page.get_by_role("heading", name="Vault 目录", level=1, exact=True)).to_be_visible()
                assert page.url == origin + "/#/vaults"

            login(password)
            page.get_by_role("link", name="Vault 目录", exact=True).click()
            expect(page.get_by_text(str(vaults), exact=True)).to_be_visible()
            expect(page.get_by_text("尚未注册 Vault", exact=True)).to_be_visible()
            toolbar = page.get_by_role("group", name="Vault 目录工具栏", exact=True)
            health_button = toolbar.get_by_role("button", name="Vault 健康与采集", exact=True)
            register_button = toolbar.get_by_role("button", name="注册服务器目录", exact=True)
            health = page.get_by_role("dialog", name="Vault 健康与采集", exact=True)
            registration = page.get_by_role("dialog", name="注册服务器目录", exact=True)
            expect(page.get_by_role("dialog")).to_have_count(0)
            expect(page.get_by_label(field_label("服务器目录绝对路径"))).to_have_count(0)
            expect(page.get_by_text("将允许基目录中的已有目录加入管理。", exact=True)).to_have_count(0)
            expect(page.get_by_text("管理服务器目录，核对挂载健康与采集进度。", exact=True)).to_have_count(0)

            evidence = RUN / "evidence"; evidence.mkdir()
            def layouts(state):
                for scheme in ("light", "dark"):
                    page.emulate_media(color_scheme=scheme)
                    for width in (320, 1440):
                        page.set_viewport_size({"width": width, "height": 900})
                        # Allow Mantine modal transitions and viewport reflow to settle.
                        page.wait_for_timeout(200)
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width)
                        bases = page.locator(".vaults-allowed-bases").bounding_box()
                        bar = toolbar.bounding_box()
                        assert bases and bar and abs(bases["x"] + bases["width"] - bar["x"] - bar["width"]) < 2
                        page.screenshot(path=str(evidence / f"{state}-list-{scheme}-{width}.png"), full_page=True)
                        health_button.click()
                        expect(health).to_be_visible()
                        page.wait_for_timeout(250)
                        page.screenshot(path=str(evidence / f"{state}-health-{scheme}-{width}.png"), full_page=True)
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
                        page.keyboard.press("Escape")
                        expect(health).to_have_count(0)
                        expect(health_button).to_be_focused()
                        register_button.click()
                        expect(registration).to_be_visible()
                        expect(registration.get_by_label(field_label("服务器目录绝对路径"))).to_be_focused()
                        page.wait_for_timeout(250)
                        page.screenshot(path=str(evidence / f"{state}-register-{scheme}-{width}.png"), full_page=True)
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
                        page.keyboard.press("Escape")
                        expect(registration).to_have_count(0)
                        expect(register_button).to_be_focused()
                page.set_viewport_size({"width": 1440, "height": 900})
            layouts("empty")
            def register(path):
                if not registration.count(): register_button.click()
                registration.get_by_label(field_label("服务器目录绝对路径")).fill(str(path))
                registration.get_by_role("button", name="校验并注册 Vault", exact=True).click()
            register(candidate)
            expect(notifications.filter(has_text="已注册 Vault")).to_have_attribute("role", "status")
            expect(notifications.filter(has_text="已注册 Vault")).to_contain_text("等待初始采集")
            expect(page.get_by_text(str(candidate), exact=True)).to_be_visible()
            expect(registration).to_have_count(0)
            assert hashlib.sha256(sample.read_bytes()).hexdigest() == original_hash
            ordinary = page.evaluate("fetch('/v1/vaults').then(r => r.json())")
            assert str(candidate) not in json.dumps(ordinary)
            managed = page.evaluate("fetch('/v1/admin/vaults').then(r => r.json())")
            assert len(managed["vaults"]) == 1 and managed["vaults"][0]["health"] == "healthy"
            assert managed["vaults"][0]["collection"]["state"] in ["not_scanned", "scanning", "pending", "complete"]
            register(candidate / "child")
            overlap_error = notifications.filter(has_text="路径重叠（overlapping_vault）")
            expect(overlap_error).to_have_attribute("role", "alert")
            register(unsupported)
            expect(notifications.filter(has_text="btrfs")).to_have_attribute("role", "alert")
            expect(overlap_error).to_have_count(0)
            register("/etc")
            expect(notifications.filter(has_text="允许基目录")).to_have_attribute("role", "alert")
            expect(notifications.filter(has_text="btrfs")).to_have_count(0)
            expect(registration.get_by_label(field_label("服务器目录绝对路径"))).to_have_value("/etc")
            page.keyboard.press("Escape")
            expect(registration).to_have_count(0)
            health_button.click()
            for _ in range(30):
                page.get_by_role("button", name="刷新目录状态", exact=True).click()
                try:
                    expect(page.get_by_text("本进程初始采集完成", exact=True)).to_be_visible(timeout=500)
                    break
                except AssertionError: time.sleep(.2)
            else: raise AssertionError("observer never provided completed initial import evidence")
            managed = page.evaluate("fetch('/v1/admin/vaults').then(r => r.json())")
            assert int(managed["vaults"][0]["vault_seq"]) > 0
            # Actual mount replacement, scoped only to this run; API must detect mount drift.
            checked(["sudo", "-n", "mount", "--bind", alternate, candidate]); mounted = True
            page.get_by_role("button", name="刷新目录状态", exact=True).click()
            expect(health.get_by_text("Vault 不可用", exact=True)).to_be_visible()
            expect(health.get_by_text("恢复原挂载", exact=False)).to_be_visible()
            unavailable = page.evaluate("fetch('/v1/admin/vaults').then(r => r.json())")["vaults"][0]
            assert unavailable["health"] == "unavailable" and unavailable["collection"]["state"] == "blocked"
            assert unavailable["reason"]
            checked(["sudo", "-n", "umount", candidate]); mounted = False
            page.get_by_role("button", name="刷新目录状态", exact=True).click()
            expect(page.get_by_text("挂载核对通过", exact=True)).to_be_visible()
            assert hashlib.sha256(sample.read_bytes()).hexdigest() == original_hash
            page.screenshot(path=str(evidence / "vaults.png"), full_page=True)
            page.keyboard.press("Escape")
            expect(health).to_have_count(0)
            layouts("populated")
            assert page.evaluate("localStorage.length") == 0
            assert page.evaluate("window.__csp") == [], page.evaluate("window.__csp")
            context.close(); browser.close()
            (evidence / "result.json").write_text(json.dumps({"passed":True,"checks":["toolbar-two-dialogs", "allowed-bases-right-aligned", "light/dark-320/1440-empty-and-populated", "dialog-autofocus-escape-return-focus", "registration-errors-retain-input", "owner-login", "allowed-bases", "empty-is-not-synced", "register-btrfs", "existing-hash-preserved", "ordinary-list-no-path", "overlap-rejected", "unsupported-filesystem-rejected", "outside-bases-rejected", "actual-observer-progress", "real-bind-mount-drift", "restore-mount-recovers-health", "source-hash-retained", "notifications/register-success/scoped-error-replacement", "CSP"], "mount_failure":unavailable, "hash":original_hash, "scope":"HTTP loopback browser test; HTTPS deployment S097"}, ensure_ascii=False, indent=2))
        print(f"S154 browser acceptance passed: {RUN}")
    finally:
        if mounted: checked(["sudo", "-n", "umount", candidate])
        if unsupported_mounted: checked(["sudo", "-n", "umount", unsupported])
        unsupported.rmdir()
        for listener in [proxy, evil]:
            if listener: listener.shutdown(); listener.server_close()
        if server.poll() is None:
            server.terminate()
            try: server.wait(timeout=15)
            except subprocess.TimeoutExpired: server.kill(); server.wait()
        log.close()


if __name__ == "__main__": main()
