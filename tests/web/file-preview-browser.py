#!/usr/bin/env python3
"""S209: built Web + Chromium + real isolated btrfs preview integration.

Run in `nix develop` after building Web. VAULTSYNCD_TEST_BIN_DIR and
VAULTSYNCD_TEST_WEB_DIR can select the exact deployed binaries/assets instead.
VAULTSYNCD_WEB_RUN_ROOT selects an existing writable directory beneath
/data/vaultsyncd-dev (default: /data/vaultsyncd-dev). All generated files,
browser profiles, downloads and caches stay in a unique S209 run directory.

Only explicitly armed proxy faults alter real responses: bad hash/oversize
snapshot metadata, corrupt/truncated content, or delayed content delivery.
Fixtures enter the service via real filesystem writes and the observer; no SQL,
production state, external network, browser API mocks are used.
`--check-fixtures` validates the deterministic PNG/PDF construction only; it is
NOT a substitute for the integration run or a claim that previews rendered.
"""
import argparse
import base64
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
import shutil
import struct
import subprocess
import threading
import time
from urllib.parse import urlsplit
import zlib

from playwright.sync_api import Error, expect, sync_playwright
from csp_support import CspStaticFiles, field_label
from preview_exit_support import PreviewExit

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug")).resolve()
WEB = Path(os.environ.get("VAULTSYNCD_TEST_WEB_DIR", REPO / "apps/web/dist")).resolve()
DEV = Path("/data/vaultsyncd-dev").resolve()
RUN_PARENT = Path(os.environ.get("VAULTSYNCD_WEB_RUN_ROOT", str(DEV))).resolve()
RUN = RUN_PARENT / f"S209-native-pdf-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"
PDF_NAME = "two pages & '文档'.pdf"
PNG_NAME = "bands & '图片'.png"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def png_fixture():
    """96x64 RGB PNG: opaque red upper half and blue lower half."""
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xffffffff)

    rows = b"".join(b"\0" + (b"\xff\0\0" if y < 32 else b"\0\0\xff") * 96 for y in range(64))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 96, 64, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


def raster_fixtures():
    # Small red/blue fixtures generated once from png_fixture with ImageMagick;
    # embedding bytes keeps the integration suite independent of image encoders.
    encoded = {
        "bands.jpg": "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgICAgMCAgIDAwMDBAYEBAQEBAgGBgUGCQgKCgkICQkKDA8MCgsOCwkJDRENDg8QEBEQCgwSExIQEw8QEBD/2wBDAQMDAwQDBAgEBAgQCwkLEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBD/wAARCABAAGADAREAAhEBAxEB/8QAFgABAQEAAAAAAAAAAAAAAAAAAAcI/8QAFBABAAAAAAAAAAAAAAAAAAAAAP/EABYBAQEBAAAAAAAAAAAAAAAAAAAJB//EABQRAQAAAAAAAAAAAAAAAAAAAAD/2gAMAwEAAhEDEQA/AM6MMVTAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAQJcZHQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB//9k=",
        "bands.gif": "R0lGODlhYABAAPAAAP8AAAAA/yH5BAAAAAAALAAAAABgAEAAAAKChI+py+0Po5y02ouz3rz7D4biSJbmiabqyrbuC8fyTNf2jef6zvf+DwwKh8Si8YhMKpfMpvMJjSYD1Kr1is1qt9yu9wsOi8fksvmMTqvX7Lb7DY/L5/S6/Y7P6/f8vv8PGCg4SFhoeIiYqLjI2Oj4CBkpOUlZaXmJmam5ydnp+XlZAAA7",
        "bands.webp": "UklGRnQAAABXRUJQVlA4IGgAAABwBQCdASpgAEAAPpFEnUmlpCMhKUgAsBIJYwDVAQA0AHyAAE6NTDspX/Bd162Az5MeR7nAAP7upj/1HFOerO/+5wPmW+Zb+T2S4MJtYFB/eUx/3OB+9b8iP8pnDcFFqQ8Hdt8ugAAAAA==",
        "bands.avif": "AAAAHGZ0eXBhdmlmAAAAAG1pZjFhdmlmbWlhZgAAANZtZXRhAAAAAAAAACFoZGxyAAAAAAAAAABwaWN0AAAAAAAAAAAAAAAAAAAAACJpbG9jAAAAAERAAAEAAQAAAAAA+gABAAAAAAAAADgAAAAjaWluZgAAAAAAAQAAABVpbmZlAgAAAAABAABhdjAxAAAAAA5waXRtAAAAAAABAAAAVmlwcnAAAAA4aXBjbwAAAAxhdjFDgQAMAAAAABRpc3BlAAAAAAAAAGAAAABAAAAAEHBpeGkAAAAAAwgICAAAABZpcG1hAAAAAAAAAAEAAQOBAgMAAABAbWRhdBIACgkYGW/+0QENBoQyKRSjw8MQw88880AAAM/MRSK+CzvyduH+OThZIJfMAOZP8IAtHHP0nWrr",
    }
    result = {name: base64.b64decode(value, validate=True) for name, value in encoded.items()}
    rows = b"".join((b"\xff\0\0" if y < 32 else b"\0\0\xff") * 96 for y in range(64))
    # 24-bit bottom-up BGR: blue rows are first in storage, red rows last.
    result["bands.bmp"] = (b"BM" + struct.pack("<IHHI", 54 + len(rows), 0, 0, 54)
                           + struct.pack("<IiiHHIIiiII", 40, 96, 64, 1, 24, 0, len(rows), 0, 0, 0, 0) + rows)
    return result


