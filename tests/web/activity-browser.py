#!/usr/bin/env python3
"""S172: real Chromium and isolated btrfs service; no online SQL or fabricated activity."""
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
from urllib.parse import parse_qs, urlsplit

from playwright.sync_api import expect, sync_playwright
from csp_support import CspStaticFiles, field_label

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug"))
RUN = Path(os.environ.get("VAULTSYNCD_WEB_RUN_ROOT", "/data/vaultsyncd-dev/vaultsyncd-temp")) / f"S172-activity-browser-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


def checked(args, **kwargs):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, **kwargs)
    assert result.returncode == 0, f"{args[0]} failed: {result.stderr}"
    return result.stdout


def main():
    RUN.mkdir(mode=0o700)
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    state, vaults = RUN / "state", RUN / "vaults"
    state.mkdir(mode=0o700); vaults.mkdir()
    documents, archive = vaults / "documents", vaults / "archive"
    documents.mkdir(); archive.mkdir()
    (documents / "anchor.txt").write_bytes(b"S172 observer anchor\n")
    (archive / "untouched.txt").write_bytes(b"S172 unrelated vault\n")
    password = secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", state, "--login", "owner"], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[vaults]\nallowed_bases=["{vaults}"]\n[log]\nformat="json"\nlevel="info"\n')
    log = (RUN / "server.log").open("w")
    service = proxy = browser = page = None
    backend_address = {"value": None}
    network = {"offline": False, "hold_direction": None, "hold_vault": None}
    triggered, release = threading.Event(), threading.Event()
    errors, checks = [], []

    def start():
        process = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log)
        for _ in range(200):
            addresses = [json.loads(line).get("fields", {}).get("listen") for line in (RUN / "server.log").read_text().splitlines() if "服务端开始监听" in line]
            if len(addresses) > start.count:
                start.count += 1
                backend_address["value"] = addresses[-1]
                return process
            assert process.poll() is None, "service exited"
            time.sleep(.1)
        raise AssertionError("service did not listen")
    start.count = 0

    def request(path, body=None, token=None, method=None, headers=None, binary=False):
        connection = http.client.HTTPConnection(backend_address["value"], timeout=30)
        request_headers = dict(headers or {})
        if token: request_headers["Authorization"] = "Bearer " + token
        if body is not None and not isinstance(body, bytes):
            request_headers["content-type"] = "application/json"
            body = json.dumps(body)
        connection.request(method or ("POST" if body is not None else "GET"), path, body, request_headers)
        response = connection.getresponse(); payload = response.read()
        result = (response.status, payload if binary else json.loads(payload) if payload.startswith((b"{", b"[")) else payload.decode(), dict(response.getheaders()))
        connection.close()
        return result

    def api(path, body=None, token=None):
        status, payload, _ = request(path, body, token)
        assert status in (200, 201, 202), payload
        return payload

    def snapshot(vault, token):
        metadata = api(f"/v1/vaults/{vault}/snapshots", {}, token)
        entries = api(f"/v1/vaults/{vault}/snapshots/{metadata['snapshot_id']}/entries?limit=1000", token=token)["entries"]
        return metadata, entries

    try:
        service = start()
        for invalid in ["", "bad\nname", "a" * 256]:
            assert request("/v1/auth/login", {"login": "owner", "password": password, "device_label": "bad", "hostname": invalid})[0] == 400
        status, session, _ = request("/v1/auth/login", {"login": "owner", "password": password, "device_label": "旧应用标签", "hostname": "<img src=x onerror=alert(1)> 客户端主机", "operating_system": "Windows 11 (10.0.26300) x64"})
        assert status == 200
        token = session["access_token"]
        doc_id = api("/v1/admin/vaults", {"path": str(documents)}, token)["vault_id"]
        archive_id = api("/v1/admin/vaults", {"path": str(archive)}, token)["vault_id"]
        for _ in range(200):
            if all(item["collection"]["state"] == "complete" for item in api("/v1/admin/vaults", token=token)["vaults"]): break
            time.sleep(.1)
        else: raise AssertionError("observer import did not complete")
        assert request(f"/v1/vaults/{doc_id}/activity")[0] == 401
        assert api(f"/v1/vaults/{doc_id}/activity", token=token)["items"] == []
        metadata, entries = snapshot(doc_id, token)
        parent = next(item for item in entries if "parent_id" not in item)
        data = b"S172 exact fixed content and historical path\n"
        digest = hashlib.sha256(data).hexdigest()
        upload = api(f"/v1/vaults/{doc_id}/uploads", {"total_size": str(len(data)), "hash": digest, "chunk_size": "4194304"}, token)
        status, _, _ = request(f"/v1/vaults/{doc_id}/uploads/{upload['upload_id']}/parts/0", data, token, "PUT", {"x-part-sha256": digest})
        assert status == 200
        api(f"/v1/vaults/{doc_id}/uploads/{upload['upload_id']}/finalize", {}, token)
        original_name, renamed_name = "文档 & history.txt", "renamed.txt"
        operation = {"kind": "create", "op_id": secrets.token_hex(16), "device_id": secrets.token_hex(16), "server_epoch": metadata["server_epoch"], "parent_id": parent["entry_id"], "name": original_name, "expected_absent": True, "upload_id": upload["upload_id"]}
        result = api(f"/v1/vaults/{doc_id}/operations", operation, token)
        while result["state"] == "pending":
            time.sleep(.1)
            result = api(f"/v1/vaults/{doc_id}/operations/{operation['op_id']}", token=token)
        assert result["state"] == "committed", result
        # Same immutable operation is not a second synchronization event.
        assert api(f"/v1/vaults/{doc_id}/operations", operation, token)["state"] == "committed"
        metadata, entries = snapshot(doc_id, token)
        entry = next(item for item in entries if item["name"] == original_name)
        download_path = f"/v1/vaults/{doc_id}/entries/{entry['entry_id']}/versions/{entry['version_id']}/content"
        for _ in range(54):
            status, payload, _ = request(download_path, token=token, binary=True)
            assert status == 200 and payload == data
        rename = {"kind": "move", "op_id": secrets.token_hex(16), "device_id": operation["device_id"], "server_epoch": metadata["server_epoch"], "entry_id": entry["entry_id"], "base_revision": entry["revision"], "parent_id": parent["entry_id"], "name": renamed_name}
        assert api(f"/v1/vaults/{doc_id}/operations", rename, token)["state"] == "committed"
        status, payload, _ = request(download_path, token=token, headers={"Range": "bytes=1-3"}, binary=True)
        assert status == 206 and payload == data[1:4]
        download_rows = api(f"/v1/vaults/{doc_id}/activity?direction=download", token=token)
        assert download_rows["items"][0]["path"] == renamed_name
        assert download_rows["items"][0]["offset"] == "1" and download_rows["items"][0]["bytes"] == "3"
        assert any(item.get("path") == original_name for item in download_rows["items"])
        upload_rows = api(f"/v1/vaults/{doc_id}/activity?direction=upload", token=token)["items"]
        assert len([item for item in upload_rows if item["action"] == "sync_commit"]) == 1
        assert next(item for item in upload_rows if item["action"] == "sync_commit")["path"] == original_name
        assert all(not ({"client_hostname", "client_os", "client_ip"} & row.keys()) for row in upload_rows)
        _, extended, _ = request(f"/v1/vaults/{doc_id}/activity?direction=upload", token=token, headers={"X-Vaultsyncd-Client-Properties": "1"})
        assert all(row["client_hostname"] == "<img src=x onerror=alert(1)> 客户端主机" and row["client_os"].startswith("Windows 11") and row["client_ip"] == "127.0.0.1" for row in extended["items"])
        checks.append("legacy-strict-shape/opt-in-client-properties")
        checks.append("real-upload/finalize/commit-idempotency/range/historical-path")

        class Handler(CspStaticFiles):
            def log_message(self, *_): pass
            def do_POST(self): self.forward()
            def do_GET(self):
                if self.path.startswith("/v1/"): self.forward()
                else: super().do_GET()
            def forward(self):
                activity = urlsplit(self.path).path.endswith("/activity")
                if activity and network["offline"]:
                    self.close_connection = True
                    return
                connection = http.client.HTTPConnection(backend_address["value"], timeout=40)
                body = self.rfile.read(int(self.headers.get("content-length", "0")))
                headers = {key: value for key, value in self.headers.items() if key.lower() not in ("connection", "transfer-encoding")}
                try:
                    connection.request(self.command, self.path, body, headers)
                    response = connection.getresponse(); payload = response.read()
                    query = parse_qs(urlsplit(self.path).query)
                    should_hold = activity and network["hold_direction"] is not None and query.get("direction", [""])[0] == network["hold_direction"] and network["hold_vault"] in self.path
                    if should_hold:
                        network["hold_direction"] = None
                        triggered.set()
                        assert release.wait(timeout=30), "held activity response not released"
                    self.send_response(response.status)
                    for key, value in response.getheaders():
                        if key.lower() not in ("connection", "transfer-encoding", "content-length"): self.send_header(key, value)
                    self.send_header("content-length", str(len(payload))); self.end_headers(); self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError): pass
                finally: connection.close()

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=str(REPO / "apps/web/dist")))
        proxy.daemon_threads = True
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        origin = f"http://localhost:{proxy.server_port}"
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            context = browser.new_context(viewport={"width": 1440, "height": 900})
            page = context.new_page()
            page.on("pageerror", lambda error: errors.append(str(error)))
            context.add_init_script("window.__csp=[];document.addEventListener('securitypolicyviolation',e=>window.__csp.push(e.effectiveDirective));")
            page.goto(origin + "/#/activity")
            expect(page.get_by_label(field_label("登录名"))).to_be_visible()
            expect(page.get_by_role("navigation", name="主要导航")).to_have_count(0)
            page.get_by_label(field_label("登录名")).fill("owner")
            page.get_by_label(field_label("密码")).fill(password)
            page.get_by_role("button", name="登录", exact=True).click()
            expect(page.get_by_role("heading", name="同步与传输记录", exact=True)).to_be_visible()
            page.get_by_label("Vault", exact=True).select_option(doc_id)
            table = page.get_by_role("table", name="同步与传输记录", exact=True)
            expect(table.locator("tbody tr")).to_have_count(50)
            expect(table.get_by_text("已提供下载内容", exact=True)).to_have_count(50)
            expect(table.get_by_text("<img src=x onerror=alert(1)> 客户端主机", exact=True)).to_have_count(50)
            expect(table.locator("img")).to_have_count(0)
            expect(table.get_by_text(renamed_name, exact=True)).to_have_count(1)
            table.locator("tbody tr").first.get_by_role("button", name="详情", exact=False).click()
            dialog = page.get_by_role("dialog", name="记录详情")
            expect(dialog.get_by_text("范围起点", exact=True)).to_be_visible()
            expect(dialog.get_by_text("3 B", exact=True)).to_be_visible()
            expect(dialog.get_by_text("版本", exact=True)).to_be_visible()
            expect(dialog.get_by_text("Windows 11 (10.0.26300) x64", exact=True)).to_be_visible()
            expect(dialog.get_by_text("登录来源 IP", exact=True)).to_be_visible()
            expect(dialog.get_by_text("127.0.0.1", exact=True)).to_be_visible()
            page.keyboard.press("Escape")
            page.get_by_role("button", name="下一页", exact=True).click()
            expect(table.get_by_text("同步已提交", exact=True)).to_have_count(1)
            expect(table.get_by_text("上传内容已校验", exact=True)).to_have_count(1)
            page.get_by_role("button", name="第一页", exact=True).click()
            expect(table.locator("tbody tr")).to_have_count(50)
            page.get_by_label("传输方向", exact=True).select_option("upload")
            expect(table.locator("tbody tr")).to_have_count(2)
            expect(table.get_by_text("同步已提交", exact=True)).to_have_count(1)
            expect(page.get_by_role("button", name="下一页", exact=True)).to_be_disabled()
            assert "客户端已下载" not in table.inner_text()
            assert page.evaluate("path => fetch(path).then(r=>r.headers.get('cache-control'))", f"/v1/vaults/{doc_id}/activity") == "no-store"
            checks.append("auth-gate/client-escaping/direction/pagination/record-details/no-store")

            # A real old upload response cannot replace the newer download filter.
            page.get_by_label("传输方向", exact=True).select_option("download")
            expect(table.locator("tbody tr")).to_have_count(50)
            triggered.clear(); release.clear()
            network.update(hold_direction="upload", hold_vault=doc_id)
            page.get_by_label("传输方向", exact=True).select_option("upload")
            assert triggered.wait(timeout=10)
            page.get_by_label("传输方向", exact=True).select_option("download")
            expect(table.locator("tbody tr")).to_have_count(50)
            release.set(); page.wait_for_timeout(250)
            expect(table.get_by_text("同步已提交", exact=True)).to_have_count(0)
            # Same protection when switching Vault while the previous request is held.
            triggered.clear(); release.clear()
            network.update(hold_direction="download", hold_vault=doc_id)
            page.get_by_role("button", name="刷新", exact=True).click()
            assert triggered.wait(timeout=10)
            page.get_by_label("Vault", exact=True).select_option(archive_id)
            expect(page.get_by_text("暂无同步与传输记录。", exact=True)).to_be_visible()
            release.set(); page.wait_for_timeout(250)
            expect(page.get_by_role("table", name="同步与传输记录", exact=True).locator("tbody tr")).to_have_count(0)
            checks.append("late-response-filter-and-vault-isolation/empty-vault")
            page.get_by_label("Vault", exact=True).select_option(doc_id)
            expect(table.locator("tbody tr")).to_have_count(50)
            network["offline"] = True
            page.get_by_role("button", name="刷新", exact=True).click()
            expect(page.get_by_role("button", name="重试", exact=True)).to_be_visible()
            expect(page.get_by_text("暂无同步与传输记录。", exact=True)).to_have_count(0)
            network["offline"] = False
            page.get_by_role("button", name="重试", exact=True).click()
            expect(table.locator("tbody tr")).to_have_count(50)
            expect(page.get_by_role("button", name="重试", exact=True)).to_have_count(0)
            service.terminate(); service.wait(timeout=15); service = start()
            page.reload()
            page.get_by_label("Vault", exact=True).select_option(doc_id)
            expect(table.locator("tbody tr")).to_have_count(50)
            checks.append("network-error-not-empty/retry/restart-durability")
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({"width": width, "height": 900})
                    page.wait_for_timeout(200)
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
                    page.screenshot(path=str(RUN / f"activity-{scheme}-{width}.png"))
            page.set_viewport_size({"width": 1440, "height": 900})
            assert page.evaluate("fetch('/v1/auth/revoke-all',{method:'POST',headers:{'content-type':'application/json'},body:'{}'}).then(r=>r.status)") == 200
            page.get_by_role("button", name="刷新", exact=True).click()
            expect(page.get_by_label(field_label("登录名"))).to_be_visible()
            expect(page.get_by_role("navigation", name="主要导航")).to_have_count(0)
            assert errors == [], errors
            assert page.evaluate("window.__csp") == []
            checks.append("320-1440/light-dark/CSP/no-runtime-error/revocation")
            browser.close(); browser = None
        (RUN / "result.json").write_text(json.dumps({"status": "pass", "checks": checks}, ensure_ascii=False, indent=2))
        print(f"S172 activity browser PASS: {RUN}", flush=True)
    except Exception:
        if page and not page.is_closed():
            page.screenshot(path=str(RUN / "failure.png"), full_page=True)
            (RUN / "failure.html").write_text(page.content())
        raise
    finally:
        release.set()
        if browser: browser.close()
        if proxy: proxy.shutdown(); proxy.server_close()
        if service and service.poll() is None: service.terminate(); service.wait(timeout=15)
        log.close()


if __name__ == "__main__": main()
