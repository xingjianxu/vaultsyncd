#!/usr/bin/env python3
"""S210 text/Markdown previews against built Web, Chromium and isolated btrfs.

Run with `nix develop .#web-tests --command python3 tests/web/text-preview-browser.py`.
VAULTSYNCD_TEST_BIN_DIR, VAULTSYNCD_TEST_WEB_DIR and VAULTSYNCD_WEB_RUN_ROOT
select real binaries/assets and the evidence parent beneath /data/vaultsyncd-dev.
Only explicitly armed proxy faults corrupt or delay actual backend responses.
"""
from contextlib import contextmanager
import functools
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import threading
import time
from urllib.parse import urlsplit

from playwright.sync_api import expect, sync_playwright
from csp_support import CspStaticFiles, field_label

REPO = Path(__file__).resolve().parents[2]
DEV = Path("/data/vaultsyncd-dev").resolve()
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug")).resolve()
WEB = Path(os.environ.get("VAULTSYNCD_TEST_WEB_DIR", REPO / "apps/web/dist")).resolve()
PARENT = Path(os.environ.get("VAULTSYNCD_WEB_RUN_ROOT", str(DEV))).resolve()
RUN = PARENT / f"S210-text-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def checked(args, **kwargs):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, **kwargs)
    assert result.returncode == 0, f"{args[0]} failed: {result.stderr}"
    return result.stdout


