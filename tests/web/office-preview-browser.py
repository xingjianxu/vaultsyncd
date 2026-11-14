#!/usr/bin/env python3
"""S213: real Office -> OnlyOffice -> PDF -> Chromium, in isolated btrfs state.

Run in the pinned web-tests devShell. VAULTSYNCD_TEST_BIN_DIR,
VAULTSYNCD_TEST_WEB_DIR, VAULTSYNCD_WEB_RUN_ROOT select built inputs/evidence.
VAULTSYNCD_TEST_OFFICE_FIXTURES must contain demo.docx/demo.xlsx/demo.pptx;
VAULTSYNCD_TEST_OFFICE_JWT_FILE points to a private test copy of the service key.
Both paths must be under /data/vaultsyncd-dev. The conversion service defaults
only to the separately deployed S212 endpoint; production vaultsyncd is untouched.
Explicit proxy faults affect only this run. The second account is seeded
OFFLINE after stopping this test server; no online database is modified directly.
"""
from contextlib import contextmanager
import functools
import hashlib
import http.client
import http.server
import json
import os
import re
from pathlib import Path
import secrets
import shutil
import sqlite3
import stat
import subprocess
import threading
import time
from urllib.parse import urlsplit
import zipfile

from playwright.sync_api import Error, expect, sync_playwright
from csp_support import CspStaticFiles, field_label

REPO = Path(__file__).resolve().parents[2]
DEV = Path("/data/vaultsyncd-dev").resolve()
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug")).resolve()
WEB = Path(os.environ.get("VAULTSYNCD_TEST_WEB_DIR", REPO / "apps/web/dist")).resolve()
PARENT = Path(os.environ.get("VAULTSYNCD_WEB_RUN_ROOT", str(DEV))).resolve()
RUN = PARENT / f"S213-office-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"
OFFICE = os.environ.get("VAULTSYNCD_TEST_OFFICE_URL", "https://wlab.132140.xyz:15402")
NAMES = {"docx": "文档 & 样本.docx", "xlsx": "工作簿 & 样本.xlsx", "pptx": "幻灯片 & 样本.pptx"}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def checked(args, **kwargs):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, **kwargs)
    assert result.returncode == 0, f"{args[0]} failed: {result.stderr}"
    return result.stdout