def pdf_fixture(active=False):
    """ASCII PDF with a real xref, two colored pages, and an optional action probe.

    The optional document action only assigns a marker, has no links/network,
    dialogs, filesystem actions or application commands. The native viewer owns
    document actions; the integration test only checks host-page isolation.
    """
    def stream(data):
        return f"<< /Length {len(data)} >>\nstream\n".encode() + data + b"\nendstream"

    objects = [
        b"<< /Type /Catalog /Pages 2 0 R" + (b" /OpenAction 7 0 R" if active else b"") + b" >>",
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
        f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 320] /Resources << /Font << /F1 {8 if active else 7} 0 R >> >> /Contents 5 0 R >>".encode(),
        f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 320] /Resources << /Font << /F1 {8 if active else 7} 0 R >> >> /Contents 6 0 R >>".encode(),
        stream(b"1 0 0 rg\n20 20 200 280 re f\n0 0 0 rg BT /F1 12 Tf 30 280 Td (S208 PDF text) Tj ET\n"),
        stream(b"0 0 1 rg\n20 20 200 280 re f\n0 0 0 rg BT /F1 12 Tf 30 280 Td (S208 PDF text) Tj ET\n"),
    ]
    if active:
        objects.append(b"<< /S /JavaScript /JS (globalThis.__s208PdfExecuted = true;) >>")
    objects.append(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
    result = bytearray(b"%PDF-1.4\n% S208 deterministic fixture\n")
    offsets = [0]
    for index, body in enumerate(objects, 1):
        offsets.append(len(result))
        result.extend(f"{index} 0 obj\n".encode() + body + b"\nendobj\n")
    start = len(result)
    result.extend(f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode())
    for offset in offsets[1:]:
        result.extend(f"{offset:010d} 00000 n \n".encode())
    result.extend(f"trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n".encode())
    return bytes(result)



def check_fixtures():
    for active in (False, True):
        data = pdf_fixture(active)
        start = int(re.search(rb"startxref\n(\d+)\n", data).group(1))
        assert data[start:].startswith(b"xref\n")
        table = data[start:].splitlines()
        count = int(table[1].split()[1])
        for index, line in enumerate(table[3:count + 2], 1):
            offset = int(line.split()[0])
            assert data[offset:].startswith(f"{index} 0 obj\n".encode())
        assert b"/Count 2" in data
        assert (b"/JavaScript" in data) == active
    data = png_fixture()
    position, rows = 8, None
    while position < len(data):
        length = struct.unpack(">I", data[position:position + 4])[0]
        kind = data[position + 4:position + 8]
        payload = data[position + 8:position + 8 + length]
        assert zlib.crc32(kind + payload) & 0xffffffff == struct.unpack(">I", data[position + 8 + length:position + 12 + length])[0]
        if kind == b"IDAT":
            rows = zlib.decompress(payload)
        position += 12 + length
    assert len(rows) == 64 * (1 + 96 * 3)
    assert rows[:4] == b"\0\xff\0\0" and rows[32 * 289:32 * 289 + 4] == b"\0\0\0\xff"
    print("S209 fixtures: PNG CRC/scanlines and PDF xrefs verified", flush=True)


def checked(args, **kwargs):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, **kwargs)
    assert result.returncode == 0, f"{args[0]} failed: {result.stderr}"
    return result.stdout


