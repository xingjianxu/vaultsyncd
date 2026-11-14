#!/usr/bin/env python3
"""S220 suffix-only icons on real committed files, native server and Chromium.

Run: nix develop .#web-tests --command python3 tests/web/file-icons-browser.py
VAULTSYNCD_TEST_BIN_DIR and VAULTSYNCD_TEST_WEB_DIR select exact binaries/assets.
VAULTSYNCD_WEB_RUN_ROOT selects an existing evidence parent under the dev root.
Fixtures deliberately contain plain bytes, not valid media/documents. No file is
opened, previewed, downloaded or executed by the browser.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import time
from urllib.parse import urlsplit

from playwright.sync_api import expect, sync_playwright
from csp_support import field_label

REPO = Path(__file__).resolve().parents[2]
DEV = Path("/data/vaultsyncd-dev").resolve()
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug")).resolve()
WEB = Path(os.environ.get("VAULTSYNCD_TEST_WEB_DIR", REPO / "apps/web/dist")).resolve()
PARENT = Path(os.environ.get("VAULTSYNCD_WEB_RUN_ROOT", DEV / "S220-file-type-icons-20261113")).resolve()
RUN = PARENT / f"S220-icons-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"
PAYLOAD = b"S220 plain test bytes; the filename alone selects the icon.\n"
FOLDER_PATH = "M3 7h6l2 2h10v11H3z M3 7V4h6l2 3"
FILE_PATH = "M6 3h8l4 4v14H6z M14 3v5h4"
# Expected Lucide classes from FileIcon.tsx, independent of MIME/content.
CATEGORIES = {
    "text": ("docx", "lucide-file-text"),
    "pdf": ("pdf", "lucide-file-type"),
    "spreadsheet": ("xlsx", "lucide-file-spreadsheet"),
    "presentation": ("pptx", "lucide-file-chart-column"),
    "image": ("png", "lucide-file-image"),
    "audio": ("mp3", "lucide-file-audio"),
    "video": ("mp4", "lucide-file-video"),
    "archive": ("zip", "lucide-file-archive"),
    "code": ("tsx", "lucide-file-code-2"),
    "json": ("json", "lucide-file-json"),
    "config": ("toml", "lucide-file-cog"),
    "script": ("ps1", "lucide-file-terminal"),
    "font": ("woff2", "lucide-file-type-2"),
    "database": ("sqlite3", "lucide-database"),
}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def checked(args, **kwargs):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, **kwargs)
    assert result.returncode == 0, f"{args[0]} failed: {result.stderr}"
    return result.stdout


def binary_input(path):
    data = path.read_bytes()
    result = {"path": str(path), "sha256": sha(data)}
    # Nix bin/ entries may be wrappers: retain both the entry point and ELF hash.
    if data.startswith(b"#!"):
        target = re.search(rb'^exec "(/nix/store/[^"\n]+)"', data, re.MULTILINE)
        if target:
            executable = Path(os.fsdecode(target.group(1))).resolve()
            result["wrapped_executable"] = {"path": str(executable), "sha256": sha(executable.read_bytes())}
    return result


def main():
    assert PARENT.is_relative_to(DEV) and PARENT.is_dir() and os.access(PARENT, os.W_OK), PARENT
    assert (WEB / "index.html").is_file(), WEB
    assert all(os.access(BIN / name, os.X_OK) for name in ("vaultsyncd-admin", "vaultsyncd-server")), BIN
    chromium = shutil.which("chromium")
    assert chromium, "Chromium must be supplied by the web-tests devShell"
    RUN.mkdir(mode=0o700)
    for name in ("tmp", "cache", "downloads", "evidence", "vaults", "state"):
        (RUN / name).mkdir(mode=0o700)
    browser_tmp = DEV / "cache/tmp/S220"
    browser_tmp.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.environ.update(TMPDIR=str(browser_tmp), XDG_CACHE_HOME=str(RUN / "cache"))
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    evidence, documents = RUN / "evidence", RUN / "vaults/documents"
    documents.mkdir()
    (documents / "folder.pdf").mkdir()
    (documents / "folder.pdf/leaf.txt").write_bytes(PAYLOAD)
    expected = {}
    for category, (extension, icon_class) in CATEGORIES.items():
        expected[f"sample-{category}.{extension}"] = icon_class
        expected[f"UPPER-{category}.{extension.upper()}"] = icon_class
    expected.update({
        "bundle.tar.gz": "lucide-file-archive",
        "report.pdf.txt": "lucide-file-text",
        "report.txt.PDF": "lucide-file-type",
        ".config.json": "lucide-file-json",
        "unknown.unlisted": None,
        "README": None,
        ".pdf": None,
        ".env": None,
        "report.pdf.unknown": None,
    })
    for name in expected:
        (documents / name).write_bytes(PAYLOAD)
    inputs = {
        "binary_directory": str(BIN), "web_directory": str(WEB),
        "binaries": {name: binary_input(BIN / name) for name in ("vaultsyncd-admin", "vaultsyncd-server")},
        "assets_sha256": {str(path.relative_to(WEB)): sha(path.read_bytes()) for path in sorted(WEB.rglob("*")) if path.is_file()},
        "fixtures": {name: {"size": len(PAYLOAD), "sha256": sha(PAYLOAD), "expected_class": icon_class} for name, icon_class in expected.items()},
        "chromium": chromium,
    }
    (evidence / "inputs.json").write_text(json.dumps(inputs, ensure_ascii=False, indent=2))
    print(f"S220 artifacts: {RUN}", flush=True)
    password = secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", RUN / "state", "--login", "owner"], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{RUN / "state"}"\n[vaults]\nallowed_bases=["{RUN / "vaults"}"]\n[log]\nformat="json"\nlevel="info"\n')
    log = (evidence / "server.log").open("w")
    service = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)],
        env={**os.environ, "VAULTSYNCD_WEB_DIR": str(WEB)}, stdout=log, stderr=log, cwd=RUN)
    checks, requests, errors, violations, external, dialogs, downloads, layouts = [], [], [], [], [], [], [], []
    failure = None
    page = context = None
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
        origin = f"http://localhost:{address.rsplit(':', 1)[1]}"
        with sync_playwright() as playwright:
            context = playwright.chromium.launch_persistent_context(user_data_dir=str(RUN / "browser-profile"),
                downloads_path=str(RUN / "downloads"), executable_path=chromium,
                headless=True, accept_downloads=False, viewport={"width": 1440, "height": 1000})
            try:
                context.expose_binding("s220CspViolation", lambda _source, detail: violations.append(detail))
                context.add_init_script("document.addEventListener('securitypolicyviolation', e => window.s220CspViolation({directive:e.effectiveDirective,blocked:e.blockedURI}));")
                context.on("request", lambda request: requests.append({"method": request.method, "url": request.url}))
                context.on("request", lambda request: external.append(request.url) if urlsplit(request.url).scheme in ("http", "https") and not request.url.startswith(origin + "/") else None)
                context.on("page", lambda opened: opened.on("pageerror", lambda error: errors.append(str(error))))
                page = context.pages[0] if context.pages else context.new_page()
                page.on("pageerror", lambda error: errors.append(str(error)))
                page.on("dialog", lambda dialog: (dialogs.append(dialog.message), dialog.dismiss()))
                page.on("download", lambda download: downloads.append(download.suggested_filename))
                response = page.goto(origin + "/#/files")
                assert response and response.ok
                policy = response.headers.get("content-security-policy", "")
                assert "style-src 'self' 'nonce-" in policy and "script-src 'self'" in policy, policy
                assert "'unsafe-inline'" not in policy and "'unsafe-eval'" not in policy, policy
                (evidence / "csp.json").write_text(json.dumps({"origin": origin, "policy": policy}, indent=2))
                page.get_by_label(field_label("登录名")).fill("owner")
                page.get_by_label(field_label("密码")).fill(password)
                page.get_by_role("button", name="登录", exact=True).click()
                expect(page.get_by_role("combobox", name="浏览 Vault", exact=True)).to_be_visible(timeout=30000)

                def api(path, body=None):
                    result = page.evaluate("""async ([path,body])=>{const r=await fetch(path,body===null?{}:{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});return {status:r.status,body:await r.json()}}""", [path, body])
                    assert result["status"] in (200, 201), result
                    return result["body"]

                vault = api("/v1/admin/vaults", {"path": str(documents)})["vault_id"]
                for _ in range(240):
                    managed = api("/v1/admin/vaults")["vaults"]
                    if any(item["vault_id"] == vault and item["collection"]["state"] == "complete" for item in managed):
                        break
                    page.wait_for_timeout(250)
                else:
                    (evidence / "observer.json").write_text(json.dumps(managed, ensure_ascii=False, indent=2))
                    raise AssertionError(f"real observer did not complete initial import: {managed}")
                snapshot = api(f"/v1/vaults/{vault}/snapshots", {})
                entries = api(f"/v1/vaults/{vault}/snapshots/{snapshot['snapshot_id']}/entries?limit=1000")["entries"]
                by_name = {entry["name"]: entry for entry in entries}
                for name in (*expected, "leaf.txt"):
                    assert by_name[name]["hash"] == sha(PAYLOAD) and by_name[name]["size"] == str(len(PAYLOAD)), by_name[name]
                assert by_name["folder.pdf"]["kind"] == "directory", by_name["folder.pdf"]
                checks.append("real-admin-login/API-registration/observer-complete/committed-hash-size")
                page.goto(origin + f"/#/files/{vault}")
                page.reload()  # Refresh the Vault list cached before API registration.
                table = page.get_by_role("table", name="文件与目录", exact=True)
                expect(table.locator("tbody tr")).to_have_count(len(expected) + 1, timeout=30000)

                def icon(name):
                    return table.get_by_role("button", name=name, exact=True).locator("svg.files-icon")

                for scheme in ("light", "dark"):
                    page.emulate_media(color_scheme=scheme)
                    expect(page.locator("html")).to_have_attribute("data-mantine-color-scheme", scheme)
                    for width in (320, 1440):
                        page.set_viewport_size({"width": width, "height": 1000})
                        page.wait_for_timeout(200)
                        baseline = icon("README").evaluate("e => ({width:e.getBoundingClientRect().width,height:e.getBoundingClientRect().height,shrink:getComputedStyle(e).flexShrink})")
                        assert baseline == {"width": 18, "height": 18, "shrink": "0"}, baseline
                        for name, icon_class in expected.items():
                            svg = icon(name)
                            expect(svg).to_have_count(1)
                            expect(svg).to_have_attribute("aria-hidden", "true")
                            classes = (svg.get_attribute("class") or "").split()
                            if icon_class:
                                assert icon_class in classes and "lucide" in classes, (name, classes)
                            else:
                                assert not any(value.startswith("lucide") for value in classes), (name, classes)
                                expect(svg.locator("path")).to_have_attribute("d", FILE_PATH)
                            dimensions = svg.evaluate("e => ({width:e.getBoundingClientRect().width,height:e.getBoundingClientRect().height,shrink:getComputedStyle(e).flexShrink})")
                            assert dimensions == baseline, (scheme, width, name, dimensions, baseline)
                        folder = icon("folder.pdf")
                        expect(folder).to_have_attribute("aria-hidden", "true")
                        expect(folder.locator("path")).to_have_attribute("d", FOLDER_PATH)
                        assert "lucide" not in (folder.get_attribute("class") or "").split()
                        overflow = page.evaluate("({viewport:innerWidth,document:document.documentElement.scrollWidth,body:document.body.scrollWidth})")
                        assert overflow["document"] <= width and overflow["body"] <= width, (scheme, width, overflow)
                        expect(page.get_by_role("dialog")).to_have_count(0)
                        page.screenshot(path=str(evidence / f"icons-{scheme}-{width}.png"), full_page=True)
                        layouts.append({"scheme": scheme, "width": width, "overflow": overflow, "icon_dimensions": baseline, "files_checked": len(expected)})
                checks.extend(["14-suffix-categories/lowercase-uppercase/plain-bytes", "last-suffix/multi-suffix/unknown/no-extension/dotfiles", "folder.pdf-remains-folder", "svg/aria-hidden/files-icon/18px-generic-parity/no-shrink", "light-dark/320-1440/screenshots/no-page-overflow"])
                page.wait_for_timeout(300)
                assert not errors and not violations and not external and not dialogs and not downloads, (errors, violations, external, dialogs, downloads)
                forbidden = [item for item in requests if any(part in urlsplit(item["url"]).path for part in ("/content", "/preview", "/uploads", "/operations"))]
                assert not forbidden, forbidden
                allowed_posts = {"/v1/auth/browser/login", "/v1/auth/browser/refresh", "/v1/admin/vaults", f"/v1/vaults/{vault}/snapshots"}
                assert all(item["method"] == "GET" or (item["method"] == "POST" and urlsplit(item["url"]).path in allowed_posts) for item in requests), requests
                assert all((documents / name).read_bytes() == PAYLOAD for name in (*expected, "folder.pdf/leaf.txt"))
                checks.append("native-server-CSP/no-pageerror/no-content-preview-download-execution/no-writes")
                # Verify the exact asset tree did not change under the running test.
                assert inputs["assets_sha256"] == {str(path.relative_to(WEB)): sha(path.read_bytes()) for path in sorted(WEB.rglob("*")) if path.is_file()}
            except Exception:
                if page and not page.is_closed():
                    page.screenshot(path=str(evidence / "failure.png"), full_page=True)
                    (evidence / "failure.html").write_text(page.content())
                raise
            finally:
                context.close()
    except Exception as cause:
        failure = f"{type(cause).__name__}: {cause}"
        raise
    finally:
        service.terminate()
        try:
            service.wait(timeout=30)
        except subprocess.TimeoutExpired:
            service.kill(); service.wait(timeout=10)
        log.close()
        (evidence / "requests.json").write_text(json.dumps(requests, ensure_ascii=False, indent=2))
        (evidence / "result.json").write_text(json.dumps({"passed": failure is None, "failure": failure,
            "binary_directory": str(BIN), "web_directory": str(WEB), "checks": checks,
            "layouts": layouts, "pageerrors": errors, "csp_violations": violations,
            "external_requests": external, "dialogs": dialogs, "downloads": downloads,
            "limitations": ["Chromium only; icon checks do not assert content validity or preview eligibility."]}, ensure_ascii=False, indent=2))
    print(f"S220 file icons: {len(checks)} check groups passed; evidence: {evidence}", flush=True)


if __name__ == "__main__":
    main()