def main():
    assert PARENT.is_relative_to(DEV) and PARENT.is_dir() and os.access(PARENT, os.W_OK), PARENT
    assert (WEB / "index.html").is_file(), WEB
    assert all(os.access(BIN / name, os.X_OK) for name in ("vaultsyncd-admin", "vaultsyncd-server")), BIN
    RUN.mkdir(mode=0o700)
    for name in ("tmp", "cache", "downloads", "evidence", "vaults", "state"):
        (RUN / name).mkdir(mode=0o700)
    browser_tmp = DEV / "cache/tmp" / f"S210-{os.getpid()}"
    browser_tmp.mkdir(mode=0o700)
    os.environ.update(TMPDIR=str(browser_tmp), XDG_CACHE_HOME=str(RUN / "cache"))
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    evidence, documents = RUN / "evidence", RUN / "vaults/documents"
    print(f"S210 artifacts: {RUN}", flush=True)
    documents.mkdir(); (documents / "child").mkdir()
    fixtures = {
        "example.js": b'const answer = 42;\nconsole.log("hello");\n',
        "example.ts": b'interface Item { name: string }\nconst value: Item = { name: "hello" };\n',
        "example.py": b'def greet(name):\n    return "hello " + name\n',
        "example.rs": b'fn main() { let answer = 42; println!("{}", answer); }\n',
        "example.json": b'{"hello": true, "count": 42}\n',
        "unicode.txt": "中文文本与 emoji 🐈\nsecond line\n".encode(),
        "empty.txt": b"",
        "unknown.unlisted": b"plain fallback text\n",
        "README": b"extensionless text\n",
        "utf16.txt": "带 BOM 的 UTF-16 文本\r\nhello".encode("utf-16"),
        "source.html": b'<script>globalThis.__s210Executed=true;alert("unsafe")</script>\n',
        "source.svg": b'<svg xmlns="http://www.w3.org/2000/svg" onload="globalThis.__s210Executed=true"/>\n',
        "document.md": ("# Preview heading\n\n中文 Markdown\n\n"
                        "| Name | Value |\n| --- | --- |\n| hello | 42 |\n\n"
                        "- [x] done\n\n```js\nconst fenced = 42;\n```\n\n"
                        '<script>globalThis.__s210Executed=true;alert("unsafe")</script>\n\n'
                        '<img src="https://untrusted.invalid/html-image" onerror="alert(1)">\n\n'
                        "![remote image](https://untrusted.invalid/image.png)\n\n"
                        "[active link](javascript:alert(1))\n\n[external link](https://untrusted.invalid/)\n").encode(),
        "complex.md": ("```js\nx\n```\n\n" * 33).encode(),
        "bad.txt": b"invalid UTF-8: \xff\xfe\xff\n",
        "binary.txt": b"binary\0payload\x01\x02\x03",
        "oversize.txt": b"x" * (2 * 1024 * 1024 + 1),
    }
    for name, data in fixtures.items():
        (documents / name).write_bytes(data)
    (documents / "child/leaf.txt").write_text("child leaf text\n")
    assets = {str(path.relative_to(WEB)): sha(path.read_bytes()) for path in sorted(WEB.rglob("*")) if path.is_file()}
    (evidence / "inputs.json").write_text(json.dumps({
        "binary_directory": str(BIN), "web_directory": str(WEB), "assets_sha256": assets,
        "binaries": {name: sha((BIN / name).read_bytes()) for name in ("vaultsyncd-admin", "vaultsyncd-server")},
        "fixtures": {name: {"size": len(data), "sha256": sha(data)} for name, data in fixtures.items()},
    }, ensure_ascii=False, indent=2))
    password = secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", RUN / "state", "--login", "owner"], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{RUN / "state"}"\n[vaults]\nallowed_bases=["{RUN / "vaults"}"]\n[log]\nformat="json"\nlevel="info"\n')
    log = (evidence / "server.log").open("w")
    service = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log, cwd=RUN)
    proxy = context = page = None
    requests, checks, errors, violations, external, dialogs = [], [], [], [], [], []
    lock = threading.Lock()
    fault = {"mode": None, "entry_id": None}
    holds = []
    failure = None

    def arm(mode=None, entry_id=None):
        with lock:
            fault.clear(); fault.update(mode=mode, entry_id=entry_id)
            if mode == "delay":
                events = [threading.Event() for _ in range(3)]
                fault.update(started=events[0], release=events[1], finished=events[2])
                holds.append(events[1])
                return events

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
                    except Exception as capture_error:
                        errors.append(f"capture: {capture_error}")
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
                backend = http.client.HTTPConnection(address, timeout=30)
                body = self.rfile.read(int(self.headers.get("content-length", "0")))
                headers = {key: value for key, value in self.headers.items() if key.lower() not in ("connection", "transfer-encoding")}
                record = {"method": self.command, "path": self.path}
                with lock:
                    current = dict(fault); requests.append(record)
                content = urlsplit(self.path).path.endswith("/content") and f"/entries/{current['entry_id']}/" in self.path
                try:
                    backend.request(self.command, self.path, body=body, headers=headers)
                    response = backend.getresponse(); payload = response.read()
                    record["status"] = response.status
                    if content and response.status in (200, 206):
                        record["fault"] = current["mode"]
                        if current["mode"] == "corrupt" and payload:
                            payload = payload[:-1] + bytes([payload[-1] ^ 1])
                        elif current["mode"] == "delay":
                            current["started"].set()
                            assert current["release"].wait(45), "delay never released"
                    self.send_response(response.status)
                    for key, value in response.getheaders():
                        if key.lower() not in ("connection", "transfer-encoding", "content-length"):
                            self.send_header(key, value)
                    self.send_header("content-length", str(len(payload))); self.end_headers()
                    self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError):
                    record["browser_cancelled"] = True
                except Exception as cause:
                    record["proxy_error"] = str(cause); self.close_connection = True
                finally:
                    if content and current["mode"] == "delay":
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
            context.expose_binding("s210CspViolation", lambda _source, detail: violations.append(detail))
            context.add_init_script("document.addEventListener('securitypolicyviolation', e => window.s210CspViolation({directive:e.effectiveDirective,blocked:e.blockedURI}));")
            context.on("page", lambda opened: opened.on("pageerror", lambda error: errors.append(str(error))))
            for opened in context.pages:
                opened.on("pageerror", lambda error: errors.append(str(error)))
            context.on("request", lambda request: external.append(request.url) if urlsplit(request.url).scheme in ("http", "https") and not request.url.startswith(origin + "/") else None)
            page = context.pages[0] if context.pages else context.new_page()
            page.on("dialog", lambda dialog: (dialogs.append(dialog.message), dialog.dismiss()))
            page.goto(origin + "/#/files")
            page.get_by_label(field_label("登录名")).fill("owner")
            page.get_by_label(field_label("密码")).fill(password)
            page.get_by_role("button", name="登录", exact=True).click()
            expect(page.get_by_role("combobox", name="浏览 Vault", exact=True)).to_be_visible(timeout=30000)

            def api(path, body=None):
                result = page.evaluate("""async ([path,body])=>{const r=await fetch(path,body===null?{}:{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});return {status:r.status,body:await r.json()}}""", [path, body])
                assert result["status"] in (200, 201), result
                return result["body"]

            def wait_for(read, message):
                for _ in range(240):
                    value = read()
                    if value:
                        return value
                    page.wait_for_timeout(250)
                raise AssertionError(message)

            vault = api("/v1/admin/vaults", {"path": str(documents)})["vault_id"]
            wait_for(lambda: all(item["collection"]["state"] == "complete" for item in api("/v1/admin/vaults")["vaults"]), "observer import incomplete")
            snapshot = api(f"/v1/vaults/{vault}/snapshots", {})
            entries = api(f"/v1/vaults/{vault}/snapshots/{snapshot['snapshot_id']}/entries?limit=1000")["entries"]
            by_name = {item["name"]: item for item in entries}
            for name, data in fixtures.items():
                assert by_name[name]["hash"] == sha(data) and by_name[name]["size"] == str(len(data)), by_name[name]
            checks.append("real-observer/committed-fixtures/hash-and-size")
            source = by_name["example.js"]
            path = f"/v1/vaults/{vault}/entries/{source['entry_id']}/versions/{source['version_id']}/content"
            assert page.evaluate("async path => (await fetch(path,{credentials:'omit'})).status", path) == 401
            checks.append("fixed-version-text/anonymous-401")
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

            def source_ready(text):
                editor = preview.locator(".cm-content").first
                expect(editor).to_be_visible(timeout=20000)
                # Empty CodeMirror lines contain BR nodes whose innerText adds
                # extra breaks; compare the actual logical line text instead.
                expected = text.replace("\r\n", "\n").rstrip("\n")
                wait_for(lambda: "\n".join(editor.locator(".cm-line").all_text_contents()).rstrip("\n") == expected,
                         f"source content differs for {text[:40]!r}")
                return editor

            def download_exact(name):
                with page.expect_download() as pending:
                    preview.get_by_role("button", name="下载原文件", exact=True).click()
                download = pending.value
                assert download.suggested_filename == name and download.failure() is None
                saved = RUN / "downloads" / name
                download.save_as(saved)
                assert saved.read_bytes() == fixtures[name]

            highlights = {}
            for name in ("example.js", "example.ts", "example.py", "example.rs", "example.json"):
                before_url = page.url
                open_preview(name, menu=name == "example.ts")
                editor = source_ready(fixtures[name].decode())
                wait_for(lambda: editor.locator("span[class]").count() > 1, f"no syntax spans for {name}")
                colors = editor.locator("span[class]").evaluate_all("nodes=>[...new Set(nodes.map(n=>getComputedStyle(n).color))]")
                assert len(colors) > 1, (name, colors)
                highlights[name] = colors
                assert editor.get_attribute("contenteditable") == "false"
                before = editor.inner_text()
                editor.click(); page.keyboard.type("UNAUTHORIZED EDIT")
                assert editor.inner_text() == before
                assert page.url == before_url
                assert preview.locator("iframe,object,embed").count() == 0
                download_exact(name); close_preview()
            checks.append("JS-TS-Python-Rust-JSON/visible-syntax-colors/read-only/in-place/exact-download")
            for name in ("unicode.txt", "empty.txt", "unknown.unlisted", "README", "utf16.txt", "source.html", "source.svg"):
                open_preview(name)
                text = fixtures[name].decode("utf-16" if name == "utf16.txt" else "utf-8")
                source_ready(text); download_exact(name)
                assert preview.locator(".cm-content iframe,.cm-content object,.cm-content embed,.cm-content script,.cm-content img,.cm-content svg").count() == 0
                close_preview()
            checks.append("Unicode-empty-unknown-extension-no-extension-UTF16/HTML-SVG-source-only/original-download")

            open_preview("document.md")
            rendered = preview.locator(".file-preview-markdown")
            expect(rendered.get_by_role("heading", name="Preview heading")).to_be_visible(timeout=20000)
            assert rendered.evaluate("article => parseFloat(getComputedStyle(article.querySelector('h1')).fontSize) > parseFloat(getComputedStyle(article).fontSize)"), "rendered Markdown heading must be visually distinct from body text"
            expect(rendered.locator("table")).to_be_visible()
            expect(rendered.get_by_text("中文 Markdown", exact=True)).to_be_visible()
            expect(rendered.locator("input[type=checkbox]")).to_be_checked()
            expect(rendered.locator(".file-preview-code-block .cm-content")).to_contain_text("const fenced = 42;")
            wait_for(lambda: rendered.locator(".file-preview-code-block .cm-content span[class]").count() > 1,
                     "Markdown fenced code was not highlighted")
            assert rendered.locator("script,img,iframe,object,embed,a[href]").count() == 0
            assert not page.evaluate("Boolean(window.__s210Executed)")
            assert not external and not dialogs
            preview.get_by_text("代码", exact=True).click()
            source_ready(fixtures["document.md"].decode())
            preview.get_by_text("预览", exact=True).click()
            expect(rendered.get_by_role("heading", name="Preview heading")).to_be_visible()
            download_exact("document.md"); close_preview()
            open_preview("document.md")
            expect(rendered.get_by_role("heading", name="Preview heading")).to_be_visible(timeout=20000)
            checks.append("Markdown/default-rendered/GFM/table-task-fence/source-toggle/reopen-default/no-active-HTML-links-images")
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({"width": width, "height": 900})
                    expect(rendered).to_be_visible()
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width)
                    page.wait_for_timeout(200)
                    page.screenshot(path=str(evidence / f"markdown-{scheme}-{width}.png"))
            close_preview()
            page.set_viewport_size({"width": 1440, "height": 900})
            checks.append("Markdown/light-dark/320-1440/no-horizontal-page-overflow")

            open_preview("complex.md")
            expect(preview.get_by_text("Markdown 内容过于复杂，请切换代码模式或下载后查看。", exact=True)).to_be_visible(timeout=20000)
            expect(preview.locator(".cm-editor")).to_have_count(0)
            preview.get_by_text("代码", exact=True).click()
            expect(preview.locator(".cm-content")).to_be_visible(timeout=20000)
            expect(preview.locator(".cm-content .cm-line").first).to_have_text("```js")
            download_exact("complex.md"); close_preview()
            checks.append("Markdown/33-code-block-budget/no-embedded-editors/source-mode-still-available")

            for name in ("bad.txt", "binary.txt", "oversize.txt"):
                start = len(requests)
                open_preview(name)
                expect(preview.get_by_role("alert")).to_be_visible(timeout=20000)
                if name == "oversize.txt":
                    expect(preview.get_by_role("alert")).to_contain_text("2 MiB")
                    assert not any(f"/entries/{by_name[name]['entry_id']}/" in item["path"] and item["path"].endswith("/content") for item in requests[start:])
                else:
                    download_exact(name)
                assert preview.locator(".cm-editor").count() == 0
                close_preview()
            checks.append("invalid-encoding-binary/rejected/oversize-before-content-download")
            arm("corrupt", source["entry_id"])
            open_preview("example.js")
            expect(preview.get_by_role("alert")).to_contain_text("SHA-256", timeout=20000)
            arm()
            preview.get_by_role("button", name="重试预览", exact=True).click()
            source_ready(fixtures["example.js"].decode()); close_preview()
            checks.append("corrupt-fixed-version/hash-rejection/retry-recovers")

            def delayed():
                events = arm("delay", source["entry_id"])
                open_preview("example.js")
                wait_for(events[0].is_set, "content did not reach delay gate")
                return events

            _, release, finished = delayed()
            preview.get_by_role("button", name="取消预览", exact=True).click()
            expect(preview.get_by_role("alert")).to_contain_text("已取消")
            arm(); release.set(); wait_for(finished.is_set, "cancel response did not finish")
            page.wait_for_timeout(200)
            assert preview.locator(".cm-editor").count() == 0
            close_preview()
            _, release, finished = delayed()
            page.keyboard.press("Escape"); expect(preview).not_to_be_visible()
            arm(); open_preview("document.md")
            expect(rendered.get_by_role("heading", name="Preview heading")).to_be_visible(timeout=20000)
            release.set(); wait_for(finished.is_set, "late response did not finish")
            page.wait_for_timeout(200)
            expect(page.get_by_role("dialog", name="预览 · document.md", exact=True)).to_be_visible()
            expect(rendered.get_by_role("heading", name="Preview heading")).to_be_visible()
            close_preview()
            checks.append("cancel-close/abort/late-source-cannot-replace-new-Markdown")
            _, release, finished = delayed()
            arm(); page.evaluate("path=>location.hash=path", f"/files/{vault}/child")
            expect(preview).not_to_be_visible()
            expect(link("leaf.txt")).to_be_visible(timeout=20000)
            release.set(); wait_for(finished.is_set, "navigation response did not finish")
            open_preview("leaf.txt"); source_ready("child leaf text\n"); close_preview()
            checks.append("directory-navigation/in-flight-close/new-text-independent")
            page.evaluate("path=>location.hash=path", f"/files/{vault}")
            _, release, finished = delayed()
            arm(); page.evaluate("location.hash='/account'")
            expect(preview).not_to_be_visible()
            page.get_by_role("button", name="退出当前会话", exact=True).click()
            expect(page.get_by_role("button", name="登录", exact=True)).to_be_visible()
            release.set(); wait_for(finished.is_set, "logout response did not finish")
            page.wait_for_timeout(200)
            expect(page.get_by_role("dialog")).to_have_count(0)
            checks.append("logout/late-text-cannot-restore-authenticated-preview")
            assert not errors and not violations and not external and not dialogs, (errors, violations, external, dialogs)
            assert not any(item.get("proxy_error") for item in requests), requests
            assert not any(item["method"] != "GET" and ("/operations" in item["path"] or "/uploads" in item["path"]) for item in requests)
            assert all((documents / name).read_bytes() == data for name, data in fixtures.items())
            checks.append("no-pageerrors-CSP-external-network-dialogs-content-writes/files-unchanged")
            (evidence / "highlight-colors.json").write_text(json.dumps(highlights, indent=2))
    except Exception as cause:
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
            "binary_directory": str(BIN), "web_directory": str(WEB), "pageerrors": errors,
            "csp": violations, "external_requests": external, "dialogs": dialogs,
            "limitations": ["Chromium exercised; other browser engines not run.", "Text is read-only; no editing or save flow is provided."]}, ensure_ascii=False, indent=2))
    print(f"S210 text previews: {len(checks)} check groups passed; evidence: {evidence}", flush=True)


if __name__ == "__main__":
    main()