def main():
    assert RUN_PARENT.is_relative_to(DEV), "test artifacts must remain under /data/vaultsyncd-dev"
    assert RUN_PARENT.is_dir() and os.access(RUN_PARENT, os.W_OK), RUN_PARENT
    assert (WEB / "index.html").is_file(), f"build Web or set VAULTSYNCD_TEST_WEB_DIR: {WEB}"
    for name in ("vaultsyncd-admin", "vaultsyncd-server"):
        assert os.access(BIN / name, os.X_OK), f"set VAULTSYNCD_TEST_BIN_DIR: {BIN / name}"
    RUN.mkdir(mode=0o700)
    for name in ("tmp", "cache", "downloads", "evidence", "vaults", "state"):
        (RUN / name).mkdir(mode=0o700)
    # Full Chromium's singleton socket must fit the Unix 108-byte path limit.
    browser_tmp = DEV / "cache/tmp" / f"S209-{os.getpid()}"
    browser_tmp.mkdir(mode=0o700)
    os.environ.update(TMPDIR=str(browser_tmp), XDG_CACHE_HOME=str(RUN / "cache"))
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    evidence = RUN / "evidence"
    print(f"S209 artifacts: {RUN}", flush=True)
    documents, archive = RUN / "vaults/documents", RUN / "vaults/archive"
    documents.mkdir(); archive.mkdir(); (documents / "child").mkdir()
    fixtures = {
        PDF_NAME: pdf_fixture(), PNG_NAME: png_fixture(), "active.pdf": pdf_fixture(active=True), **raster_fixtures(),
        "broken.pdf": b"not a PDF document\n",
        "malformed.pdf": b"%PDF-1.4\nnot a valid document\n",
        "broken.png": b"\x89PNG\r\n\x1a\nnot a valid image\n",
        "unsupported.html": b"<!doctype html><script>globalThis.__s208UntrustedExecuted=true</script>",
        "unsupported.svg": b'<svg xmlns="http://www.w3.org/2000/svg" onload="globalThis.__s208UntrustedExecuted=true"><rect width="20" height="20"/></svg>',
        "oversize.png": png_fixture(), "hash.png": png_fixture(),
    }
    for name, data in fixtures.items():
        (documents / name).write_bytes(data)
    (documents / "child/leaf.png").write_bytes(png_fixture())
    (archive / "other.png").write_bytes(png_fixture())
    assets = {str(path.relative_to(WEB)): sha(path.read_bytes()) for path in sorted(WEB.rglob("*")) if path.is_file()}
    (evidence / "inputs.json").write_text(json.dumps({
        "binary_directory": str(BIN), "web_directory": str(WEB),
        "binaries": {name: sha((BIN / name).read_bytes()) for name in ("vaultsyncd-admin", "vaultsyncd-server")},
        "assets_sha256": assets,
        "fixtures": {name: {"size": len(data), "sha256": sha(data)} for name, data in fixtures.items()},
    }, ensure_ascii=False, indent=2))
    password = secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", RUN / "state", "--login", "owner"], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{RUN / "state"}"\n[vaults]\nallowed_bases=["{RUN / "vaults"}"]\n[log]\nformat="json"\nlevel="info"\n')
    log = (evidence / "server.log").open("w")
    service = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log, cwd=RUN)
    proxy = context = page = None
    requests, checks, errors, violations, external, dialogs, resources = [], [], [], [], [], [], []
    lock = threading.Lock()
    fault = {"mode": None, "entry_id": None}
    holds = []
    failure = None

    def arm(mode=None, entry_id=None):
        with lock:
            fault.clear()
            fault.update(mode=mode, entry_id=entry_id)
            if mode == "delay":
                started, release, finished = threading.Event(), threading.Event(), threading.Event()
                fault.update(started=started, release=release, finished=finished)
                holds.append(release)
                return started, release, finished

    @contextmanager
    def browser_runtime():
        with sync_playwright() as playwright:
            try:
                yield playwright
            except Exception:
                if page and not page.is_closed():
                    try:
                        page.screenshot(path=str(evidence / "failure.png"), full_page=True)
                        (evidence / "failure.html").write_text(page.content())
                    except Exception as capture_error:
                        errors.append(f"failure capture: {capture_error}")
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
            assert service.poll() is None, "service exited"
            time.sleep(.2)
        assert address, "service did not listen"

        class Handler(CspStaticFiles):
            def log_message(self, *_):
                pass

            def do_POST(self):
                self.forward()

            def do_GET(self):
                if self.path.startswith("/v1/"):
                    self.forward()
                else:
                    super().do_GET()

            def forward(self):
                backend = http.client.HTTPConnection(address, timeout=30)
                body = self.rfile.read(int(self.headers.get("content-length", "0")))
                headers = {key: value for key, value in self.headers.items() if key.lower() not in ("connection", "transfer-encoding")}
                path = urlsplit(self.path).path
                record = {"method": self.command, "path": self.path}
                with lock:
                    current = dict(fault)
                    requests.append(record)
                content = path.endswith("/content") and f"/entries/{current['entry_id']}/" in path
                try:
                    backend.request(self.command, self.path, body=body, headers=headers)
                    response = backend.getresponse(); payload = response.read()
                    record["status"] = response.status
                    mode = current["mode"]
                    if response.status == 200 and "/snapshots/" in path and path.endswith("/entries") and mode in ("oversize", "bad-hash"):
                        data = json.loads(payload)
                        for entry in data["entries"]:
                            if entry["entry_id"] == current["entry_id"]:
                                if mode == "oversize":
                                    entry["size"] = str(50 * 1024 ** 2 + 1)
                                else:
                                    entry["hash"] = "0" * 64
                                record["fault"] = mode
                        payload = json.dumps(data).encode()
                    if content and response.status in (200, 206):
                        record["fault"] = mode
                        if mode == "corrupt" and payload:
                            payload = payload[:-1] + bytes([payload[-1] ^ 1])
                        elif mode == "truncate":
                            payload = payload[:-1]
                        elif mode == "delay":
                            current["started"].set()
                            assert current["release"].wait(timeout=45), "delayed content not released"
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
                finally:
                    if content and current["mode"] == "delay":
                        current["finished"].set()
                    backend.close()

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=str(WEB)))
        proxy.daemon_threads = True
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        origin = f"http://localhost:{proxy.server_port}"
        with browser_runtime() as playwright:
            context = playwright.chromium.launch_persistent_context(
                user_data_dir=str(RUN / "browser-profile"), downloads_path=str(RUN / "downloads"),
                executable_path=shutil.which("chromium"), headless=True, accept_downloads=True, viewport={"width": 1440, "height": 900})
            context.expose_binding("s208CspViolation", lambda _source, detail: violations.append(detail))
            context.add_init_script("""document.addEventListener('securitypolicyviolation', e =>
                window.s208CspViolation({directive:e.effectiveDirective, blocked:e.blockedURI}));""")
            def observe_page(opened):
                opened.on("pageerror", lambda error: errors.append(str(error)))
            context.on("page", observe_page)
            for opened in context.pages:
                observe_page(opened)
            context.on("request", lambda request: external.append(request.url) if urlsplit(request.url).scheme in ("http", "https") and not request.url.startswith(origin + "/") else None)
            context.on("request", lambda request: resources.append(request.url) if "/assets/" in request.url else None)
            page = context.pages[0] if context.pages else context.new_page()
            def dismiss_dialog(dialog):
                dialogs.append(dialog.message)
                dialog.dismiss()
            page.on("dialog", dismiss_dialog)
            page.goto(origin + "/#/files")
            page.get_by_label(field_label("登录名")).fill("owner")
            page.get_by_label(field_label("密码")).fill(password)
            page.get_by_role("button", name="登录", exact=True).click()
            expect(page.get_by_role("combobox", name="浏览 Vault", exact=True)).to_be_visible(timeout=30000)

            def api(path, body=None):
                result = page.evaluate("""async ([path, body]) => {
                    const r = await fetch(path, body === null ? {} : {method:'POST',
                        headers:{'content-type':'application/json'}, body:JSON.stringify(body)});
                    return {status:r.status, body:await r.json()};
                }""", [path, body])
                assert result["status"] in (200, 201), result
                return result["body"]

            vault = api("/v1/admin/vaults", {"path": str(documents)})["vault_id"]
            other = api("/v1/admin/vaults", {"path": str(archive)})["vault_id"]

            def wait_for(read, message):
                for _ in range(240):
                    value = read()
                    if value:
                        return value
                    page.wait_for_timeout(250)
                raise AssertionError(message)

            wait_for(lambda: all(item["collection"]["state"] == "complete" for item in api("/v1/admin/vaults")["vaults"]), "real observer did not complete import")
            metadata = api(f"/v1/vaults/{vault}/snapshots", {})
            entries = api(f"/v1/vaults/{vault}/snapshots/{metadata['snapshot_id']}/entries?limit=1000")["entries"]
            by_name = {entry["name"]: entry for entry in entries}
            for name, data in fixtures.items():
                assert by_name[name]["hash"] == sha(data) and by_name[name]["size"] == str(len(data)), by_name[name]
            (evidence / "real-entries.json").write_text(json.dumps(entries, ensure_ascii=False, indent=2))
            checks.append("real-observer/committed-fixture-hashes")
            private_path = f"/v1/vaults/{vault}/entries/{by_name[PDF_NAME]['entry_id']}/versions/{by_name[PDF_NAME]['version_id']}/content"
            anonymous = page.evaluate("async path => { const r=await fetch(path,{credentials:'omit'}); return r.status; }", private_path)
            assert anonymous == 401, anonymous
            checks.append("fixed-version-content/anonymous-request-denied")
            page.goto(origin + f"/#/files/{vault}"); page.reload()
            table = page.get_by_role("table", name="文件与目录", exact=True)
            preview = page.get_by_role("dialog")
            exits = PreviewExit(page, evidence)

            def link(name):
                return table.get_by_role("button", name=name, exact=True)

            def open_preview(name, menu=False, keyboard=False):
                expect(link(name)).to_be_visible(timeout=20000)
                if menu or keyboard:
                    if keyboard:
                        link(name).focus(); page.keyboard.press("Shift+F10")
                    else:
                        link(name).click(button="right")
                    page.get_by_role("menuitem", name="预览", exact=True).click()
                else:
                    link(name).click()
                expect(preview).to_be_visible()
                expect(page.get_by_role("dialog", name=f"预览 · {name}", exact=True)).to_be_visible()
                expect(preview.get_by_role("button", name="关闭", exact=True)).to_have_count(0)
                header = preview.locator(".mantine-Modal-header")
                download = header.get_by_role("button", name="下载原文件", exact=True)
                expect(download).to_have_attribute("title", "下载原文件")
                expect(download).to_have_text("")
                expect(download.locator("svg.lucide-cloud-download")).to_have_count(1)
                expect(header.get_by_role("button", name="关闭预览", exact=True)).to_be_focused()

            def compact_spacing(selector):
                content = preview.locator(selector)
                expect(content).to_be_visible()
                # Measure the actual title-to-content distance; an empty action
                # row must not reserve a Stack gap after loading completes.
                wait_for(lambda: not preview.get_by_role("status").count(), "preview still loading")
                spacing = content.evaluate("""node => {
                    const dialog = node.closest('[role="dialog"]');
                    const title = dialog.querySelector('.mantine-Modal-title').getBoundingClientRect();
                    const content = node.getBoundingClientRect();
                    return {gap: content.top - title.bottom,
                        emptyGroups: [...dialog.querySelectorAll('.mantine-Group-root')].filter(n => !n.children.length).length};
                }""")
                assert 0 <= spacing["gap"] <= 12, spacing
                assert spacing["emptyGroups"] == 0, spacing

            def close_preview(escape=False):
                if escape:
                    page.keyboard.press("Escape")
                else:
                    preview.get_by_role("button", name="关闭预览", exact=True).click()
                expect(preview).not_to_be_visible()

            def pixels():
                canvas = preview.locator("canvas").first
                expect(canvas).to_be_visible(timeout=20000)
                return canvas.evaluate("""canvas => {
                    if (!canvas.width || !canvas.height) return null;
                    const data = canvas.getContext('2d').getImageData(0,0,canvas.width,canvas.height).data;
                    let red=0, blue=0, opaque=0;
                    for (let i=0; i<data.length; i+=4) {
                        if(data[i+3]>200) opaque++;
                        if(data[i+3]>200 && data[i]>180 && data[i+1]<80 && data[i+2]<80) red++;
                        if(data[i+3]>200 && data[i+2]>180 && data[i]<80 && data[i+1]<80) blue++;
                    }
                    return {width:canvas.width,height:canvas.height,red,blue,opaque};
                }""")

            def rendered(red=True, blue=True):
                def ready():
                    value = pixels()
                    if not value:
                        return None
                    total = value["width"] * value["height"]
                    if value["opaque"] < total / 2:
                        return None
                    if red and value["red"] < total / 10:
                        return None
                    if blue and value["blue"] < total / 10:
                        return None
                    return value
                return wait_for(ready, "canvas never rendered expected real colored pixels")

            baseline = page.url
            open_preview(PNG_NAME)
            png_pixels = rendered()
            assert page.url == baseline
            expect(preview.locator("iframe, object, embed")).to_have_count(0)
            checks.append("PNG/click/in-place/decoded-canvas-pixels")
            exits.close("canvas", "image-X/pixels-title-geometry-retained")
            open_preview(PNG_NAME); rendered()
            exits.reopen("canvas", PNG_NAME, "image/rapid-same-file-reopen")
            rendered()
            exits.close("canvas", "image-overlay/pixels-title-geometry-retained", "overlay")
            checks.append("exit-animation/image-X-overlay/real-intermediate-frames/pixels-title-geometry/rapid-same-file-reopen")
            for name in raster_fixtures():
                open_preview(name)
                rendered()
                close_preview()
            checks.append("JPEG-GIF-WebP-BMP-AVIF/real-decoders/colored-canvas-pixels")
            open_preview(PNG_NAME); rendered()
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({"width": width, "height": 900})
                    rendered()
                    compact_spacing(".file-preview-viewport")
                    box = preview.bounding_box()
                    assert box and box["x"] >= -1 and box["x"] + box["width"] <= width + 1, box
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), f"horizontal overflow at {scheme}/{width}"
                    header = preview.locator(".mantine-Modal-header")
                    title = header.get_by_role("heading")
                    original_title = title.inner_text()
                    title.evaluate("node => node.textContent = '长文件名称'.repeat(40) + '.png'")
                    download_box = header.get_by_role("button", name="下载原文件", exact=True).bounding_box()
                    close_box = header.get_by_role("button", name="关闭预览", exact=True).bounding_box()
                    title_box = title.bounding_box()
                    assert title_box and download_box and close_box
                    assert title_box["x"] + title_box["width"] <= download_box["x"] + 1
                    assert download_box["x"] + download_box["width"] <= close_box["x"] + 1
                    assert close_box["x"] + close_box["width"] <= width + 1
                    title.evaluate("(node, value) => node.textContent = value", original_title)
                    page.wait_for_timeout(250)  # Capture after Mantine's enter transition.
                    page.screenshot(path=str(evidence / f"image-{scheme}-{width}.png"))
            page.set_viewport_size({"width": 1440, "height": 900})
            close_preview(escape=True)
            open_preview(PNG_NAME, keyboard=True); rendered(); close_preview()
            checks.append("preview/keyboard-context-menu/Escape/light-dark-320-1440")

            open_preview(PDF_NAME, menu=True)
            def native_pdf():
                expect(preview.locator("iframe.file-preview-pdf")).to_be_visible(timeout=20000)
                expect(preview.locator("canvas, object, embed")).to_have_count(0)
                frame = wait_for(lambda: next((f for f in page.frames if f.url.startswith("chrome-extension://") and f.locator("pdf-viewer").count()), None), "browser PDF viewer did not load")
                expect(frame.locator("pdf-viewer")).to_be_visible(timeout=20000)
                wait_for(lambda: frame.locator("pdf-viewer").evaluate("v => v.loadProgress === 100 || v.loadProgress_ === 100"), "browser PDF did not finish loading")
                return frame

            native = native_pdf()
            # Browser-owned toolbar and plugin are inspected through Playwright;
            # the application never reaches into the extension or parses pages.
            native_state = native.locator("pdf-viewer").evaluate("v => ({dimensions:v.documentDimensions, progress:v.loadProgress_, html:v.shadowRoot.innerHTML})")
            assert len(native_state["dimensions"]["pageDimensions"]) == 2, native_state
            selector = native.locator("input#pageSelector")
            selector.fill("2"); selector.press("Enter")
            expect(selector).to_have_value("2")
            selector.fill("1"); selector.press("Enter")
            expect(selector).to_have_value("1")
            zoom = native.locator("viewer-toolbar input").first
            zoom.fill("150%"); zoom.press("Enter")
            expect(zoom).to_have_value("150%")
            wait_for(lambda: native.locator("pdf-viewer").evaluate("v => v.viewport_.getZoom() > 1"), "native zoom did not take effect")
            zoom.fill("100%"); zoom.press("Enter")
            expect(zoom).to_have_value("100%")
            (evidence / "native-viewer.json").write_text(json.dumps(native_state, ensure_ascii=False, indent=2))
            page.screenshot(path=str(evidence / "native-pdf.png"))
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 1440):
                    page.set_viewport_size({"width": width, "height": 900})
                    native_pdf()
                    compact_spacing(".file-preview-pdf")
                    box = preview.bounding_box()
                    assert box and box["x"] >= -1 and box["x"] + box["width"] <= width + 1, box
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
                    page.wait_for_timeout(250)  # Capture after Mantine's enter transition.
                    page.screenshot(path=str(evidence / f"pdf-{scheme}-{width}.png"))
            page.set_viewport_size({"width": 1440, "height": 900})
            revoked_url = preview.locator("iframe.file-preview-pdf").get_attribute("src")
            exits.close("iframe.file-preview-pdf", "PDF-Escape/frame-Blob-title-geometry-retained", "escape")
            checks.append("exit-animation/PDF-Escape/real-intermediate-frames/live-Blob-stable-frame-title-geometry")
            revoked = context.new_page()
            try:
                revoked.goto(revoked_url)
                raise AssertionError("closed PDF Blob URL is still readable")
            except Error as error:
                assert "ERR_FILE_NOT_FOUND" in str(error), error
            finally:
                revoked.close()
            assert not any("pdfjs" in url for url in resources), "application PDF.js assets must be removed"
            checks.append("PDF/browser-native-viewer/two-pages/toolbar-page-and-zoom/closed-Blob-revoked/CSP-layout")

            open_preview("active.pdf"); native_pdf()
            assert not page.evaluate("Boolean(window.__s208PdfExecuted || window.__s208UntrustedExecuted)")
            close_preview()
            checks.append("PDF/browser-owned-document-actions/do-not-affect-host-page")
            for name in ("unsupported.html", "unsupported.svg"):
                open_preview(name, menu=True)
                expect(preview.locator(".cm-content")).to_contain_text("globalThis.__s208UntrustedExecuted=true")
                expect(preview.locator("canvas, iframe, object, embed, script, img")).to_have_count(0)
                close_preview()
            assert not page.evaluate("Boolean(window.__s208UntrustedExecuted)")
            link("child").click(button="right")
            expect(page.get_by_role("menuitem", name="预览", exact=True)).to_have_count(0)
            page.keyboard.press("Escape")
            checks.append("HTML-SVG/source-only/no-active-content/directory-no-preview")

            def download_exact(name):
                with page.expect_download() as pending:
                    preview.get_by_role("button", name="下载原文件", exact=True).click()
                saved = pending.value
                assert saved.suggested_filename == name and saved.failure() is None
                target = RUN / "downloads" / name
                saved.save_as(target)
                assert target.read_bytes() == fixtures[name], name

            for name in (PDF_NAME, PNG_NAME):
                open_preview(name)
                native_pdf() if name == PDF_NAME else rendered()
                download_exact(name); close_preview()
            for name in ("broken.pdf", "broken.png"):
                open_preview(name)
                expect(preview.get_by_role("alert")).to_be_visible(timeout=20000)
                preview.get_by_role("button", name="重试预览", exact=True).click()
                expect(preview.get_by_role("alert")).to_be_visible(timeout=20000)
                download_exact(name); close_preview()
            checks.append("corrupt-PDF-image/error/retry/download-original-bytes")

            open_preview("malformed.pdf")
            expect(preview.locator("iframe.file-preview-pdf")).to_be_visible(timeout=20000)
            # A valid signature is enough to hand off; errors inside the native
            # reader belong to the browser, while original download stays usable.
            download_exact("malformed.pdf"); close_preview()
            checks.append("PDF/malformed-document/handed-to-native-reader/download-original")

            for mode in ("corrupt", "truncate"):
                arm(mode, by_name[PNG_NAME]["entry_id"])
                start = len(requests)
                open_preview(PNG_NAME)
                expect(preview.get_by_role("alert")).to_be_visible(timeout=20000)
                # Chromium may reject a truncated 206 in its network stack before
                # the application's length checker receives the response body.
                expect(preview.get_by_role("alert")).to_contain_text("SHA-256" if mode == "corrupt" else re.compile("长度|Failed to fetch"))
                assert any(item.get("fault") == mode for item in requests[start:]), requests[start:]
                arm()
                preview.get_by_role("button", name="重试预览", exact=True).click()
                rendered(); close_preview()
            checks.append("real-content/hash-corruption-and-short-response/retry-recovers")

            for mode, name in (("oversize", "oversize.png"), ("bad-hash", "hash.png")):
                arm(mode, by_name[name]["entry_id"])
                page.reload()
                start = len(requests)
                open_preview(name)
                expect(preview.get_by_role("alert")).to_be_visible(timeout=20000)
                assert any(item.get("fault") == mode for item in requests), f"{mode} metadata fault was not applied"
                if mode == "oversize":
                    expect(preview.get_by_role("alert")).to_contain_text("50 MiB")
                    assert not any(f"/entries/{by_name[name]['entry_id']}/" in item["path"] and item["path"].endswith("/content") for item in requests[start:]), "oversize preview downloaded content"
                else:
                    expect(preview.get_by_role("alert")).to_contain_text("SHA-256")
                close_preview(); arm(); page.reload()
                open_preview(name); rendered(); close_preview()
            checks.append("controlled-snapshot/oversize-rejected-before-content/bad-hash/reload-recovers")

            def delayed_preview():
                events = arm("delay", by_name[PDF_NAME]["entry_id"])
                open_preview(PDF_NAME)
                wait_for(events[0].is_set, "real PDF content request did not reach delay gate")
                return events

            _, release, finished = delayed_preview()
            exits.reopen(".mantine-Stack-root", PNG_NAME, "delayed-PDF/rapid-different-file-reopen", require_abort=True)
            arm(); rendered()
            release.set()
            wait_for(finished.is_set, "late response did not finish")
            # Let the browser process the delivered response/abort before checking
            # that the newly opened image did not inherit the old PDF render.
            page.wait_for_timeout(200)
            expect(page.get_by_role("dialog", name=f"预览 · {PNG_NAME}", exact=True)).to_be_visible()
            rendered(); close_preview()
            checks.append("close-in-flight/abort/late-PDF-cannot-replace-new-image")

            _, release, finished = delayed_preview()
            arm()
            page.evaluate("path => { location.hash = path; }", f"/files/{vault}/child")
            expect(preview).not_to_be_visible()
            expect(link("leaf.png")).to_be_visible()
            release.set(); wait_for(finished.is_set, "navigation-delayed response did not finish")
            open_preview("leaf.png"); rendered()
            page.evaluate("path => { location.hash = path; }", f"/files/{other}")
            expect(preview).not_to_be_visible()
            expect(link("other.png")).to_be_visible()
            open_preview("other.png"); rendered(); close_preview()
            checks.append("directory-navigation/in-flight-close/vault-change/unmount")

            # Disable Chromium's viewer with a real browser preference, not a JS
            # test double, and verify that the application retains its download.
            disabled_profile = RUN / "browser-pdf-disabled"
            (disabled_profile / "Default").mkdir(parents=True)
            (disabled_profile / "Default/Preferences").write_text(json.dumps({"plugins": {"always_open_pdf_externally": True}}))
            disabled = playwright.chromium.launch_persistent_context(
                user_data_dir=str(disabled_profile), executable_path=shutil.which("chromium"), headless=True,
                downloads_path=str(RUN / "downloads"), accept_downloads=True)
            disabled_page = disabled.pages[0]
            disabled_page.goto(origin + f"/#/files/{vault}")
            assert disabled_page.evaluate("navigator.pdfViewerEnabled") is False
            disabled_page.get_by_label(field_label("登录名")).fill("owner")
            disabled_page.get_by_label(field_label("密码")).fill(password)
            disabled_page.get_by_role("button", name="登录", exact=True).click()
            disabled_page.get_by_role("table", name="文件与目录", exact=True).get_by_role("button", name=PDF_NAME, exact=True).click()
            fallback = disabled_page.get_by_role("dialog", name=f"预览 · {PDF_NAME}", exact=True)
            expect(fallback.get_by_role("alert")).to_contain_text("浏览器未启用 PDF 预览", timeout=20000)
            expect(fallback.locator("iframe")).to_have_count(0)
            with disabled_page.expect_download() as pending:
                fallback.get_by_role("button", name="下载原文件", exact=True).click()
            target = RUN / "downloads/disabled-viewer.pdf"
            pending.value.save_as(target)
            assert target.read_bytes() == fixtures[PDF_NAME]
            disabled.close()
            checks.append("PDF/native-viewer-disabled/real-browser-preference/fallback-and-original-download")

            page.evaluate("path => { location.hash = path; }", f"/files/{vault}")
            _, release, finished = delayed_preview()
            arm()
            page.evaluate("location.hash = '/account'")
            expect(preview).not_to_be_visible()
            page.get_by_role("button", name="退出当前会话", exact=True).click()
            expect(page.get_by_role("button", name="登录", exact=True)).to_be_visible()
            release.set(); wait_for(finished.is_set, "logout-delayed response did not finish")
            page.wait_for_timeout(200)
            expect(page.get_by_role("dialog")).to_have_count(0)
            expect(page.get_by_role("button", name="登录", exact=True)).to_be_visible()
            checks.append("logout/late-preview-cannot-restore-authenticated-content")
            assert not errors, errors
            assert not violations, violations
            assert not external, external
            assert not dialogs, dialogs
            assert not [item for item in requests if item.get("proxy_error")], requests
            assert not [item for item in requests if item["method"] != "GET" and ("/operations" in item["path"] or "/uploads" in item["path"])], "preview wrote file content"
            checks.append("no-pageerrors/CSP-violations/external-network/script-dialogs/content-writes")
            (evidence / "pixels.json").write_text(json.dumps({"png": png_pixels}, indent=2))
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
        (evidence / "result.json").write_text(json.dumps({
            "passed": failure is None, "failure": failure, "checks": checks,
            "binary_directory": str(BIN), "web_directory": str(WEB),
            "pageerrors": errors, "csp": violations, "external_requests": external, "dialogs": dialogs, "asset_requests": resources,
            "limitations": ["Animated image playback is not tested; previews show a static frame.",
                            "Oversize and bad-hash metadata are controlled proxy responses, not huge real files.",
                            "PDF rendering, actions, passwords and decoder budgets are browser-owned; only Chromium native viewer is exercised."],
        }, ensure_ascii=False, indent=2))
    print(f"S209 preview: {len(checks)} check groups passed; evidence: {evidence}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-fixtures", action="store_true")
    arguments = parser.parse_args()
    check_fixtures()
    if not arguments.check_fixtures:
        main()
