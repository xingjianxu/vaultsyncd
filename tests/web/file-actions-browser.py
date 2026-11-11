#!/usr/bin/env python3
"""S136/S137: built Web + real Chromium + isolated btrfs service, not API fixtures.

After the main agent's final Web build, run from this worktree:
  VAULTSYNCD_TEST_BIN_DIR=/wrd/prj/vaultsyncd/target/debug \
    nix develop .#web-tests --command python3 tests/web/file-actions-browser.py

The binary directory can be overridden; static assets always come from this
worktree's apps/web/dist. Artifacts, screenshots, server state and input bytes
stay under /data/vaultsyncd-dev/vaultsyncd-temp/vs-s137-<runid>. No online SQL, production writes, fabricated
operation responses, or application/transport mocks. A scoped same-origin proxy
can hold one real request/response or discard one real submit response.
"""
from contextlib import contextmanager
import functools
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import threading
import time
from urllib.parse import quote, urlsplit

from playwright.sync_api import TimeoutError as PlaywrightTimeoutError, expect, sync_playwright
from csp_support import CspStaticFiles, field_label

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug"))
RUN = Path(os.environ.get("VAULTSYNCD_WEB_RUN_ROOT", "/data/vaultsyncd-dev/vaultsyncd-temp")) / f"vs-s137-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


def checked(args, **kwargs):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, **kwargs)
    assert result.returncode == 0, f"{args[0]} failed: {result.stderr}"
    return result.stdout


def sha(data):
    return hashlib.sha256(data).hexdigest()