def main():
    assert PARENT.is_relative_to(DEV) and PARENT.is_dir() and os.access(PARENT, os.W_OK), PARENT
    source = Path(os.environ["VAULTSYNCD_TEST_OFFICE_FIXTURES"]).resolve()
    key = Path(os.environ["VAULTSYNCD_TEST_OFFICE_JWT_FILE"]).resolve()
    assert source.is_relative_to(DEV) and source.is_dir(), source
    assert key.is_relative_to(DEV) and key.is_file() and key.stat().st_uid == os.geteuid(), key
    assert not stat.S_IMODE(key.stat().st_mode) & 0o077, "test JWT file must remain private"
    assert urlsplit(OFFICE).scheme == "https" or (urlsplit(OFFICE).scheme == "http" and urlsplit(OFFICE).hostname in ("127.0.0.1", "localhost"))
    assert (WEB / "index.html").is_file(), WEB
    assert all(os.access(BIN / name, os.X_OK) for name in ("vaultsyncd-admin", "vaultsyncd-server")), BIN
    RUN.mkdir(mode=0o700)
    for name in ("tmp", "cache", "downloads", "evidence", "vaults", "state", "intruder-state"):
        (RUN / name).mkdir(mode=0o700)
    # Chromium's SingletonSocket must fit sockaddr_un (108 bytes).
    browser_tmp = DEV / "cache/tmp" / f"S213-{os.getpid()}"
    browser_tmp.mkdir(mode=0o700, parents=True)
    os.environ.update(TMPDIR=str(browser_tmp), XDG_CACHE_HOME=str(RUN / "cache"))
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    evidence, documents = RUN / "evidence", RUN / "vaults/documents"
    print(f"S213 artifacts: {RUN}", flush=True)
    documents.mkdir(); (documents / "child").mkdir()
    fixtures = {NAMES[extension]: (source / f"demo.{extension}").read_bytes() for extension in NAMES}
    assert all(fixtures.values()), "Office fixtures cannot be empty"
    fixtures["损坏.docx"] = b"PK\x03\x04\x14\x00S213 truncated OOXML archive\x00\xff"
    expected_disk = dict(fixtures)
    for name, data in fixtures.items():
        (documents / name).write_bytes(data)
    (documents / "child/leaf.txt").write_text("S213 child file\n")
    (evidence / "inputs.json").write_text(json.dumps({
        "binary_directory": str(BIN), "web_directory": str(WEB), "conversion_service": OFFICE,
        "binaries": {name: sha((BIN / name).read_bytes()) for name in ("vaultsyncd-admin", "vaultsyncd-server")},
        "assets_sha256": {str(path.relative_to(WEB)): sha(path.read_bytes()) for path in sorted(WEB.rglob("*")) if path.is_file()},
        "fixtures": {name: {"size": len(data), "sha256": sha(data)} for name, data in fixtures.items()},
        "account_fixture": "Both accounts initialized with admin; intruder row copied only while the isolated server is stopped, after owner registers its Vault.",
    }, ensure_ascii=False, indent=2))
    password, intruder_password = secrets.token_urlsafe(24), secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", RUN / "state", "--login", "owner"], input=password + "\n")
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", RUN / "intruder-state", "--login", "intruder"], input=intruder_password + "\n")
    # Prepare a real admin-generated identity. It is inserted only during the
    # explicit offline restart below, after the sole owner registers its Vault.
    with sqlite3.connect(RUN / "intruder-state/server.sqlite") as other:
        row = other.execute("SELECT id,login,password_hash,status FROM users WHERE login='intruder'").fetchone()
    assert row and len(row[0]) == 16
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{RUN / "state"}"\n[vaults]\nallowed_bases=["{RUN / "vaults"}"]\n[log]\nformat="json"\nlevel="info"\n[office]\ndocument_server_url={json.dumps(OFFICE)}\njwt_secret_file={json.dumps(str(key))}\n')
    log = (evidence / "server.log").open("w")
    service = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log, cwd=RUN)
    proxy = context = page = None
    requests, checks, errors, violations, external, dialogs, viewer_states = [], [], [], [], [], [], {}
    lock = threading.Lock()
    fault = {"mode": None, "entry_id": None}
    holds = []
    failure = None

    def arm(mode=None, entry_id=None):
        with lock:
            fault.clear(); fault.update(mode=mode, entry_id=entry_id)
            if mode == "delay":
                started, release, finished = threading.Event(), threading.Event(), threading.Event()
                fault.update(started=started, release=release, finished=finished)
                holds.append(release)
                return started, release, finished

    @contextmanager
    def runtime():
        with sync_playwright() as playwright:
            try:
                yield playwright
            except Exception:
                if page and not page.is_closed():
                    try:
                        page.screenshot(path=str(evidence / "failure.png"), full_page=True)
                        (evidence / "failure.html").write_text(page.content())
                    except Exception as cause:
                        errors.append(f"capture: {cause}")
                raise
            finally:
                if context:
                    context.close()

    try:
        address = None
        for _ in range(150):
            for line in (evidence / "server.log").read_text().splitlines():
                fields = json.loads(line).get("fields", {})
                if fields.get("message") == "服务端开始监听":
                    address = fields["listen"]
            if address:
                break
            assert service.poll() is None, "server exited"
            time.sleep(.2)
        assert address, "server did not listen"

        class Handler(CspStaticFiles):
            def log_message(self, *_):
                pass

            def do_POST(self):
                self.forward()

            def do_GET(self):
                self.forward() if self.path.startswith("/v1/") else super().do_GET()

            def forward(self):
                backend = http.client.HTTPConnection(address, timeout=150)
                body = self.rfile.read(int(self.headers.get("content-length", "0")))
                headers = {k: v for k, v in self.headers.items() if k.lower() not in ("connection", "transfer-encoding")}
                path = urlsplit(self.path).path
                record = {"method": self.command, "path": self.path}
                with lock:
                    current = dict(fault); requests.append(record)
                office = path.endswith("/office-preview")
                targeted = office and f"/entries/{current['entry_id']}/" in path
                if office:
                    record["office_request"] = json.loads(body)
                try:
                    if targeted and current["mode"] in ("503", "422"):
                        status = int(current["mode"])
                        payload = json.dumps({"error": {"code": "NEEDS_RECOVERY" if status == 503 else "INVALID_REQUEST", "message": "隔离转换服务暂不可用，请重试。" if status == 503 else "Office 文件转换失败（隔离故障夹具）。", "request_id": "S213-fault", "retryable": status == 503}}).encode()
                        response_headers = [("content-type", "application/json")]
                        record["fault"] = f"simulated-converter-{status}"
                    else:
                        backend.request(self.command, self.path, body=body, headers=headers)
                        response = backend.getresponse(); status, payload = response.status, response.read()
                        response_headers = response.getheaders()
                    record["status"] = status
                    if status == 200 and "/snapshots/" in path and path.endswith("/entries") and current["mode"] == "oversize":
                        data = json.loads(payload)
                        for item in data["entries"]:
                            if item["entry_id"] == current["entry_id"]:
                                item["size"] = str(50 * 1024 ** 2 + 1); record["fault"] = "snapshot-oversize"
                        payload = json.dumps(data).encode()
                    if office and status == 200:
                        assert key.read_bytes().strip() not in payload, "JWT leaked in Office response"
                        assert re.search(rb"http://127\.0\.0\.1:[0-9]+/[0-9a-f]{64}", payload) is None, "temporary source URL leaked in Office response"
                        record["pdf_bytes"] = len(payload); record["pdf_sha256"] = sha(payload)
                        record["preview_headers"] = {k.lower(): v for k, v in response_headers if k.lower() in (
                            "content-type", "content-length", "x-content-sha256", "x-preview-source-version", "x-preview-source-sha256")}
                    if targeted and status == 200:
                        record["fault"] = current["mode"]
                        if current["mode"] == "corrupt" and payload:
                            payload = payload[:-1] + bytes([payload[-1] ^ 1])
                        elif current["mode"] == "wrong-source":
                            response_headers = [(k, "0" * 64 if k.lower() == "x-preview-source-sha256" else v) for k, v in response_headers]
                        elif current["mode"] == "delay":
                            current["started"].set()
                            assert current["release"].wait(60), "delayed conversion response not released"
                    self.send_response(status)
                    for k, v in response_headers:
                        if k.lower() not in ("connection", "transfer-encoding", "content-length"):
                            self.send_header(k, v)
                    self.send_header("content-length", str(len(payload))); self.end_headers()
                    self.wfile.write(payload); record["delivered"] = True
                except (BrokenPipeError, ConnectionResetError):
                    record["browser_cancelled"] = True
                except Exception as cause:
                    record["proxy_error"] = str(cause); self.close_connection = True
                finally:
                    if targeted and current["mode"] == "delay":
                        current["finished"].set()
                    backend.close()

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=str(WEB)))
        proxy.daemon_threads = True
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        origin = f"http://localhost:{proxy.server_port}"
        with runtime() as playwright:
            context = playwright.chromium.launch_persistent_context(user_data_dir=str(RUN / "browser-profile"),
                downloads_path=str(RUN / "downloads"), executable_path=shutil.which("chromium"),
                headless=True, accept_downloads=True, viewport={"width": 1440, "height": 900})
            context.expose_binding("s213CspViolation", lambda _source, detail: violations.append(detail))
            context.add_init_script("document.addEventListener('securitypolicyviolation',e=>window.s213CspViolation({directive:e.effectiveDirective,blocked:e.blockedURI}));")
            def observe(opened):
                opened.on("pageerror", lambda error: errors.append(str(error)))
            context.on("page", observe)
            for opened in context.pages:
                observe(opened)
            context.on("request", lambda request: external.append(request.url) if urlsplit(request.url).scheme in ("http", "https") and not request.url.startswith(origin + "/") else None)
            page = context.pages[0] if context.pages else context.new_page()
            page.on("dialog", lambda dialog: (dialogs.append(dialog.message), dialog.dismiss()))
            page.goto(origin + "/#/files")
            page.get_by_label(field_label("登录名")).fill("owner")
            page.get_by_label(field_label("密码")).fill(password)
            page.get_by_role("button", name="登录", exact=True).click()
            expect(page.get_by_role("combobox", name="浏览 Vault", exact=True)).to_be_visible(timeout=30000)

            def api(path, body=None, allowed=(200, 201), anonymous=False):
                value = page.evaluate("""async ([path,body,anonymous])=>{
                    const response=await fetch(path,{...(body===null?{}:{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)}),...(anonymous?{credentials:'omit'}:{})});
                    if(response.headers.get('content-type')?.startsWith('application/pdf')){
                        const bytes=new Uint8Array(await response.arrayBuffer());
                        const digest=Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256',bytes)),x=>x.toString(16).padStart(2,'0')).join('');
                        return {status:response.status,pdf:{size:bytes.length,sha256:digest,prefix:new TextDecoder().decode(bytes.slice(0,5))},headers:Object.fromEntries(response.headers)};
                    }
                    return {status:response.status,body:await response.json()};
                }""", [path, body, anonymous])
                assert value["status"] in allowed, value
                return value

            def wait_for(read, message):
                for _ in range(360):
                    value = read()
                    if value:
                        return value
                    page.wait_for_timeout(250)
                raise AssertionError(message)

            vault = api("/v1/admin/vaults", {"path": str(documents)})["body"]["vault_id"]
            wait_for(lambda: all(item["collection"]["state"] == "complete" for item in api("/v1/admin/vaults")["body"]["vaults"]), "observer import incomplete")
            def snapshot_entries():
                snapshot = api(f"/v1/vaults/{vault}/snapshots", {})["body"]
                return api(f"/v1/vaults/{vault}/snapshots/{snapshot['snapshot_id']}/entries?limit=1000")["body"]["entries"]
            entries = snapshot_entries(); by_name = {item["name"]: item for item in entries}
            for name, data in fixtures.items():
                assert by_name[name]["hash"] == sha(data) and by_name[name]["size"] == str(len(data)), by_name[name]
            checks.append("real-observer/committed-Office-fixtures/hash-and-size")
            # Vault registration deliberately requires one active owner. First
            # register through that real API, then stop this run's server before
            # inserting the independent account fixture. Existing browser
            # cookies/session remain valid through the same-origin proxy.
            page.evaluate("location.hash='/account'")
            expect(page.get_by_role("button", name="退出当前会话", exact=True)).to_be_visible()
            service.terminate(); service.wait(timeout=30)
            with sqlite3.connect(RUN / "state/server.sqlite") as database:
                database.execute("INSERT INTO users(id,login,password_hash,status) VALUES(?,?,?,?)", row)
            prior_lines = len((evidence / "server.log").read_text().splitlines())
            service = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log, cwd=RUN)
            restarted = None
            for _ in range(150):
                for line in (evidence / "server.log").read_text().splitlines()[prior_lines:]:
                    fields = json.loads(line).get("fields", {})
                    if fields.get("message") == "服务端开始监听":
                        restarted = fields["listen"]
                if restarted:
                    break
                assert service.poll() is None, "restarted server exited"
                time.sleep(.2)
            assert restarted, "offline account-fixture restart did not listen"
            address = restarted
            checks.append("offline-intruder-account-fixture/restart-retains-owner-session-and-Vault")
            def endpoint(entry):
                return f"/v1/vaults/{vault}/entries/{entry['entry_id']}/versions/{entry['version_id']}/office-preview"
            def payload(entry):
                return {"file_type": entry["name"].split(".")[-1].lower(), "hash": entry["hash"], "size": entry["size"]}
            original = by_name[NAMES["docx"]]
            assert api(endpoint(original), payload(original), allowed=(401,), anonymous=True)["body"]["error"]["code"] == "AUTH_EXPIRED"
            login = http.client.HTTPConnection(address, timeout=30)
            login.request("POST", "/v1/auth/login", body=json.dumps({"login": "intruder", "password": intruder_password, "device_label": "S213 offline-fixture account"}), headers={"content-type": "application/json"})
            auth_response = login.getresponse(); intruder = json.loads(auth_response.read()); assert auth_response.status == 200
            login.request("POST", endpoint(original), body=json.dumps(payload(original)), headers={"content-type": "application/json", "authorization": "Bearer " + intruder["access_token"]})
            denied = login.getresponse(); denied_body = json.loads(denied.read()); assert denied.status == 403 and denied_body["error"]["code"] == "ACCESS_DENIED", denied_body
            login.close(); intruder.clear()
            checks.append("real-login/anonymous-401/cross-user-403")
            assert api(endpoint(original), {**payload(original), "hash": "0" * 64}, allowed=(422,))["body"]["error"]["code"] == "HASH_MISMATCH"
            api(endpoint(original), {**payload(original), "size": str(50 * 1024 ** 2 + 1)}, allowed=(413,))
            checks.append("wrong-snapshot-digest-422/oversize-request-413")
            page.goto(origin + f"/#/files/{vault}"); page.reload()
            table, preview = page.get_by_role("table", name="文件与目录", exact=True), page.get_by_role("dialog")
            def link(name):
                return table.get_by_role("button", name=name, exact=True)
            def open_preview(name, menu=False):
                expect(link(name)).to_be_visible(timeout=20000)
                if menu:
                    link(name).click(button="right")
                    page.get_by_role("menuitem", name="预览", exact=True).click()
                else:
                    link(name).click()
                expect(page.get_by_role("dialog", name=f"预览 · {name}", exact=True)).to_be_visible()
            def close_preview():
                preview.get_by_role("button", name="关闭预览", exact=True).click()
                expect(preview).not_to_be_visible()
            def native_pdf():
                expect(preview.locator("iframe.file-preview-pdf")).to_be_visible(timeout=120000)
                def loaded_frame():
                    for frame in page.frames:
                        if frame.url.startswith("chrome-extension://"):
                            try:
                                if frame.locator("pdf-viewer").count():
                                    return frame
                            except Error:
                                pass
                    return None
                frame = wait_for(loaded_frame, "browser PDF viewer did not load")
                expect(frame.locator("pdf-viewer")).to_be_visible(timeout=20000)
                wait_for(lambda: frame.locator("pdf-viewer").evaluate("v=>v.loadProgress===100||v.loadProgress_===100"), "browser PDF did not finish loading")
                state = frame.locator("pdf-viewer").evaluate("v=>({dimensions:v.documentDimensions,progress:v.loadProgress_})")
                assert len(state["dimensions"]["pageDimensions"]) > 0, state
                assert preview.locator("iframe.file-preview-pdf").get_attribute("src").startswith("blob:")
                expect(preview.locator("canvas,object,embed")).to_have_count(0)
                return state
            def download_exact(name):
                with page.expect_download() as pending:
                    preview.get_by_role("button", name="下载原文件", exact=True).click()
                download = pending.value
                assert download.suggested_filename == name and download.failure() is None
                saved = RUN / "downloads" / name
                download.save_as(saved); assert saved.read_bytes() == fixtures[name]

            for extension, name in NAMES.items():
                start, before_url = len(requests), page.url
                open_preview(name, menu=extension == "xlsx")
                viewer_states[extension] = native_pdf()
                assert page.url == before_url
                conversion = [item for item in requests[start:] if item["path"].endswith("/office-preview")]
                assert len(conversion) == 1 and conversion[0]["status"] == 200, conversion
                record = conversion[0]
                assert set(record["office_request"]) == {"file_type", "hash", "size"}
                assert key.read_text().strip() not in page.content() and OFFICE not in page.content()
                assert record["preview_headers"]["x-content-sha256"] == record["pdf_sha256"]
                assert record["preview_headers"]["x-preview-source-sha256"] == by_name[name]["hash"]
                assert record["preview_headers"]["x-preview-source-version"] == by_name[name]["version_id"]
                download_exact(name)
                page.screenshot(path=str(evidence / f"office-{extension}.png"))
                blob_url = preview.locator("iframe.file-preview-pdf").get_attribute("src")
                close_preview()
                if extension == "docx":
                    revoked = context.new_page()
                    try:
                        revoked.goto(blob_url)
                        raise AssertionError("closed Office PDF Blob URL remains readable")
                    except Error as cause:
                        assert "ERR_FILE_NOT_FOUND" in str(cause), cause
                    finally:
                        revoked.close()
            checks.append("DOCX-XLSX-PPTX/real-OnlyOffice-conversion/native-PDF-pages/click-context-menu/exact-original-download")

            # Change a valid OOXML file through the real observer, then ask for
            # the previous snapshot version. A fallback to current head would
            # mismatch the original hash; success proves the old version is used.
            with zipfile.ZipFile(documents / NAMES["docx"], "a") as archive:
                archive.comment = b"S213 second committed immutable source version"
            expected_disk[NAMES["docx"]] = (documents / NAMES["docx"]).read_bytes()
            changed = wait_for(lambda: next((item for item in snapshot_entries() if item["entry_id"] == original["entry_id"] and item["version_id"] != original["version_id"]), None), "new Office source version not committed")
            assert changed["hash"] == sha(expected_disk[NAMES["docx"]]) and changed["hash"] != original["hash"]
            old_pdf = api(endpoint(original), payload(original))
            assert old_pdf["headers"]["x-preview-source-version"] == original["version_id"]
            assert old_pdf["headers"]["x-preview-source-sha256"] == original["hash"]
            assert old_pdf["pdf"]["sha256"] == old_pdf["headers"]["x-content-sha256"] and old_pdf["pdf"]["prefix"] == "%PDF-"
            checks.append("real-new-head/old-snapshot-version-still-converts/no-head-fallback")

            damaged_start = len(requests)
            open_preview("损坏.docx")
            # OnlyOffice may repair even a truncated OOXML input. Accept its
            # real 422 as an error, or require the repaired PDF to load fully;
            # neither result is fabricated and the source remains untouched.
            wait_for(lambda: preview.get_by_role("alert").is_visible() or preview.locator("iframe.file-preview-pdf").is_visible(), "damaged input returned neither PDF nor error")
            if preview.get_by_role("alert").is_visible():
                expect(preview.get_by_role("alert")).to_contain_text("转换失败")
                expect(preview.locator("iframe")).to_have_count(0)
                expected_status = 422
            else:
                native_pdf(); expected_status = 200
            damaged = [item for item in requests[damaged_start:] if item["path"].endswith("/office-preview")]
            assert len(damaged) == 1 and damaged[0]["status"] == expected_status, damaged
            download_exact("损坏.docx"); close_preview()
            checks.append("real-damaged-Office/actual-error-or-checked-repaired-PDF/original-download-retained")
            for mode, message in (("503", "暂不可用"), ("422", "转换失败"), ("corrupt", "SHA-256"), ("wrong-source", "源版本或摘要不匹配")):
                arm(mode, by_name[NAMES["xlsx"]]["entry_id"])
                open_preview(NAMES["xlsx"])
                expect(preview.get_by_role("alert")).to_contain_text(message, timeout=120000)
                expect(preview.locator("iframe")).to_have_count(0)
                arm()
                preview.get_by_role("button", name="重试预览", exact=True).click(); native_pdf(); close_preview()
            checks.append("isolated-proxy-503-422/corrupt-PDF/source-mismatch/no-viewer/retry-real-conversion")

            arm("oversize", by_name[NAMES["xlsx"]]["entry_id"])
            page.reload(); start = len(requests)
            open_preview(NAMES["xlsx"])
            expect(preview.get_by_role("alert")).to_contain_text("50 MiB")
            assert not any(f"/entries/{by_name[NAMES['xlsx']]['entry_id']}/" in item["path"] and item["path"].endswith(("/content", "/office-preview")) for item in requests[start:])
            close_preview(); arm(); page.reload()
            checks.append("oversize-snapshot/rejected-before-content-or-converter-request")

            def delayed():
                events = arm("delay", by_name[NAMES["xlsx"]]["entry_id"])
                open_preview(NAMES["xlsx"])
                wait_for(events[0].is_set, "conversion response was not held")
                return events
            _, release, finished = delayed()
            preview.get_by_role("button", name="取消预览", exact=True).click()
            expect(preview.get_by_role("alert")).to_contain_text("已取消")
            expect(preview.locator("iframe")).to_have_count(0)
            close_preview(); arm(); open_preview(NAMES["pptx"]); native_pdf()
            release.set(); wait_for(finished.is_set, "late conversion response did not finish")
            expect(page.get_by_role("dialog", name=f"预览 · {NAMES['pptx']}", exact=True)).to_be_visible()
            close_preview()
            checks.append("cancel-close/late-conversion-cannot-replace-new-preview")
            _, release, finished = delayed()
            arm(); page.evaluate("path=>location.hash=path", f"/files/{vault}/child")
            expect(preview).not_to_be_visible()
            expect(link("leaf.txt")).to_be_visible(timeout=20000)
            release.set(); wait_for(finished.is_set, "navigation response did not finish")
            page.evaluate("path=>location.hash=path", f"/files/{vault}")
            checks.append("navigation/closes-and-cancels-inflight-conversion")
            open_preview(NAMES["docx"]); native_pdf()
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({"width": width, "height": 900})
                    box = preview.bounding_box()
                    assert box and box["x"] >= -1 and box["x"] + box["width"] <= width + 1, box
                    assert page.evaluate("document.documentElement.scrollWidth<=innerWidth"), (scheme, width)
                    page.wait_for_timeout(200); page.screenshot(path=str(evidence / f"office-{scheme}-{width}.png"))
            close_preview(); page.set_viewport_size({"width": 1440, "height": 900})
            checks.append("Office/light-dark/320-1440/no-horizontal-page-overflow")
            _, release, finished = delayed()
            arm(); page.evaluate("location.hash='/account'")
            expect(preview).not_to_be_visible()
            page.get_by_role("button", name="退出当前会话", exact=True).click()
            expect(page.get_by_role("button", name="登录", exact=True)).to_be_visible()
            release.set(); wait_for(finished.is_set, "logout response did not finish")
            page.wait_for_timeout(200); expect(page.get_by_role("dialog")).to_have_count(0)
            checks.append("logout/late-Office-result-cannot-restore-authenticated-UI")
            assert not errors and not violations and not external and not dialogs, (errors, violations, external, dialogs)
            assert not any(item.get("proxy_error") for item in requests), requests
            assert not any(item["method"] != "GET" and ("/operations" in item["path"] or "/uploads" in item["path"]) for item in requests)
            assert all((documents / name).read_bytes() == data for name, data in expected_disk.items())
            content = page.content()
            assert key.read_text().strip() not in content and OFFICE not in content
            assert all("/converter" not in item["path"] and "/cache/files/" not in item["path"] for item in requests)
            checks.append("no-external-Office-script-source-or-JWT/no-CSP-pageerrors-or-writes/files-unchanged")
            (evidence / "viewers.json").write_text(json.dumps(viewer_states, ensure_ascii=False, indent=2))
    except BaseException as cause:
        failure = f"{type(cause).__name__}: {cause}"
        raise
    finally:
        for release in holds:
            release.set()
        if proxy:
            proxy.shutdown(); proxy.server_close()
        service.terminate()
        try:
            service.wait(timeout=30)
        except subprocess.TimeoutExpired:
            service.kill(); service.wait(timeout=10)
        log.close()
        (evidence / "requests.json").write_text(json.dumps(requests, ensure_ascii=False, indent=2))
        (evidence / "result.json").write_text(json.dumps({"passed": failure is None, "failure": failure, "checks": checks,
            "binary_directory": str(BIN), "web_directory": str(WEB), "pageerrors": errors, "csp": violations,
            "external_requests": external, "dialogs": dialogs,
            "limitations": ["Chromium only; conversion is read-only PDF, not an interactive Office editor.",
                "PDF layout, pagination and font substitution are owned by OnlyOffice; no Chinese typography claim.",
                "OnlyOffice may repair malformed inputs; only fully checked PDF output or a real conversion error is accepted.",
                "Unavailable/corrupt/late-response UI paths use an explicitly armed isolated proxy; normal format cases use real OnlyOffice."]}, ensure_ascii=False, indent=2))
    print(f"S213 Office previews: {len(checks)} check groups passed; evidence: {evidence}", flush=True)


if __name__ == "__main__":
    main()
