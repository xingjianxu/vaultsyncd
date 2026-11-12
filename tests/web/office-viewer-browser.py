#!/usr/bin/env python3
"""S215: real Office -> OnlyOffice read-only viewer -> Chromium, in isolated btrfs state.

Run in the pinned web-tests devShell. VAULTSYNCD_TEST_BIN_DIR,
VAULTSYNCD_TEST_WEB_DIR, VAULTSYNCD_WEB_RUN_ROOT select built inputs/evidence.
VAULTSYNCD_TEST_OFFICE_FIXTURES must contain demo.docx/demo.xlsx/demo.pptx;
VAULTSYNCD_TEST_OFFICE_JWT_FILE points to a private test copy of the service key.
Both paths must be under /data/vaultsyncd-dev. The document service defaults
only to the separately deployed S212 endpoint; production vaultsyncd is untouched.
Explicit proxy faults affect only this run. The second account is seeded
OFFLINE after stopping this test server; no online database is modified directly.
"""
from contextlib import contextmanager
import io
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

from playwright.sync_api import expect, sync_playwright
from csp_support import field_label

REPO = Path(__file__).resolve().parents[2]
DEV = Path("/data/vaultsyncd-dev").resolve()
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug")).resolve()
WEB = Path(os.environ.get("VAULTSYNCD_TEST_WEB_DIR", REPO / "apps/web/dist")).resolve()
PARENT = Path(os.environ.get("VAULTSYNCD_WEB_RUN_ROOT", str(DEV))).resolve()
RUN = PARENT / f"S215-office-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"
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
    browser_tmp = DEV / "cache/tmp" / f"S215-{os.getpid()}"
    browser_tmp.mkdir(mode=0o700, parents=True)
    os.environ.update(TMPDIR=str(browser_tmp), XDG_CACHE_HOME=str(RUN / "cache"))
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    evidence, documents = RUN / "evidence", RUN / "vaults/documents"
    print(f"S215 artifacts: {RUN}", flush=True)
    documents.mkdir(); (documents / "child").mkdir()
    fixtures = {NAMES[extension]: (source / f"demo.{extension}").read_bytes() for extension in NAMES}
    assert all(fixtures.values()), "Office fixtures cannot be empty"
    # A real second sheet proves the direct viewer preserves workbook navigation.
    original_workbook = io.BytesIO(fixtures[NAMES["xlsx"]])
    multi_workbook = io.BytesIO()
    with zipfile.ZipFile(original_workbook) as src, zipfile.ZipFile(multi_workbook, "w", zipfile.ZIP_DEFLATED) as dst:
        for item in src.infolist():
            data = src.read(item.filename)
            if item.filename == "xl/workbook.xml":
                data = data.replace(b"</sheets>", '<sheet name="第二工作表" sheetId="215" state="visible" r:id="rId215"/></sheets>'.encode())
            elif item.filename == "xl/_rels/workbook.xml.rels":
                data = data.replace(b"</Relationships>", b'<Relationship Id="rId215" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet215.xml"/></Relationships>')
            elif item.filename == "[Content_Types].xml":
                data = data.replace(b"</Types>", b'<Override PartName="/xl/worksheets/sheet215.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>')
            dst.writestr(item, data)
        dst.writestr("xl/worksheets/sheet215.xml", '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>第二工作表只读验收内容 S215</t></is></c></row></sheetData></worksheet>')
    fixtures[NAMES["xlsx"]] = multi_workbook.getvalue()
    fixtures["损坏.docx"] = b"PK\x03\x04\x14\x00S215 truncated OOXML archive\x00\xff"
    expected_disk = dict(fixtures)
    for name, data in fixtures.items():
        (documents / name).write_bytes(data)
    (documents / "child/leaf.txt").write_text("S215 child file\n")
    (evidence / "inputs.json").write_text(json.dumps({
        "binary_directory": str(BIN), "web_directory": str(WEB), "document_service": OFFICE,
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
    service = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log, cwd=RUN, env={**os.environ, "VAULTSYNCD_WEB_DIR": str(WEB)})
    proxy = context = page = None
    requests, checks, errors, violations, external, dialogs, viewer_states = [], [], [], [], [], [], {}
    lock = threading.Lock()
    fault = {"mode": None, "entry_id": None}
    holds = []
    expected_violations = []
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

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                self.forward()

            def do_GET(self):
                self.forward()

            def do_DELETE(self):
                self.forward()

            def forward(self):
                backend = http.client.HTTPConnection(address, timeout=150)
                body = self.rfile.read(int(self.headers.get("content-length", "0")))
                headers = {k: v for k, v in self.headers.items() if k.lower() not in ("connection", "transfer-encoding")}
                path = urlsplit(self.path).path
                issued_session = None
                record = {"method": self.command, "path": self.path}
                with lock:
                    current = dict(fault); requests.append(record)
                office = path.endswith("/office-viewer")
                targeted = office and f"/entries/{current['entry_id']}/" in path
                if office:
                    record["office_request"] = json.loads(body)
                try:
                    if targeted and current["mode"] in ("503", "422"):
                        status = int(current["mode"])
                        payload = json.dumps({"error": {"code": "NEEDS_RECOVERY" if status == 503 else "INVALID_REQUEST", "message": "隔离转换服务暂不可用，请重试。" if status == 503 else "Office 查看服务失败（隔离故障夹具）。", "request_id": "S215-fault", "retryable": status == 503}}).encode()
                        response_headers = [("content-type", "application/json")]
                        record["fault"] = f"simulated-viewer-{status}"
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
                        assert key.read_bytes().strip() not in payload, "JWT secret leaked"
                        data = json.loads(payload)
                        issued_session = data["session_id"]
                        cfg = json.loads(data["config_json"])
                        # Only store non-sensitive invariants; signed config and capability URL stay in memory.
                        record["viewer"] = {"document_server_url": data["document_server_url"],
                            "file_type": cfg["document"]["fileType"], "mode": cfg["editorConfig"]["mode"],
                            "permissions": cfg["document"]["permissions"]}
                    if targeted and status == 200:
                        record["fault"] = current["mode"]
                        if current["mode"] == "wrong-source":
                            data = json.loads(payload)
                            cfg = json.loads(data["config_json"])
                            cfg["document"]["permissions"]["edit"] = True
                            data["config_json"] = json.dumps(cfg)
                            payload = json.dumps(data).encode()
                        elif current["mode"] == "delay":
                            current["started"].set()
                            assert current["release"].wait(60), "delayed viewer response not released"
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
                        # The deliberately lost creation response cannot reveal its session id to the UI.
                        # Revoke this fixture through the authenticated API so later cases do not consume its 120s slot.
                        if issued_session and not record.get("delivered"):
                            try:
                                backend.request("DELETE", path + "/" + issued_session, headers=headers)
                                cleanup = backend.getresponse(); cleanup.read()
                                record["fixture_cleanup_status"] = cleanup.status
                            except (ConnectionError, http.client.HTTPException):
                                pass
                        current["finished"].set()
                    backend.close()

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        proxy.daemon_threads = True
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        origin = f"http://localhost:{proxy.server_port}"
        with runtime() as playwright:
            context = playwright.chromium.launch_persistent_context(user_data_dir=str(RUN / "browser-profile"),
                downloads_path=str(RUN / "downloads"), executable_path=shutil.which("chromium"),
                headless=True, accept_downloads=True, viewport={"width": 1440, "height": 900})
            context.expose_binding("s215CspViolation", lambda _source, detail: violations.append(detail))
            context.add_init_script("document.addEventListener('securitypolicyviolation',e=>window.s215CspViolation({directive:e.effectiveDirective,blocked:e.blockedURI,document:e.documentURI.split('?')[0],source:e.sourceFile.split('?')[0],line:e.lineNumber,sample:e.sample}));")
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
            service = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log, cwd=RUN, env={**os.environ, "VAULTSYNCD_WEB_DIR": str(WEB)})
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
                return f"/v1/vaults/{vault}/entries/{entry['entry_id']}/versions/{entry['version_id']}/office-viewer"
            def payload(entry):
                return {"file_type": entry["name"].split(".")[-1].lower(), "hash": entry["hash"], "size": entry["size"]}
            original = by_name[NAMES["docx"]]
            assert api(endpoint(original), payload(original), allowed=(401,), anonymous=True)["body"]["error"]["code"] == "AUTH_EXPIRED"
            login = http.client.HTTPConnection(address, timeout=30)
            login.request("POST", "/v1/auth/login", body=json.dumps({"login": "intruder", "password": intruder_password, "device_label": "S215 offline-fixture account"}), headers={"content-type": "application/json"})
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
                preview.get_by_role("button", name="关闭", exact=True).click()
                expect(preview).not_to_be_visible()
            def native_office():
                holder = preview.locator(".file-preview-office")
                expect(holder).to_be_visible(timeout=30000)
                expect(holder.locator("iframe")).to_have_count(1, timeout=30000)
                frame = wait_for(lambda: next((f for f in page.frames if urlsplit(f.url).netloc == urlsplit(OFFICE).netloc and "/web-apps/" in urlsplit(f.url).path and f.locator("canvas").count()), None), "OnlyOffice document canvas did not load")
                wait_for(lambda: holder.get_attribute("aria-busy") == "false", "OnlyOffice onDocumentReady was not reported")
                expect(preview.locator("iframe.file-preview-pdf,object,embed")).to_have_count(0)
                assert "/web-apps/" in frame.url and not frame.url.startswith("blob:")
                # Real editor canvas and its view URL are independent of the application's configuration checks.
                state = frame.evaluate("""()=>({title:document.title,canvases:document.querySelectorAll('canvas').length,
                    editable:typeof Common!=='undefined'&&Common.Controllers?.Viewport?.mode?.isEdit,
                    bodyText:document.body.innerText.slice(0,4000)})""")
                assert state["canvases"] > 0 and "mode=view" in frame.url, state
                return frame, state
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
                frame, viewer_states[extension] = native_office()
                assert page.url == before_url
                conversion = [item for item in requests[start:] if item["path"].endswith("/office-viewer")]
                assert len(conversion) == 1 and conversion[0]["status"] == 200, conversion
                record = conversion[0]
                assert set(record["office_request"]) == {"file_type", "hash", "size"}
                assert key.read_text().strip() not in page.content()
                assert record["viewer"]["mode"] == "view"
                assert all(record["viewer"]["permissions"].get(flag) is False for flag in ("edit", "review", "comment", "fillForms"))
                if extension == "xlsx":
                    first = frame.locator('#statusbar_bottom > li[data-label="Sheet1"]')
                    second = frame.locator('#statusbar_bottom > li[data-label="第二工作表"]')
                    expect(first).to_be_visible(timeout=30000); expect(second).to_be_visible(timeout=30000)
                    assert frame.evaluate("Asc.editor.asc_getWorksheetsCount()") == 2
                    assert frame.evaluate("Asc.editor.asc_getActiveWorksheetIndex()") == 0
                    boxes = {"first": first.bounding_box(), "second": second.bounding_box(), "label": second.locator("span").bounding_box()}
                    # Target the actual tab LI, whose inline label's union rectangle can extend beyond its visible hit area.
                    second.click(position={"x": 5, "y": 5})
                    expect(second).to_have_class(re.compile("active"), timeout=15000)
                    wait_for(lambda: frame.evaluate("Asc.editor.asc_getActiveWorksheetIndex()") == 1, "UI second-sheet click did not activate the second worksheet")
                    page.screenshot(path=str(evidence / "office-xlsx-second-sheet.png"))
                    first.click(position={"x": 5, "y": 5})
                    expect(first).to_have_class(re.compile("active"), timeout=15000)
                    wait_for(lambda: frame.evaluate("Asc.editor.asc_getActiveWorksheetIndex()") == 0, "UI first-sheet click did not restore the first worksheet")
                    viewer_states[extension]["sheet_tabs"] = {"names": [first.inner_text(), second.inner_text()], "active_indices": [0,1,0], "bounds": boxes}
                download_exact(name)
                page.screenshot(path=str(evidence / f"office-{extension}.png"))
                close_preview()
                expect(preview.locator(".file-preview-office")).to_have_count(0)
            checks.append("DOCX-XLSX-PPTX/real-OnlyOffice-readonly-canvas/XLSX-two-interactive-sheets/exact-original-download")

            # The deployment permits only ONLYOFFICE's empty onmousewheel compatibility attribute.
            # A nonempty attribute remains blocked by CSP and cannot run in the authenticated page.
            probe_start = len(violations)
            page.evaluate("""()=>{
                window.__s215InlineRan=false;const probe=document.createElement('button');probe.id='s215-inline-probe';
                probe.setAttribute('onclick','window.__s215InlineRan=true');document.body.appendChild(probe);probe.click();probe.remove();
            }""")
            wait_for(lambda: len(violations) > probe_start, "nonempty inline event attribute was not rejected")
            assert page.evaluate("window.__s215InlineRan") is False
            probes = violations[probe_start:]
            assert all(item["directive"] == "script-src-attr" and item["blocked"] == "inline" for item in probes), probes
            expected_violations.extend(probes); del violations[probe_start:]
            checks.append("precise-empty-handler-compatibility/nonempty-inline-handler-still-CSP-blocked")

            # A valid source with an invalid signature must be rejected by the real Document Server.
            token_session = api(endpoint(original), payload(original))["body"]
            token_config = json.loads(token_session["config_json"])
            token_parts = token_config["token"].split(".")
            token_parts[2] = ("A" if token_parts[2][0] != "A" else "B") + token_parts[2][1:]
            token_config["token"] = ".".join(token_parts)
            page.evaluate("""config=>{
                const node=document.createElement('div');node.id='s215-tampered';document.body.appendChild(node);
                window.s215TamperedResult={ready:false,error:null};
                window.s215TamperedEditor=new DocsAPI.DocEditor(node.id,{...config,events:{
                    onDocumentReady(){window.s215TamperedResult.ready=true},
                    onError(e){window.s215TamperedResult.error={errorCode:e.data.errorCode,errorDescription:e.data.errorDescription}}
                }});
            }""", token_config)
            wait_for(lambda: page.evaluate("window.s215TamperedResult.error"), "Document Server did not reject invalid JWT signature")
            token_result = page.evaluate("window.s215TamperedResult")
            assert not token_result["ready"] and token_result["error"]["errorCode"], token_result
            page.evaluate("window.s215TamperedEditor.destroyEditor();document.getElementById('s215-tampered')?.remove()")
            status = page.evaluate("async path=>(await fetch(path,{method:'DELETE'})).status", endpoint(original) + "/" + token_session["session_id"])
            assert status == 200, status
            token_config.clear(); token_session.clear()
            viewer_states["invalid_jwt"] = token_result
            checks.append("real-OnlyOffice-JWT-invalid-signature/rejected-before-document-ready")

            # Plain configuration cannot override the signed read-only permissions.
            escalation_session = api(endpoint(original), payload(original))["body"]
            escalation_config = json.loads(escalation_session["config_json"])
            escalation_config["document"]["permissions"]["edit"] = True
            escalation_config["editorConfig"]["mode"] = "edit"
            page.evaluate("""config=>{
                const node=document.createElement('div');node.id='s215-escalation';document.body.appendChild(node);
                window.s215EscalationResult={ready:false,error:null};
                window.s215EscalationEditor=new DocsAPI.DocEditor(node.id,{...config,events:{
                    onDocumentReady(){window.s215EscalationResult.ready=true},
                    onError(e){window.s215EscalationResult.error={errorCode:e.data.errorCode,errorDescription:e.data.errorDescription}}
                }});
            }""", escalation_config)
            wait_for(lambda: page.evaluate("window.s215EscalationResult.error||window.s215EscalationResult.ready"), "Document Server did not finish permission-escalation test")
            escalation_result = page.evaluate("window.s215EscalationResult")
            assert not escalation_result["ready"] and escalation_result["error"]["errorCode"], escalation_result
            page.evaluate("window.s215EscalationEditor.destroyEditor();document.getElementById('s215-escalation')?.remove()")
            status = page.evaluate("async path=>(await fetch(path,{method:'DELETE'})).status", endpoint(original) + "/" + escalation_session["session_id"])
            assert status == 200, status
            escalation_config.clear(); escalation_session.clear()
            viewer_states["permission_escalation"] = escalation_result
            checks.append("real-OnlyOffice-signed-permissions/plain-edit-and-mode-tampering-rejected")

            # Change a valid OOXML file through the real observer, then ask for
            # the previous snapshot version. A fallback to current head would
            # mismatch the original hash; success proves the old version is used.
            with zipfile.ZipFile(documents / NAMES["docx"], "a") as archive:
                archive.comment = b"S215 second committed immutable source version"
            expected_disk[NAMES["docx"]] = (documents / NAMES["docx"]).read_bytes()
            changed = wait_for(lambda: next((item for item in snapshot_entries() if item["entry_id"] == original["entry_id"] and item["version_id"] != original["version_id"]), None), "new Office source version not committed")
            assert changed["hash"] == sha(expected_disk[NAMES["docx"]]) and changed["hash"] != original["hash"]
            old_session = api(endpoint(original), payload(original))["body"]
            old_source = urlsplit(json.loads(old_session["config_json"])["document"]["url"])
            source_connection = http.client.HTTPConnection(old_source.hostname, old_source.port, timeout=30)
            source_connection.request("GET", old_source.path)
            source_reply = source_connection.getresponse()
            assert source_reply.status == 200 and source_reply.read() == fixtures[NAMES["docx"]]
            source_connection.close()
            value = page.evaluate("""async path=>{let r=await fetch(path,{method:'DELETE'});return r.status}""", endpoint(original) + "/" + old_session["session_id"])
            assert value == 200, value
            # Cancellation closes the fixed-version source capability, including its listening socket.
            def source_closed():
                connection = http.client.HTTPConnection(old_source.hostname, old_source.port, timeout=2)
                try:
                    connection.request("GET", old_source.path)
                    reply = connection.getresponse(); reply.read()
                    return reply.status in (401,403,410)
                except (ConnectionRefusedError, ConnectionResetError, http.client.RemoteDisconnected):
                    return True
                finally:
                    connection.close()
            wait_for(source_closed, "closed viewer source remains readable")
            old_session.clear()
            checks.append("real-new-head/old-snapshot-viewer-serves-exact-old-bytes/no-head-fallback/explicit-close")
            for mode, message in (("503", "暂不可用"), ("422", "查看服务失败"), ("wrong-source", "只读")):
                arm(mode, by_name[NAMES["xlsx"]]["entry_id"])
                open_preview(NAMES["xlsx"])
                expect(preview.get_by_role("alert")).to_contain_text(message, timeout=120000)
                expect(preview.locator("iframe")).to_have_count(0)
                arm()
                preview.get_by_role("button", name="重试预览", exact=True).click(); native_office(); close_preview()
            checks.append("isolated-proxy-503-422/tampered-permission-config/no-viewer/retry-real-viewer")

            arm("oversize", by_name[NAMES["xlsx"]]["entry_id"])
            page.reload(); start = len(requests)
            open_preview(NAMES["xlsx"])
            expect(preview.get_by_role("alert")).to_contain_text("50 MiB")
            assert not any(f"/entries/{by_name[NAMES['xlsx']]['entry_id']}/" in item["path"] and item["path"].endswith(("/content", "/office-viewer")) for item in requests[start:])
            close_preview(); arm(); page.reload()
            checks.append("oversize-snapshot/rejected-before-content-or-viewer-request")

            def delayed():
                events = arm("delay", by_name[NAMES["xlsx"]]["entry_id"])
                open_preview(NAMES["xlsx"])
                wait_for(events[0].is_set, "viewer response was not held")
                return events
            _, release, finished = delayed()
            preview.get_by_role("button", name="取消预览", exact=True).click()
            expect(preview.get_by_role("alert")).to_contain_text("已取消")
            expect(preview.locator("iframe")).to_have_count(0)
            close_preview(); arm(); open_preview(NAMES["pptx"]); native_office()
            release.set(); wait_for(finished.is_set, "late viewer response did not finish")
            expect(page.get_by_role("dialog", name=f"预览 · {NAMES['pptx']}", exact=True)).to_be_visible()
            close_preview()
            checks.append("cancel-close/late-viewer-cannot-replace-new-preview")
            _, release, finished = delayed()
            arm(); page.evaluate("path=>location.hash=path", f"/files/{vault}/child")
            expect(preview).not_to_be_visible()
            expect(link("leaf.txt")).to_be_visible(timeout=20000)
            release.set(); wait_for(finished.is_set, "navigation response did not finish")
            page.evaluate("path=>location.hash=path", f"/files/{vault}")
            checks.append("navigation/closes-and-cancels-inflight-viewer")
            open_preview(NAMES["docx"]); native_office()
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
            unexpected_external = [url for url in external if urlsplit(url).netloc != urlsplit(OFFICE).netloc]
            assert not errors and not violations and not unexpected_external and not dialogs, (errors, violations, unexpected_external, dialogs)
            assert not any(item.get("proxy_error") for item in requests), requests
            assert not any(item["method"] != "GET" and ("/operations" in item["path"] or "/uploads" in item["path"]) for item in requests)
            assert all((documents / name).read_bytes() == data for name, data in expected_disk.items())
            content = page.content()
            assert key.read_text().strip() not in content
            assert all("/converter" not in item["path"] and "/cache/files/" not in item["path"] for item in requests)
            checks.append("only-configured-OnlyOffice-origin/no-CSP-pageerrors-or-writes/files-unchanged")
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
            "binary_directory": str(BIN), "web_directory": str(WEB), "pageerrors": errors, "csp": violations, "expected_csp_probe": expected_violations,
            "external_origins": sorted({urlsplit(url).scheme+"://"+urlsplit(url).netloc for url in external}), "dialogs": dialogs,
            "limitations": ["Chromium only; OnlyOffice renderer, layout and fonts remain third-party behavior.",
                "Read-only preview cannot revoke bytes already received or OnlyOffice internal caches.",
                "No native Microsoft Office comparison or Chinese typography claim.",
                "Unavailable/tampered/late-response UI paths use an explicitly armed isolated proxy; normal format cases use real OnlyOffice."]}, ensure_ascii=False, indent=2))
    print(f"S215 Office previews: {len(checks)} check groups passed; evidence: {evidence}", flush=True)


if __name__ == "__main__":
    main()