def main():
    for name in ("vaultsyncd-admin", "vaultsyncd-server"):
        assert os.access(BIN / name, os.X_OK), f"missing executable: {BIN / name}"
    assert (REPO / "apps/web/dist/index.html").is_file(), "build this worktree's Web first"
    checked(["sudo", "-n", "mkdir", "-m", "755", RUN])
    checked(["sudo", "-n", "chown", f"{os.getuid()}:{os.getgid()}", RUN])
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    print(f"S136/S137 artifacts: {RUN}", flush=True)
    state, vaults, inputs = RUN / "state", RUN / "vaults", RUN / "inputs"
    state.mkdir(mode=0o700); vaults.mkdir(); inputs.mkdir()
    documents, archive = vaults / "documents", vaults / "archive"
    documents.mkdir(); archive.mkdir()
    (documents / "source/child").mkdir(parents=True)
    (documents / "destination").mkdir()
    (documents / "s139.folder").mkdir()
    fixtures = {
        "source/keep & 文档.txt": b"S136 original, never overwrite\n",
        "source/child/deep.txt": b"S136 deep child\n",
        "destination/anchor.txt": b"S136 destination anchor\n",
        "cancel.txt": b"S136 cancelled delete must survive\n",
        "delete.txt": b"S136 recoverable deletion\n",
        "stale.txt": b"S136 newer namespace must survive stale delete\n",
        "s139.folder/keep.txt": b"S139 nonempty dotted directory\n",
        "s139.tar.gz": b"S139 multi-dot extension\n",
        ".s139-hidden": b"S139 dotfile without extension\n",
        ".s139-hidden.txt": b"S139 dotfile with extension\n",
        "s139-no-extension": b"S139 extensionless file\n",
        "s139-文档😀.txt": b"S139 Unicode filename\n",
    }
    for name, data in fixtures.items(): (documents / name).write_bytes(data)
    (archive / "archive-only.txt").write_bytes(b"S136 unrelated vault\n")
    upload_data = {
        "upload & '文档'.txt": b"S136 multi-file upload\n",
        "empty-upload.dat": b"",
        "cross-chunk.bin": bytes(range(256)) * (5 * 1024 * 1024 // 256) + b"S136 final part\n",
    }
    for name, data in upload_data.items(): (inputs / name).write_bytes(data)
    password = secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", state, "--login", "owner"], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[vaults]\nallowed_bases=["{vaults}"]\n[log]\nformat="json"\nlevel="info"\n')
    log = (RUN / "server.log").open("w")
    service = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log)
    proxy = browser = context = page = None
    requests, checks, errors, csp = [], [], [], []
    lock = threading.Lock()
    fault = {"mode": None, "vault": None, "kind": None, "record": None, "corrupt_download": False}
    triggered, release = threading.Event(), threading.Event()

    def save_result(passed, failure=None):
        (RUN / "result.json").write_text(json.dumps({
            "passed": passed, "failure": failure, "checks": checks,
            "binary_directory": str(BIN), "web_dist": str(REPO / "apps/web/dist"),
            "requests": requests, "page_errors": errors, "csp": csp,
            "limitations": ["No fabricated 202: delayed/unknown real submissions are tested; a deterministic server-side pending state is not injected."],
        }, ensure_ascii=False, indent=2))

    @contextmanager
    def browser_runtime():
        # Capture while Playwright is alive; an outer exception handler runs
        # after sync_playwright has already closed its event loop.
        with sync_playwright() as playwright:
            try:
                yield playwright
            except Exception:
                if page and not page.is_closed():
                    try:
                        csp.extend(page.evaluate("window.__csp || []"))
                        page.screenshot(path=str(RUN / "failure.png"), full_page=True)
                        (RUN / "failure.html").write_text(page.content())
                    except Exception as capture_error:
                        errors.append(f"failure artifact capture: {capture_error}")
                raise

    try:
        address = None
        for _ in range(150):
            for line in (RUN / "server.log").read_text().splitlines():
                fields = json.loads(line).get("fields", {})
                if fields.get("message") == "服务端开始监听": address = fields["listen"]
            if address: break
            assert service.poll() is None, "service exited"
            time.sleep(.2)
        assert address, "service did not listen"

        class Handler(CspStaticFiles):
            def log_message(self, *_): pass

            def do_POST(self): self.forward()

            def do_PUT(self): self.forward()  # Required for real multipart uploads.

            def do_GET(self):
                if self.path.startswith("/v1/"): self.forward()
                else: super().do_GET()

            def forward(self):
                backend = http.client.HTTPConnection(address, timeout=90)
                body = self.rfile.read(int(self.headers.get("content-length", "0")))
                probe = self.headers.get("x-s136-probe") == "1"
                path = urlsplit(self.path).path
                is_submit = self.command == "POST" and path.endswith("/operations")
                command = json.loads(body) if is_submit else None
                record = {"path": self.path, "method": self.command, "source": "probe" if probe else "ui"}
                # Never retain credentials, cookies, CSRF values, or upload bytes.
                if command: record["operation"] = command
                mode = None
                with lock:
                    requests.append(record)
                    if not probe and is_submit and fault["mode"] and f"/vaults/{fault['vault']}/" in path and command["kind"] == fault["kind"]:
                        mode = fault["mode"]
                        fault.update(mode=None, record=record)
                        record["fault"] = mode
                headers = {key: value for key, value in self.headers.items() if key.lower() not in ("connection", "transfer-encoding", "x-s136-probe")}
                try:
                    if mode == "hold-before":
                        triggered.set()
                        assert release.wait(timeout=60), "held submit was not released"
                    backend.request(self.command, self.path, body=body, headers=headers)
                    response = backend.getresponse(); payload = response.read()
                    record["status"] = response.status
                    if is_submit or response.status >= 400 or (self.command == "POST" and path.endswith("/uploads")):
                        record["result"] = json.loads(payload)
                    if mode == "hold-after":
                        triggered.set()
                        assert release.wait(timeout=60), "held response was not released"
                    if mode == "drop-after":
                        record["response_dropped"] = True
                        triggered.set()
                        self.close_connection = True
                        return  # Backend really processed it; Chromium receives no success.
                    if path.endswith("/content") and fault["corrupt_download"] and payload:
                        payload = payload[:-1] + bytes([payload[-1] ^ 1])
                        record["corrupted_download"] = True
                    self.send_response(response.status)
                    for key, value in response.getheaders():
                        if key.lower() not in ("connection", "transfer-encoding", "content-length"):
                            self.send_header(key, value)
                    self.send_header("content-length", str(len(payload))); self.end_headers()
                    self.wfile.write(payload)
                    record["delivered"] = True
                except (BrokenPipeError, ConnectionResetError):
                    record["browser_cancelled"] = True
                except Exception as cause:
                    record["proxy_error"] = str(cause)
                    self.close_connection = True
                finally: backend.close()

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=str(REPO / "apps/web/dist")))
        proxy.daemon_threads = True
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        origin = f"http://localhost:{proxy.server_port}"
        with browser_runtime() as playwright:
            browser = playwright.chromium.launch(headless=True)
            context = browser.new_context(viewport={"width": 1440, "height": 1000})
            page = context.new_page()
            page.on("pageerror", lambda error: errors.append(str(error)))
            context.expose_binding("s136CspViolation", lambda _source, directive: csp.append(directive))
            context.add_init_script("window.__csp=[];document.addEventListener('securitypolicyviolation',e=>{window.__csp.push(e.effectiveDirective);window.s136CspViolation(e.effectiveDirective)});")
            page.goto(origin + "/#/files")
            expect(page.get_by_label(field_label("登录名"))).to_be_visible()
            expect(page.get_by_role("navigation", name="主要导航")).to_have_count(0)
            page.get_by_label(field_label("登录名")).fill("owner")
            page.get_by_label(field_label("密码")).fill(password)
            page.get_by_role("button", name="登录", exact=True).click()
            expect(page.get_by_role("navigation", name="主要导航")).to_be_visible()
            assert page.url == origin + "/#/files"

            def api(path, body=None):
                result = page.evaluate("""async ([path,body]) => {
                  const r=await fetch(path,{method:body===null?'GET':'POST',headers:{'x-s136-probe':'1',...(body===null?{}:{'content-type':'application/json'})},...(body===null?{}:{body:JSON.stringify(body)})});
                  return {status:r.status,body:await r.json()};
                }""", [path, body])
                assert result["status"] in (200, 201, 202), result
                return result["body"]

            def wait_for(predicate, message):
                for _ in range(180):
                    result = predicate()
                    if result: return result
                    page.wait_for_timeout(200)
                raise AssertionError(message)

            doc_id = api("/v1/admin/vaults", {"path": str(documents)})["vault_id"]
            arc_id = api("/v1/admin/vaults", {"path": str(archive)})["vault_id"]
            wait_for(lambda: all(item["collection"]["state"] == "complete" for item in api("/v1/admin/vaults")["vaults"]), "real observer import did not complete")

            def snapshot(vault_id=doc_id):
                metadata = api(f"/v1/vaults/{vault_id}/snapshots", {})
                entries, cursor = [], None
                while True:
                    batch = api(f"/v1/vaults/{vault_id}/snapshots/{metadata['snapshot_id']}/entries?limit=1000" + (f"&cursor={cursor}" if cursor else ""))
                    entries.extend(batch["entries"])
                    cursor = batch.get("next_cursor")
                    if cursor is None: break
                return metadata, entries

            def entry_at(path, vault_id=doc_id):
                _, entries = snapshot(vault_id)
                current = next(item for item in entries if "parent_id" not in item)
                for part in path.strip("/").split("/") if path.strip("/") else []:
                    current = next((item for item in entries if item.get("parent_id") == current["entry_id"] and item["name"] == part), None)
                    if current is None: return None
                return current

            def command(kind, **fields):
                epoch = snapshot()[0]["server_epoch"]
                result = api(f"/v1/vaults/{doc_id}/operations", {"kind": kind, "op_id": secrets.token_hex(16), "device_id": secrets.token_hex(16), "server_epoch": epoch, **fields})
                if result["state"] == "pending":
                    result = wait_for(lambda: terminal(result["op_id"]), "probe operation remained pending")
                assert result["state"] == "committed", result
                return result

            def terminal(op_id):
                result = api(f"/v1/vaults/{doc_id}/operations/{op_id}")
                return result if result["state"] != "pending" else None

            # The observer intentionally does not import external empty directories.
            source = entry_at("source")
            empty = command("create", parent_id=source["entry_id"], name="empty", expected_absent=True)
            checks.append("real-observer/small-fixture/public-api-empty-directory")
            page.goto(origin + f"/#/files/{doc_id}")
            table = page.get_by_role("table", name="文件与目录", exact=True)
            toolbar = page.get_by_role("toolbar", name="文件浏览器工具栏", exact=True)
            notifications = page.locator(".mantine-Notification-root")
            error_notices = page.locator('.mantine-Notification-root[role="alert"]')

            def row(name):
                return table.get_by_role("button", name=name, exact=True).locator("xpath=ancestor::tr[1]")

            def at(path="/", vault_id=doc_id):
                if f"/#/files/{vault_id}" not in page.url:
                    page.get_by_role("combobox", name="浏览 Vault", exact=True).click()
                    name = "documents" if vault_id == doc_id else "archive"
                    page.get_by_role("option", name=f"{name} · {vault_id[:8]}", exact=True).click()
                    expect(table).to_be_visible(timeout=15000)
                address_input = page.get_by_role("textbox", name="目录地址", exact=True)
                address_input.fill(path); address_input.press("Enter")
                encoded = "/".join(quote(part, safe="") for part in path.strip("/").split("/") if part)
                expect(page).to_have_url(origin + f"/#/files/{vault_id}" + ("/" + encoded if encoded else ""))
                expect(address_input).to_have_value(path)
                expect(table).to_be_visible(timeout=15000)
                clear_notices()

            def refresh():
                clear_notices()
                with page.expect_response(lambda response: "/snapshots/" in response.url and "/entries" in response.url and response.status == 200):
                    toolbar.get_by_role("button", name="刷新文件列表", exact=True).click()
                expect(table).to_be_visible(timeout=15000)

            def menu_action(name, action):
                clear_notices()
                row(name).click(button="right")
                menu = page.get_by_role("menu")
                expect(menu).to_be_visible()
                menu.get_by_role("menuitem", name=action, exact=True).click()
                expect(menu).not_to_be_visible()

            def clear_notices():
                # Closing a visible notice can promote queued notices; drain the
                # real notification area instead of leaving an error over toolbar.
                for _ in range(20):
                    if not notifications.count(): return
                    # Pin this notice: an expiring first item must not make the
                    # click wait forever on a now-empty dynamic first locator.
                    notice_id = notifications.first.get_attribute("id")
                    notice = page.locator(f'[id="{notice_id}"]')
                    try:
                        notice.get_by_role("button", name="关闭通知", exact=True).click(timeout=1500)
                    except PlaywrightTimeoutError:
                        if notice.is_visible(): raise
                    page.wait_for_timeout(80)
                expect(notifications).to_have_count(0)

            def end_rejected():
                # Only a definitive rejection can unlock this control. Unknown
                # operations must remain recoverable instead of being re-created.
                end = page.get_by_role("button", name="结束操作", exact=True)
                expect(end).to_be_visible(timeout=15000)
                expect(end).to_be_enabled()
                clear_notices()
                start = len(requests)
                end.click()
                expect(end).to_have_count(0)
                expect(toolbar.get_by_role("button", name="上传文件", exact=True)).to_be_enabled()
                assert not submits_since(start), "ending a rejected job submitted a mutation"

            def upload(paths):
                expect(toolbar.get_by_role("button", name="上传文件", exact=True).locator("svg.lucide-upload")).to_have_count(1)
                expect(toolbar.get_by_role("button", name="上传文件", exact=True)).to_have_attribute("title", "上传文件")
                clear_notices()
                with page.expect_file_chooser() as chooser:
                    toolbar.get_by_role("button", name="上传文件", exact=True).click()
                assert chooser.value.is_multiple(), "upload must support selecting multiple files"
                chooser.value.set_files([str(path) for path in paths])

            def settled_entry(path, vault_id=doc_id):
                item = wait_for(lambda: entry_at(path, vault_id), f"entry never committed: {path}")
                expect(table.get_by_role("button", name=Path(path).name, exact=True)).to_be_visible(timeout=30000)
                return item

            def arm(mode, kind):
                triggered.clear(); release.clear()
                with lock: fault.update(mode=mode, vault=doc_id, kind=kind, record=None)

            def await_fault():
                wait_for(triggered.is_set, "UI did not reach the armed real operation")
                return fault["record"]

            def submits_since(start):
                return [item for item in requests[start:] if item["source"] == "ui" and "operation" in item]

            def assert_bytes(path, data):
                wait_for(lambda: path.is_file(), f"published file missing: {path}")
                assert path.read_bytes() == data, f"changed bytes: {path}"

            expect(table).to_be_visible(timeout=15000)
            expect(page.locator('input[type="file"][multiple]')).to_have_count(1)
            start = len(requests)
            upload([inputs / name for name in upload_data])
            uploaded = {name: settled_entry(name) for name in upload_data}
            for name, data in upload_data.items():
                assert_bytes(documents / name, data)
                assert uploaded[name]["hash"] == sha(data) and uploaded[name]["size"] == str(len(data)), uploaded[name]
            # 5 MiB is deliberately larger than the Web uploader's chunk size.
            sessions = [item["result"]["upload_id"] for item in requests[start:] if item["source"] == "ui" and item["method"] == "POST" and urlsplit(item["path"]).path.endswith("/uploads") and "result" in item]
            parts = {session: [item for item in requests[start:] if item["source"] == "ui" and item["method"] == "PUT" and f"/uploads/{session}/parts/" in item["path"]] for session in sessions}
            assert len(sessions) == 3 and any(len(items) >= 2 for items in parts.values()), parts
            assert all(item["status"] in (200, 201, 204) for items in parts.values() for item in items), parts
            checks.append("I01/I05/multi-upload/empty-bytes/cross-chunk/real-PUT/hash-size-disk")

            # S171: 新建目录使用真实create与恢复流程，目标取右键目录而非当前目录。
            def create_dialog(name):
                dialog = page.get_by_role("dialog", name="新建目录", exact=True)
                expect(dialog).to_be_visible()
                dialog.get_by_role("textbox", name=field_label("目录名称")).fill(name)
                return dialog

            background = page.get_by_label("当前目录空白区域", exact=True)
            background.focus(); background.press("Shift+F10")
            page.get_by_role("menuitem", name="新建目录", exact=True).click()
            dialog = create_dialog("cancelled-s171")
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({"width": width, "height": 900}); page.wait_for_timeout(300)
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width, "create dialog")
                    page.screenshot(path=str(RUN / f"s171-create-{scheme}-{width}.png"), full_page=True)
            page.emulate_media(color_scheme="light")
            page.set_viewport_size({"width":1440,"height":1000})
            start = len(requests); dialog.get_by_role("button", name="取消", exact=True).click()
            assert not submits_since(start) and entry_at("cancelled-s171") is None
            background.focus(); background.press("Shift+F10")
            page.get_by_role("menuitem", name="新建目录", exact=True).click()
            dialog = create_dialog("bad/name")
            dialog.get_by_role("button", name="创建", exact=True).click()
            expect(dialog.get_by_text("文件名无效。", exact=True)).to_be_visible()
            dialog.get_by_role("textbox", name=field_label("目录名称")).fill("S171 新目录 😀")
            dialog.get_by_role("textbox", name=field_label("目录名称")).press("Enter")
            root_created = settled_entry("S171 新目录 😀")
            assert root_created["kind"] == "directory" and (documents / "S171 新目录 😀").is_dir()
            menu_action("S171 新目录 😀", "新建目录")
            dialog = create_dialog("inside")
            dialog.get_by_role("button", name="创建", exact=True).click()
            wait_for(lambda: entry_at("S171 新目录 😀/inside"), "new directory target was not honored")
            assert entry_at("inside") is None
            clear_notices()
            tree = page.get_by_role("tree", name="目录树", exact=True)
            # 根节点同样允许新建，不允许改动Vault本身。
            tree.locator("[role=treeitem]").first.locator(":scope > div").click(button="right")
            page.get_by_role("menuitem", name="新建目录", exact=True).click()
            dialog = create_dialog("S171 新目录 😀")
            dialog.get_by_role("button", name="创建", exact=True).click()
            expect(dialog.get_by_text(re.compile("目标已存在同名条目"))).to_be_visible()
            dialog.get_by_role("button", name="取消", exact=True).click()
            checks.append("S171/create/cancel/invalid/Unicode/background-keyboard/row-directory-target/tree-root/duplicate")

            # 丢真实create响应后只能核对原OpId，不重复创建目录。
            background.focus(); background.press("Shift+F10")
            page.get_by_role("menuitem", name="新建目录", exact=True).click()
            dialog = create_dialog("s171-lost-response")
            arm("drop-after", "create")
            dialog.get_by_role("button", name="创建", exact=True).click()
            original = await_fault()
            release.set()
            expect(page.get_by_role("button", name="继续操作", exact=True)).to_be_visible(timeout=15000)
            clear_notices()
            page.get_by_role("button", name="继续操作", exact=True).click()
            settled_entry("s171-lost-response")
            assert len([r for r in requests if r.get("operation", {}).get("op_id") == original["operation"]["op_id"]]) == 1
            checks.append("S171/create/lost-real-response/query-original-opid")

            # 实际浏览器另存，原始名称、空内容与跨4MiB分段字节/摘要一致。
            for name, data in upload_data.items():
                with page.expect_download() as pending_download:
                    menu_action(name, "下载")
                saved = pending_download.value
                assert saved.suggested_filename == name and saved.failure() is None
                target = RUN / "downloads" / name
                target.parent.mkdir(exist_ok=True)
                saved.save_as(target)
                assert target.read_bytes() == data and sha(target.read_bytes()) == sha(data)
                expect(page.get_by_role("button", name="取消下载", exact=True)).to_have_count(0)
            downloads_seen = []
            page.on("download", lambda download: downloads_seen.append(download.suggested_filename))
            fault["corrupt_download"] = True
            menu_action("cross-chunk.bin", "下载")
            expect(error_notices.get_by_text("下载 SHA-256 校验失败，未保存文件。", exact=True)).to_be_visible(timeout=15000)
            assert not downloads_seen, "corrupt content reached browser save"
            fault["corrupt_download"] = False
            clear_notices()
            checks.append("S171/download/corrupted-real-range/hash-error/no-save")
            row("S171 新目录 😀").click(button="right")
            expect(page.get_by_role("menuitem", name="下载", exact=True)).to_have_count(0)
            page.keyboard.press("Escape")
            checks.append("S171/download/original-Unicode-name/empty/multiple-ranges/SHA256/no-directory-download")

            keep_name = "keep & 文档.txt"
            at("/source")
            row(keep_name).click(button="right")
            menu = page.get_by_role("menu")
            for action in ("复制", "剪切", "粘贴", "删除"):
                expect(menu.get_by_role("menuitem", name=action, exact=True)).to_have_count(1)
            menu.get_by_role("menuitem", name="复制", exact=True).click()
            at("/destination")
            toolbar.get_by_role("button", name="粘贴", exact=True).click()
            copied = settled_entry("destination/" + keep_name)
            assert copied["entry_id"] != entry_at("source/" + keep_name)["entry_id"]
            assert_bytes(documents / "destination" / keep_name, fixtures["source/" + keep_name])
            # Same-directory duplicate may be safely rejected or get a new name;
            # the existing source and destination identities/bytes must not change.
            menu_action(keep_name, "复制")
            baseline = snapshot()[1]
            baseline_ids = {item["entry_id"] for item in baseline}
            clear_notices(); start = len(requests)
            toolbar.get_by_role("button", name="粘贴", exact=True).click()
            wait_for(lambda: notifications.count() > 0, "same-name copy gave no feedback")
            wait_for(lambda: toolbar.get_by_role("button", name="上传文件", exact=True).is_enabled(), "copy never settled")
            after = snapshot()[1]
            before = next(item for item in baseline if item["entry_id"] == copied["entry_id"])
            now = next(item for item in after if item["entry_id"] == copied["entry_id"])
            assert (now["revision"], now["hash"]) == (before["revision"], before["hash"]), "copy overwrote existing target"
            additions = [item for item in after if item["entry_id"] not in baseline_ids]
            if additions:
                assert len(additions) == 1 and additions[0]["parent_id"] == copied["parent_id"], additions
                assert additions[0]["name"] != keep_name and additions[0]["hash"] == copied["hash"], additions
                assert_bytes(documents / "destination" / additions[0]["name"], fixtures["source/" + keep_name])
            else:
                expect(error_notices).not_to_have_count(0)
                assert not any(item.get("result", {}).get("state") == "committed" for item in submits_since(start)), "collision committed without a new entry"
            assert_bytes(documents / "destination" / keep_name, fixtures["source/" + keep_name])
            assert_bytes(documents / "source" / keep_name, fixtures["source/" + keep_name])
            checks.append("I04/file-copy/new-identity/same-directory-no-overwrite/visible-outcome")

            # Paste on a directory row targets that directory, not the current one.
            at("/"); menu_action("source", "复制")
            menu_action("destination", "粘贴")
            copied_dir = settled_entry("destination/source")
            assert copied_dir["entry_id"] != source["entry_id"]
            assert_bytes(documents / "destination/source" / keep_name, fixtures["source/" + keep_name])
            assert_bytes(documents / "destination/source/child/deep.txt", fixtures["source/child/deep.txt"])
            copied_empty = entry_at("destination/source/empty")
            assert copied_empty and copied_empty["kind"] == "directory" and copied_empty["entry_id"] != empty["entry_id"]
            assert (documents / "destination/source/empty").is_dir()
            assert list((documents / "destination/source/empty").iterdir()) == []
            checks.append("directory-copy/recursive-bytes/empty-directory/new-identities/row-paste-target")

            at("/destination"); menu_action(keep_name, "剪切")
            at("/")
            # Background context-menu paste targets the current directory.
            listing = page.locator(".files-list")
            listing.click(button="right", position={"x": 8, "y": listing.bounding_box()["height"] - 8})
            page.get_by_role("menu").get_by_role("menuitem", name="粘贴", exact=True).click()
            moved = settled_entry(keep_name)
            assert moved["entry_id"] == copied["entry_id"], "I03: move changed EntryId"
            assert entry_at("destination/" + keep_name) is None
            assert not (documents / "destination" / keep_name).exists()
            assert_bytes(documents / keep_name, fixtures["source/" + keep_name])
            checks.append("I03/cut/background-paste/current-directory/move-stable-identity")

            at("/"); menu_action("source", "剪切"); at("/source/child")
            clear_notices(); start = len(requests)
            toolbar.get_by_role("button", name="粘贴", exact=True).click()
            expect(error_notices).not_to_have_count(0)
            assert entry_at("source")["entry_id"] == source["entry_id"]
            assert entry_at("source/child/source") is None
            assert not any(item.get("result", {}).get("state") == "committed" for item in submits_since(start))
            assert_bytes(documents / "source/child/deep.txt", fixtures["source/child/deep.txt"])
            checks.append("I10/move-into-descendant-rejected/source-preserved")

            at("/"); start = len(requests)
            menu_action("cancel.txt", "删除")
            deletion = page.get_by_role("dialog", name="删除确认", exact=True)
            expect(deletion).to_be_visible()
            expect(deletion).to_contain_text("cancel.txt")
            deletion.get_by_role("button", name="取消", exact=True).click()
            expect(deletion).not_to_be_visible()
            assert not submits_since(start), "cancel submitted a mutation"
            assert_bytes(documents / "cancel.txt", fixtures["cancel.txt"])
            deleted = entry_at("delete.txt")
            menu_action("delete.txt", "删除")
            deletion.get_by_role("button", name="删除", exact=True).click()
            expect(row("delete.txt")).to_have_count(0, timeout=30000)
            wait_for(lambda: not (documents / "delete.txt").exists(), "confirmed delete did not remove working path")
            trash = api(f"/v1/vaults/{doc_id}/trash")["items"]
            assert any(item["entry_id"] == deleted["entry_id"] and item["hash"] == sha(fixtures["delete.txt"]) for item in trash), trash
            page.goto(origin + "/#/recovery")
            picker = page.get_by_role("combobox", name=field_label("恢复 Vault"))
            picker.click(); page.get_by_role("option", name="documents", exact=True).click()
            page.get_by_role("button", name="回收站", exact=True).click()
            recovery_row = page.get_by_role("row").filter(has=page.get_by_text(deleted["entry_id"], exact=True))
            expect(recovery_row).to_contain_text("delete.txt")
            recovery_row.get_by_role("button", name="恢复此项", exact=True).click()
            expect(page.get_by_text("恢复已提交", exact=True)).to_be_visible(timeout=30000)
            assert_bytes(documents / "delete.txt", fixtures["delete.txt"])
            assert entry_at("delete.txt")["entry_id"] == deleted["entry_id"]
            checks.append("delete/cancel-no-submit/confirm/trash-real-UI-restore/hash-identity")

            page.goto(origin + f"/#/files/{doc_id}"); expect(table).to_be_visible(timeout=15000)
            # Hold an actual browser delete with its original baseline. A separate
            # legitimate API client moves that entry before the delete reaches Rust.
            stale = entry_at("stale.txt")
            arm("hold-before", "delete")
            menu_action("stale.txt", "删除")
            deletion.get_by_role("button", name="删除", exact=True).click()
            held = await_fault()
            assert held["operation"]["base_revision"] == stale["revision"], held
            command("move", entry_id=stale["entry_id"], base_revision=stale["revision"], parent_id=source["entry_id"], name="stale.txt")
            release.set()
            wait_for(lambda: "result" in held, "stale delete did not return")
            assert held["status"] == 409 or held["result"].get("state") == "rejected", held
            expect(error_notices).not_to_have_count(0)
            assert entry_at("source/stale.txt")["entry_id"] == stale["entry_id"]
            assert_bytes(documents / "source/stale.txt", fixtures["stale.txt"])
            assert not any(item["entry_id"] == stale["entry_id"] for item in api(f"/v1/vaults/{doc_id}/trash")["items"])
            checks.append("I04/concurrent-old-revision-delete-rejected/newer-location-and-bytes-retained")
            end_rejected()

            # Directory confirmation must carry the token for its actual baseline.
            refresh(); menu_action("destination", "删除")
            arm("hold-before", "delete")
            deletion.get_by_role("button", name="删除", exact=True).click()
            held = await_fault()
            assert held["operation"].get("subtree_token"), held
            dest = entry_at("destination")
            command("create", parent_id=dest["entry_id"], name="new-after-confirmation", expected_absent=True)
            release.set()
            wait_for(lambda: "result" in held, "subtree delete did not return")
            assert held["status"] == 409 or held["result"].get("state") == "rejected", held
            assert entry_at("destination/new-after-confirmation")
            assert_bytes(documents / "destination/anchor.txt", fixtures["destination/anchor.txt"])
            assert_bytes(documents / "destination/source/child/deep.txt", fixtures["source/child/deep.txt"])
            checks.append("I04/directory-delete-subtree-token/concurrent-child-rejected/no-subtree-loss")
            end_rejected()

            # The service commits a real create; the proxy drops only its response.
            refresh(); clear_notices()
            lost = inputs / "lost-response.txt"; lost.write_bytes(b"S136 exactly one committed create\n")
            arm("drop-after", "create"); start = len(requests)
            upload([lost]); held = await_fault()
            assert held["result"].get("state") in ("committed", "pending"), held
            retry = page.get_by_role("button", name=re.compile("继续操作|核对.*重试|重试.*原|核对.*原"))
            expect(retry).to_be_visible(timeout=30000)
            # A transport failure is not a success notification, even if Rust committed.
            assert not any("成功" in notice.inner_text() or "已完成" in notice.inner_text() for notice in notifications.all()), "unknown result announced success"
            original_request = dict(held["operation"])
            end = page.get_by_role("button", name="结束操作", exact=True)
            if end.count(): expect(end).to_be_disabled()
            clear_notices()
            retry.click()
            recovered = settled_entry(lost.name)
            result = wait_for(lambda: terminal(original_request["op_id"]), "lost-response operation did not become terminal")
            assert result["state"] == "committed" and recovered["entry_id"] == result["entry_id"], result
            assert_bytes(documents / lost.name, lost.read_bytes())
            replayed = [item for item in submits_since(start) if item["operation"]["op_id"] == original_request["op_id"]]
            assert replayed and all(item["operation"] == original_request for item in replayed), replayed
            assert len({item["operation"]["op_id"] for item in submits_since(start)}) == 1, "retry allocated a second logical create"
            assert len([item for item in snapshot()[1] if item.get("parent_id") == recovered["parent_id"] and item["name"].startswith(lost.stem)]) == 1
            assert any(item["source"] == "ui" and item["method"] == "GET" and item["path"].endswith("/operations/" + original_request["op_id"]) for item in requests[start:]), "retry never checked the original operation"
            checks.append("I01/I06/real-submit-response-lost/unknown-not-success/retry-original-op/no-duplicate")

            # Reload must preserve the unresolved request. The recovery control
            # only queries the original OpId; it must not replay namespace writes.
            reloaded = inputs / "reload-unknown.txt"
            reloaded.write_bytes(b"S136 reload recovers exactly the original operation\n")
            arm("drop-after", "create"); start = len(requests)
            upload([reloaded]); held = await_fault()
            original_request = dict(held["operation"])
            expect(retry).to_be_visible(timeout=30000)
            retained = page.evaluate("Object.entries(sessionStorage).filter(([key])=>key.startsWith('vaultsyncd.file-actions.')).flatMap(([,value])=>JSON.parse(value))")
            assert any(item["request"] == original_request and item["vault"] == doc_id for item in retained), retained
            page.once("dialog", lambda dialog: dialog.accept())
            page.reload()
            expect(table).to_be_visible(timeout=15000)
            query = page.get_by_role("button", name="核对未确认操作", exact=True)
            expect(query).to_be_visible()
            expect(toolbar.get_by_role("button", name="上传文件", exact=True)).to_be_disabled()
            query_start = len(requests)
            clear_notices(); query.click()
            expect(toolbar.get_by_role("button", name="上传文件", exact=True)).to_be_enabled(timeout=30000)
            result = wait_for(lambda: terminal(original_request["op_id"]), "reloaded operation did not become terminal")
            assert result["state"] == "committed", result
            refresh()
            recovered = settled_entry(reloaded.name)
            assert recovered["entry_id"] == result["entry_id"]
            assert_bytes(documents / reloaded.name, reloaded.read_bytes())
            assert not submits_since(query_start), "query-only recovery submitted a namespace operation"
            assert len(submits_since(start)) == 1 and submits_since(start)[0]["operation"] == original_request, "reload duplicated the operation"
            assert any(item["source"] == "ui" and item["method"] == "GET" and item["path"].endswith("/operations/" + original_request["op_id"]) for item in requests[query_start:])
            assert len([item for item in snapshot()[1] if item.get("parent_id") == recovered["parent_id"] and item["name"].startswith(reloaded.stem)]) == 1
            checks.append("I01/I06/reload-unknown/session-request-retained/query-only-original-OpId/no-duplicate-namespace-submit")

            # Navigate while a real create response is delayed. Work belongs to the
            # captured Vault/directory, never to whichever directory is now visible.
            race = inputs / "navigation-race.txt"; race.write_bytes(b"S136 captured destination\n")
            clear_notices()
            arm("hold-after", "create"); start = len(requests)
            upload([race]); held = await_fault()
            assert held["result"].get("state") in ("committed", "pending"), held
            expect(toolbar.get_by_role("button", name="上传文件", exact=True)).to_be_disabled()
            assert not any("成功" in notice.inner_text() or "已完成" in notice.inner_text() for notice in notifications.all()), "held response announced success"
            page.screenshot(path=str(RUN / "unconfirmed-task.png"), full_page=True)
            at("/destination")
            at("/", arc_id)
            expect(row("archive-only.txt")).to_be_visible()
            release.set(); page.wait_for_timeout(700)
            expect(row("archive-only.txt")).to_be_visible()
            expect(row(race.name)).to_have_count(0)
            assert not (archive / race.name).exists()
            assert list(archive.iterdir()) == [archive / "archive-only.txt"]
            assert_bytes(archive / "archive-only.txt", b"S136 unrelated vault\n")
            assert all(f"/vaults/{doc_id}/" in item["path"] for item in submits_since(start)), "task crossed Vaults after navigation"
            at("/"); refresh(); settled_entry(race.name)
            assert_bytes(documents / race.name, race.read_bytes())
            assert not (documents / "destination" / race.name).exists()
            checks.append("I01/vault-switch-and-directory-navigation-in-flight/captured-target/no-stale-view-or-cross-vault-write")

            # Cross-Vault copy is supported (cross-Vault cut is not implied). Read
            # fixed source bytes and publish a new identity only in the target.
            at("/source"); menu_action(keep_name, "复制")
            at("/", arc_id)
            start = len(requests)
            toolbar.get_by_role("button", name="粘贴", exact=True).click()
            cross_vault = settled_entry(keep_name, arc_id)
            assert cross_vault["entry_id"] != entry_at("source/" + keep_name)["entry_id"]
            assert cross_vault["hash"] == sha(fixtures["source/" + keep_name])
            assert_bytes(archive / keep_name, fixtures["source/" + keep_name])
            assert_bytes(documents / "source" / keep_name, fixtures["source/" + keep_name])
            assert all(f"/vaults/{arc_id}/" in item["path"] for item in submits_since(start)), "cross-Vault copy mutated the source namespace"
            checks.append("cross-Vault-copy/supported/new-target-identity/source-bytes-unchanged")

            # A real multi-step job has already committed its first namespace
            # operation. Log out and back in through SPA navigation before its
            # response arrives: the old job must not append its second operation.
            for task_kind in ("upload", "copy"):
                at("/")
                if task_kind == "copy":
                    menu_action("source", "复制")
                    at("/", arc_id)
                else:
                    first = inputs / "logout-upload-first.txt"
                    second = inputs / "logout-upload-second.txt"
                    first.write_bytes(b"S136 first item may already be committed\n")
                    second.write_bytes(b"S136 second item must remain unsubmitted\n")
                arm("hold-after", "create"); start = len(requests)
                # This run's proxy may intercept either Vault, never another run.
                with lock: fault["vault"] = arc_id if task_kind == "copy" else doc_id
                if task_kind == "copy":
                    toolbar.get_by_role("button", name="粘贴", exact=True).click()
                else:
                    upload([first, second])
                held = await_fault()
                assert held["result"].get("state") == "committed", "logout gate requires a committed first step, not a pending shortcut"
                clear_notices()
                page.get_by_role("link", name="账号与会话", exact=True).click()
                expect(page.get_by_text("已登录：owner", exact=True)).to_be_visible()
                page.get_by_role("button", name="退出当前会话", exact=True).click()
                expect(page.get_by_label(field_label("登录名"))).to_be_visible()
                expect(page.get_by_role("navigation", name="主要导航")).to_have_count(0)
                page.get_by_label(field_label("登录名")).fill("owner")
                page.get_by_label(field_label("密码")).fill(password)
                page.get_by_role("button", name="登录", exact=True).click()
                expect(page.get_by_text("已登录：owner", exact=True)).to_be_visible()
                release.set()
                wait_for(lambda: held.get("delivered"), "old response was not delivered after re-login")
                page.wait_for_timeout(700)  # Allow the old promise's gated continuation.
                assert len(submits_since(start)) == 1, f"{task_kind}: old task appended a namespace operation after logout"
                if task_kind == "copy":
                    partial = entry_at("source", arc_id)
                    assert partial and partial["entry_id"] == held["result"]["entry_id"]
                    assert not any(item.get("parent_id") == partial["entry_id"] for item in snapshot(arc_id)[1]), "old copy appended a child after logout"
                    assert (archive / "source").is_dir() and list((archive / "source").iterdir()) == []
                    assert_bytes(documents / "source" / keep_name, fixtures["source/" + keep_name])
                else:
                    assert_bytes(documents / first.name, first.read_bytes())
                    assert entry_at(second.name) is None and not (documents / second.name).exists()
                    assert second.read_bytes() == b"S136 second item must remain unsubmitted\n"
                page.get_by_role("link", name="文件浏览器", exact=True).click()
                expect(table).to_be_visible(timeout=15000)
                query = page.get_by_role("button", name="核对未确认操作", exact=True)
                if query.is_visible():
                    clear_notices(); query.click()
                expect(toolbar.get_by_role("button", name="上传文件", exact=True)).to_be_enabled(timeout=15000)
                assert len(submits_since(start)) == 1, f"{task_kind}: re-login recovery silently resumed old steps"
                checks.append(f"I02/I06/{task_kind}-in-flight/account-navigation/logout-relogin/old-task-no-next-namespace-submit")
            at("/")

            # S137 uses dedicated samples so every S136 source-preservation check
            # remains meaningful after real file and directory renames.
            renamed_input = inputs / "s137-before.txt"
            renamed_bytes = b"S137 rename preserves immutable content and identity\n"
            renamed_input.write_bytes(renamed_bytes)
            upload([renamed_input]); original_file = settled_entry(renamed_input.name)
            rename_dialog = page.get_by_role("dialog", name="重命名", exact=True)
            start = len(requests)
            menu_action(renamed_input.name, "重命名")
            expect(rename_dialog).to_be_visible()
            name_input = rename_dialog.get_by_role("textbox", name="名称", exact=True)
            expect(name_input).to_have_value(renamed_input.name)
            expect(name_input).to_be_focused()
            assert name_input.evaluate("el => el.value.slice(el.selectionStart, el.selectionEnd)") == "s137-before"
            # Typing replaces only the selected stem, leaving the extension intact.
            name_input.press_sequentially("s139-replacement")
            expect(name_input).to_have_value("s139-replacement.txt")
            name_input.fill(renamed_input.name)
            expect(rename_dialog.get_by_role("button", name="保存", exact=True)).to_be_disabled()
            rename_dialog.get_by_role("button", name="取消", exact=True).click()
            expect(rename_dialog).not_to_be_visible()
            assert not submits_since(start), "rename cancellation submitted a mutation"
            for filename, selected in (
                ("s139.tar.gz", "s139.tar"),
                (".s139-hidden", ".s139-hidden"),
                (".s139-hidden.txt", ".s139-hidden"),
                ("s139-no-extension", "s139-no-extension"),
                ("s139-文档😀.txt", "s139-文档😀"),
                ("s139.folder", "s139.folder"),
            ):
                menu_action(filename, "重命名")
                expect(name_input).to_have_value(filename)
                expect(name_input).to_be_focused()
                assert name_input.evaluate("el => el.value.slice(el.selectionStart, el.selectionEnd)") == selected, filename
                rename_dialog.get_by_role("button", name="取消", exact=True).click()
                expect(rename_dialog).not_to_be_visible()
            assert not submits_since(start), "selection/cancellation submitted a mutation"
            checks.append("S139/rename-default-selection/stem-only/typing-preserves-extension/multi-dot/dotfile/no-extension/Unicode/dotted-directory")
            menu_action(renamed_input.name, "重命名")
            for invalid in ("cancel.txt", "../escape", "nested/name", "nested\\name", ".", ".."):
                name_input.fill(invalid)
                rename_dialog.get_by_role("button", name="保存", exact=True).click()
                expect(rename_dialog).to_be_visible()
                expect(rename_dialog.locator('.mantine-TextInput-error')).to_be_visible()
                assert not submits_since(start), f"invalid/colliding rename submitted {invalid!r}"
            name_input.fill("")
            expect(rename_dialog.get_by_role("button", name="保存", exact=True)).to_be_disabled()
            renamed_name = "s137-renamed & 文档.txt"
            name_input.fill(renamed_name)
            rename_dialog.get_by_role("button", name="保存", exact=True).click()
            renamed_file = settled_entry(renamed_name)
            assert entry_at(renamed_input.name) is None and not (documents / renamed_input.name).exists()
            assert (renamed_file["entry_id"], renamed_file["parent_id"], renamed_file["version_id"], renamed_file["hash"]) == (original_file["entry_id"], original_file["parent_id"], original_file["version_id"], original_file["hash"])
            assert renamed_file["revision"] != original_file["revision"]
            submitted = submits_since(start)
            assert len(submitted) == 1, submitted
            request = submitted[0]["operation"]
            assert request["kind"] == "move" and request["entry_id"] == original_file["entry_id"]
            assert request["base_revision"] == original_file["revision"] and request["base_version_id"] == original_file["version_id"]
            assert request["parent_id"] == original_file["parent_id"] and request["name"] == renamed_name
            assert_bytes(documents / renamed_name, renamed_bytes)
            checks.append("S137/I03-I04/file-rename/cancel-invalid-collision-no-submit/CAS/same-identity-version-hash")

            # The open dialog keeps its original revision even when another
            # legitimate writer renames the source before Save is submitted.
            stale_rename = inputs / "s137-stale-original.txt"
            stale_rename.write_bytes(b"S137 stale rename must preserve newer namespace\n")
            upload([stale_rename]); stale_original = settled_entry(stale_rename.name)
            menu_action(stale_rename.name, "重命名")
            command("move", entry_id=stale_original["entry_id"], base_revision=stale_original["revision"],
                    base_version_id=stale_original["version_id"], parent_id=stale_original["parent_id"], name="s137-stale-current.txt")
            name_input.fill("s137-stale-requested.txt")
            start = len(requests)
            rename_dialog.get_by_role("button", name="保存", exact=True).click()
            expect(error_notices).not_to_have_count(0)
            rejected_rename = submits_since(start)
            assert len(rejected_rename) == 1, rejected_rename
            assert rejected_rename[0]["operation"]["base_revision"] == stale_original["revision"]
            assert rejected_rename[0].get("status") == 409 or rejected_rename[0].get("result", {}).get("state") == "rejected", rejected_rename
            end_rejected()
            current_stale = settled_entry("s137-stale-current.txt")
            assert current_stale["entry_id"] == stale_original["entry_id"] and current_stale["version_id"] == stale_original["version_id"]
            assert entry_at("s137-stale-requested.txt") is None and entry_at(stale_rename.name) is None
            assert_bytes(documents / "s137-stale-current.txt", stale_rename.read_bytes())
            assert not (documents / "s137-stale-requested.txt").exists()
            assert len(submits_since(start)) == 1, "ending stale rename silently submitted another operation"
            checks.append("S137/I04/rename-dialog-freezes-baseline/concurrent-source-rename-rejected/newer-name-version-bytes-preserved")

            # An unknown rename must recover its original logical operation.
            menu_action(renamed_name, "重命名")
            final_name = "s137-recovered.txt"
            name_input.fill(final_name)
            arm("drop-after", "move"); start = len(requests)
            rename_dialog.get_by_role("button", name="保存", exact=True).click()
            held = await_fault(); original_request = dict(held["operation"])
            retry = page.get_by_role("button", name="继续操作", exact=True)
            expect(retry).to_be_visible(timeout=30000)
            assert not any("重命名完成" in notice.inner_text() for notice in notifications.all()), "unknown rename announced completion"
            clear_notices(); retry.click()
            recovered_file = settled_entry(final_name)
            assert recovered_file["entry_id"] == original_file["entry_id"]
            assert recovered_file["version_id"] == original_file["version_id"]
            assert len(submits_since(start)) == 1 and submits_since(start)[0]["operation"] == original_request
            assert any(item["source"] == "ui" and item["method"] == "GET" and item["path"].endswith("/operations/" + original_request["op_id"]) for item in requests[start:])
            assert_bytes(documents / final_name, renamed_bytes)
            checks.append("S137/I01-I06/rename-lost-response/query-original-OpId/no-duplicate")

            vault_entry = entry_at("")
            parent_dir = command("create", parent_id=vault_entry["entry_id"], name="s137-tree-parent", expected_absent=True)
            child_dir = command("create", parent_id=parent_dir["entry_id"], name="child", expected_absent=True)
            target_dir = command("create", parent_id=vault_entry["entry_id"], name="s137-tree-target", expected_absent=True)
            refresh(); at("/s137-tree-parent/child")
            inside = inputs / "s137-inside.txt"; inside.write_bytes(b"S137 directory rename and tree paste\n")
            upload([inside]); inside_entry = settled_entry("s137-tree-parent/child/" + inside.name)
            tree = page.get_by_role("tree", name="目录树", exact=True)

            def tree_label(entry_id):
                return tree.locator(f'[role="treeitem"][data-value="{entry_id}"] > div[title]')

            def tree_menu_action(entry_id, action):
                clear_notices()
                tree_label(entry_id).click(button="right")
                menu = page.get_by_role("menu")
                expect(menu).to_be_visible()
                menu.get_by_role("menuitem", name=action, exact=True).click()
                expect(menu).not_to_be_visible()

            # Right-click and keyboard menus use the targeted ancestor without
            # selecting it, opening it or leaking another list/tree menu.
            before_url = page.url
            row(inside.name).click(button="right")
            expect(page.get_by_role("menu")).to_have_count(1)
            tree_label(parent_dir["entry_id"]).click(button="right")
            expect(page.get_by_role("menu")).to_have_count(1)
            assert page.url == before_url
            for action in ("打开", "属性", "复制路径", "复制", "剪切", "粘贴", "重命名", "删除"):
                expect(page.get_by_role("menu").get_by_role("menuitem", name=action, exact=True)).to_have_count(1)
            page.get_by_role("menu").get_by_role("menuitem", name="属性", exact=True).click()
            properties = page.get_by_role("dialog", name="文件属性", exact=True)
            expect(properties).to_contain_text(parent_dir["entry_id"])
            expect(properties).to_contain_text("s137-tree-parent")
            properties.get_by_role("button", name="关闭", exact=True).click()
            expect(properties).not_to_be_visible()
            tree_item = tree.locator(f'[role="treeitem"][data-value="{parent_dir["entry_id"]}"]')
            tree_item.focus(); tree_item.press("Shift+F10")
            expect(page.get_by_role("menu")).to_be_visible()
            assert page.url == before_url
            expanded_before = tree_item.get_attribute("aria-expanded")
            property_item = page.get_by_role("menu").get_by_role("menuitem", name="属性", exact=True)
            property_item.focus()
            for key in ("ArrowLeft", "ArrowRight"):
                property_item.press(key)
                expect(tree_item).to_have_attribute("aria-expanded", expanded_before)
                expect(page.get_by_role("menu")).to_be_visible()
                assert page.url == before_url
            property_item.press("Space")
            expect(properties).to_be_visible()
            expect(properties).to_contain_text(parent_dir["entry_id"])
            properties.get_by_role("button", name="关闭", exact=True).click()
            expect(properties).not_to_be_visible()
            tree_item.focus(); tree_item.press("Shift+F10")
            page.get_by_role("menu").get_by_role("menuitem", name="重命名", exact=True).press("Space")
            expect(rename_dialog).to_be_visible()
            name_input.fill("s137-tree-renamed")
            rename_dialog.get_by_role("button", name="保存", exact=True).click()
            expect(page).to_have_url(origin + f"/#/files/{doc_id}/s137-tree-renamed/child")
            expect(page.get_by_role("textbox", name="目录地址", exact=True)).to_have_value("/s137-tree-renamed/child")
            expect(row(inside.name)).to_be_visible(timeout=30000)
            expect(tree.locator(f'[role="treeitem"][data-value="{child_dir["entry_id"]}"]')).to_have_attribute("aria-selected", "true")
            assert entry_at("s137-tree-parent") is None
            assert entry_at("s137-tree-renamed")["entry_id"] == parent_dir["entry_id"]
            assert entry_at("s137-tree-renamed/child")["entry_id"] == child_dir["entry_id"]
            assert_bytes(documents / "s137-tree-renamed/child" / inside.name, inside.read_bytes())
            checks.append("S137/tree-right-click/one-menu/no-navigation/target-properties/ShiftF10/ancestor-rename-url-selection-subtree")

            tree_menu_action(child_dir["entry_id"], "重命名")
            name_input.fill("child-renamed")
            rename_dialog.get_by_role("button", name="保存", exact=True).click()
            expect(page).to_have_url(origin + f"/#/files/{doc_id}/s137-tree-renamed/child-renamed")
            expect(page.get_by_role("textbox", name="目录地址", exact=True)).to_have_value("/s137-tree-renamed/child-renamed")
            expect(row(inside.name)).to_be_visible(timeout=30000)
            assert entry_at("s137-tree-renamed/child-renamed")["entry_id"] == child_dir["entry_id"]
            assert entry_at("s137-tree-renamed/child-renamed/" + inside.name)["entry_id"] == inside_entry["entry_id"]
            # Several real observer passes must preserve the renamed subtree,
            # including descendants whose revision did not change with ancestors.
            page.wait_for_timeout(3500)
            refresh()
            expect(row(inside.name)).to_be_visible(timeout=15000)
            after_scan = entry_at("s137-tree-renamed/child-renamed/" + inside.name)
            assert after_scan["entry_id"] == inside_entry["entry_id"] and after_scan["version_id"] == inside_entry["version_id"]
            checks.append("S137/current-directory-rename/url-address-and-stable-identity/multiple-observer-passes")

            menu_action(inside.name, "复制")
            tree_menu_action(target_dir["entry_id"], "粘贴")
            copied_into_tree = wait_for(lambda: entry_at("s137-tree-target/" + inside.name), "tree paste did not target selected directory")
            expect(toolbar.get_by_role("button", name="上传文件", exact=True)).to_be_enabled(timeout=30000)
            assert copied_into_tree["parent_id"] == target_dir["entry_id"] and copied_into_tree["entry_id"] != inside_entry["entry_id"]
            assert_bytes(documents / "s137-tree-target" / inside.name, inside.read_bytes())
            assert page.url.endswith("/s137-tree-renamed/child-renamed")
            tree_label(vault_entry["entry_id"]).click(button="right")
            menu = page.get_by_role("menu")
            for action in ("复制", "剪切", "重命名", "删除"):
                expect(menu.get_by_role("menuitem", name=action, exact=True)).to_be_disabled()
            expect(menu.get_by_role("menuitem", name="粘贴", exact=True)).to_be_enabled()
            menu.get_by_role("menuitem", name="粘贴", exact=True).click()
            pasted_at_vault = wait_for(lambda: entry_at(inside.name), "vault-tree paste did not target Vault")
            expect(toolbar.get_by_role("button", name="上传文件", exact=True)).to_be_enabled(timeout=30000)
            assert pasted_at_vault["parent_id"] == vault_entry["entry_id"]
            assert_bytes(documents / inside.name, inside.read_bytes())
            checks.append("S137/tree-paste-target/vault-mutation-guards/vault-paste/stable-current-directory")

            start = len(requests)
            tree_menu_action(target_dir["entry_id"], "删除")
            expect(deletion).to_contain_text("s137-tree-target")
            expect(deletion).to_contain_text("2 项")
            deletion.get_by_role("button", name="取消", exact=True).click()
            assert not submits_since(start), "tree delete cancel submitted a mutation"
            assert_bytes(documents / "s137-tree-target" / inside.name, inside.read_bytes())
            tree_menu_action(target_dir["entry_id"], "删除")
            deletion.get_by_role("button", name="删除", exact=True).click()
            wait_for(lambda: entry_at("s137-tree-target") is None, "tree delete never committed")
            expect(tree_label(target_dir["entry_id"])).to_have_count(0, timeout=30000)
            submitted = submits_since(start)
            assert len(submitted) == 1 and submitted[0]["operation"].get("subtree_token"), submitted
            trash = api(f"/v1/vaults/{doc_id}/trash")["items"]
            assert any(item["entry_id"] == target_dir["entry_id"] for item in trash)
            assert_bytes(documents / "s137-tree-renamed/child-renamed" / inside.name, inside.read_bytes())
            checks.append("S137/tree-delete/explicit-confirmation/cancel-no-submit/subtree-token/trash/source-protected")

            # Navigation while a rename result is held must win over the old
            # directory identity retained for the operation's refresh.
            menu_action(inside.name, "重命名")
            name_input.fill("s137-inside-renamed.txt")
            arm("hold-after", "move")
            rename_dialog.get_by_role("button", name="保存", exact=True).click()
            held = await_fault()
            at("/"); release.set()
            wait_for(lambda: held.get("delivered"), "rename response never released")
            expect(toolbar.get_by_role("button", name="上传文件", exact=True)).to_be_enabled(timeout=30000)
            expect(page).to_have_url(origin + f"/#/files/{doc_id}")
            expect(page.get_by_role("textbox", name="目录地址", exact=True)).to_have_value("/")
            assert entry_at("s137-tree-renamed/child-renamed/s137-inside-renamed.txt")["entry_id"] == inside_entry["entry_id"]
            assert_bytes(documents / "s137-tree-renamed/child-renamed/s137-inside-renamed.txt", inside.read_bytes())
            checks.append("S137/rename-response-navigation-race/current-navigation-wins")

            # Real operation dialogs and expanded context menus must fit both themes.
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 390, 1440):
                    page.set_viewport_size({"width": width, "height": 900})
                    page.wait_for_timeout(250)
                    row("cancel.txt").click(button="right")
                    menu = page.get_by_role("menu")
                    expect(menu).to_be_visible()
                    box = menu.bounding_box()
                    assert box["x"] >= -1 and box["x"] + box["width"] <= width + 1, (scheme, width, box)
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width, "menu overflow")
                    page.wait_for_timeout(300)  # Capture the settled menu, not its fade-in frame.
                    page.screenshot(path=str(RUN / f"actions-{scheme}-{width}.png"), full_page=True)
                    menu.get_by_role("menuitem", name="重命名", exact=True).click()
                    expect(rename_dialog).to_be_visible()
                    name_input.fill("s137-窄屏名称-" + "long-name-" * 12 + ".txt")
                    page.wait_for_timeout(300)
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width, "rename modal overflow")
                    page.screenshot(path=str(RUN / f"rename-{scheme}-{width}.png"), full_page=True)
                    rename_dialog.get_by_role("button", name="取消", exact=True).click()
                    row("cancel.txt").click(button="right")
                    menu.get_by_role("menuitem", name="删除", exact=True).click()
                    expect(deletion).to_be_visible()
                    page.wait_for_timeout(300)  # Same for the modal transition.
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width, "delete modal overflow")
                    assert page.locator("html").get_attribute("data-mantine-color-scheme") == scheme
                    page.screenshot(path=str(RUN / f"delete-{scheme}-{width}.png"), full_page=True)
                    deletion.get_by_role("button", name="取消", exact=True).click()
            checks.append("light-dark/320-390-1440/context-menu/rename-delete-dialog/no-horizontal-overflow")
            for name, data in fixtures.items():
                actual = documents / ("source/stale.txt" if name == "stale.txt" else name)
                assert_bytes(actual, data)
            for name, data in upload_data.items(): assert_bytes(documents / name, data)
            csp.extend(page.evaluate("window.__csp"))
            assert not csp, csp
            assert not errors, errors
            assert not any("proxy_error" in item for item in requests), [item for item in requests if "proxy_error" in item]
            checks.append("I02/source-and-unrelated-bytes-protected/CSP/no-runtime-errors")
            save_result(True)
            context.close(); context = None
            browser.close(); browser = None
        print(f"S136/S137 browser acceptance passed: {RUN}", flush=True)
    except Exception as cause:
        save_result(False, f"{type(cause).__name__}: {cause}")
        raise
    finally:
        release.set()
        if context:
            try: context.close()
            except Exception: pass  # Preserve the original failure during cleanup.
        if browser:
            try: browser.close()
            except Exception: pass
        if proxy: proxy.shutdown(); proxy.server_close()
        if service.poll() is None:
            service.terminate()
            try: service.wait(timeout=15)
            except subprocess.TimeoutExpired: service.kill(); service.wait()
        log.close()


if __name__ == "__main__": main()
