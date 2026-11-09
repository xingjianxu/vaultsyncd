#!/usr/bin/env python3
"""S147: real Chromium against an isolated btrfs service, no online SQL."""
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
RUN = Path(os.environ.get("VAULTSYNCD_WEB_RUN_ROOT", "/data/vaultsyncd-temp")) / f"S147-browser-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


def checked(args, **kwargs):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, **kwargs)
    if result.returncode:
        raise AssertionError(f"command failed: {args[0]} ({result.returncode}): {result.stderr}")
    return result.stdout


def main():
    RUN.mkdir(mode=0o700)
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    state, vaults = RUN / "state", RUN / "vaults"
    state.mkdir(mode=0o700); vaults.mkdir()
    password = secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", state, "--login", "owner"], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[vaults]\nallowed_bases=["{vaults}"]\n[log]\nformat="json"\nlevel="info"\n')
    log = (RUN / "server.log").open("w")
    backend_address = {"value": None}
    server = proxy = None

    def start():
        process = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log)
        for _ in range(150):
            lines = (RUN / "server.log").read_text().splitlines()
            addresses = [json.loads(line).get("fields", {}).get("listen") for line in lines if "服务端开始监听" in line]
            if len(addresses) > start.count:
                start.count += 1
                backend_address["value"] = addresses[-1]
                return process
            assert process.poll() is None
            time.sleep(.1)
        raise AssertionError("server did not listen")
    start.count = 0

    def api(path, body=None, token=None):
        conn = http.client.HTTPConnection(backend_address["value"], timeout=15)
        headers = {"Content-Type": "application/json"}
        if token: headers["Authorization"] = "Bearer " + token
        conn.request("POST" if body is not None else "GET", path, json.dumps(body) if body is not None else None, headers)
        response = conn.getresponse(); payload = json.loads(response.read()); conn.close()
        return response.status, payload

    try:
        server = start()
        assert api("/v1/clients")[0] == 401
        status, session = api("/v1/auth/login", {"login": "owner", "password": password, "device_label": "<img src=x onerror=alert(1)> 测试设备"})
        assert status == 200
        token = session["access_token"]
        status, first = api("/v1/clients", token=token)
        assert status == 200 and len(first["connections"]) == 1
        connection_id = first["connections"][0]["connection_id"]
        assert first["connections"][0]["client_kind"] == "client"
        assert "access_token" not in json.dumps(first) and "digest" not in json.dumps(first)
        status, renewed = api("/v1/auth/refresh", {"refresh_token": session["refresh_token"]})
        assert status == 200
        token = renewed["access_token"]
        rows = api("/v1/clients", token=token)[1]["connections"]
        assert len(rows) == 1 and rows[0]["connection_id"] == connection_id and rows[0]["current"]
        assert api("/v1/clients", token=session["access_token"])[0] == 401
        assert api("/v1/clients?after=broken", token=token)[0] == 400
        assert api("/v1/clients?after=" + "00" * 16, token=token)[0] == 400
        assert api("/v1/clients?after=" + connection_id + "&after=" + connection_id, token=token)[0] == 400
        assert api("/v1/auth/logout", {}, token=token)[0] == 200
        # More than one page, including a real revoked connection.
        for index in range(51):
            status, _ = api("/v1/auth/login", {"login": "owner", "password": password, "device_label": f"分页设备 {index:02}"})
            assert status == 200
        network = {"offline": False}

        class Handler(CspStaticFiles):
            def log_message(self, *_): pass
            def do_POST(self): self.forward()
            def do_GET(self):
                if self.path.startswith("/v1/"): self.forward()
                else: super().do_GET()
            def forward(self):
                if network["offline"] and self.path.startswith("/v1/clients"):
                    self.close_connection = True
                    return
                backend = http.client.HTTPConnection(backend_address["value"], timeout=15)
                headers = {key: value for key, value in self.headers.items() if key.lower() not in ("connection", "transfer-encoding")}
                body = self.rfile.read(int(self.headers.get("content-length", "0")))
                try:
                    backend.request(self.command, self.path, body=body, headers=headers)
                    response = backend.getresponse(); payload = response.read()
                    self.send_response(response.status)
                    for key, value in response.getheaders():
                        if key.lower() not in ("connection", "transfer-encoding", "content-length"): self.send_header(key, value)
                    self.send_header("content-length", str(len(payload))); self.end_headers(); self.wfile.write(payload)
                finally: backend.close()

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=str(REPO / "apps/web/dist")))
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        origin = f"http://localhost:{proxy.server_port}"
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            context = browser.new_context()
            page = context.new_page()
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.add_init_script("window.__csp=[];document.addEventListener('securitypolicyviolation',e=>window.__csp.push(e.effectiveDirective));")
            page.goto(origin + "/#/clients")
            expect(page.get_by_label(field_label("登录名"))).to_be_visible()
            expect(page.locator(".mantine-AppShell-root")).to_have_count(0)
            page.get_by_label(field_label("登录名")).fill("owner")
            page.get_by_label(field_label("密码")).fill(password)
            page.get_by_role("button", name="登录", exact=True).click()
            table = page.get_by_role("table", name="客户端连接记录")
            expect(table).to_be_visible()
            expect(table.locator("tbody tr")).to_have_count(50)
            expect(table.get_by_text("当前连接", exact=True)).to_have_count(1)
            expect(table.get_by_text("Web 浏览器", exact=True)).to_be_visible()
            expect(table.get_by_text("浏览器", exact=True)).to_be_visible()
            page.get_by_role("button", name="加载更多", exact=True).click()
            expect(table.locator("tbody tr")).to_have_count(53)
            expect(table.get_by_text("已撤销", exact=True)).to_be_visible()
            expect(table.get_by_text("<img src=x onerror=alert(1)> 测试设备", exact=True)).to_be_visible()
            expect(table.locator("img")).to_have_count(0)
            assert page.evaluate("fetch('/v1/clients').then(r=>r.headers.get('cache-control'))") == "no-store"
            rows = page.evaluate("fetch('/v1/clients').then(r=>r.json())")
            current = next(row for row in rows["connections"] if row["current"])
            expect(page.locator(".mantine-Notification-root")).to_have_count(0, timeout=8000)
            for scheme in ["light", "dark"]:
                page.emulate_media(color_scheme=scheme)
                for width in [320, 1440]:
                    page.set_viewport_size({"width": width, "height": 900})
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
                    page.evaluate("scrollTo(0,0)")
                    page.screenshot(path=str(RUN / f"clients-{scheme}-{width}.png"))
            page.set_viewport_size({"width": 1440, "height": 900})
            network["offline"] = True
            page.get_by_role("button", name="刷新", exact=True).click()
            expect(page.get_by_role("button", name="重试", exact=True)).to_be_visible()
            expect(table.locator("tbody tr")).to_have_count(53)
            network["offline"] = False
            page.get_by_role("button", name="重试", exact=True).click()
            expect(page.get_by_role("button", name="重试", exact=True)).to_have_count(0)
            # Restart actual process; browser cookie and history survive.
            server.terminate(); server.wait(timeout=15)
            server = start()
            page.reload()
            expect(table).to_be_visible()
            next_current = page.evaluate("fetch('/v1/clients').then(r=>r.json()).then(p=>p.connections.find(c=>c.current))")
            assert next_current["connection_id"] == current["connection_id"]
            # All-session revocation removes the page through the normal auth gate.
            result = page.evaluate("fetch('/v1/auth/revoke-all',{method:'POST',headers:{'content-type':'application/json'},body:'{}'}).then(r=>r.status)")
            assert result == 200
            page.get_by_role("button", name="刷新", exact=True).click()
            expect(page.get_by_label(field_label("登录名"))).to_be_visible()
            expect(page.locator(".mantine-AppShell-root")).to_have_count(0)
            assert page.evaluate("window.__csp") == []
            assert errors == [], errors
            browser.close()
        (RUN / "result.json").write_text(json.dumps({"status": "pass", "connections": 53, "screenshots": 4, "checks": ["auth gate", "rotation identity", "revocation", "pagination", "escaping", "network retry", "restart", "CSP", "320/1440 light/dark"]}, ensure_ascii=False, indent=2))
        print(f"S147 browser PASS: {RUN}")
    finally:
        if proxy: proxy.shutdown(); proxy.server_close()
        if server and server.poll() is None: server.terminate(); server.wait(timeout=15)
        log.close()


if __name__ == "__main__": main()
