#!/usr/bin/env python3
"""S134: built Web + real Chromium + isolated btrfs service, no API fixtures.

VAULTSYNCD_TEST_BIN_DIR=... nix develop .#web-tests --command python3 tests/web/files-browser.py
Network faults only affect this run's proxy; no online SQL or production writes.
"""
import functools
import hashlib
import http.client
import http.server
import json
import os
import re
from pathlib import Path
from urllib.parse import quote
import secrets
import subprocess
import threading
import time

from playwright.sync_api import expect, sync_playwright
from csp_support import CspStaticFiles, field_label

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("VAULTSYNCD_TEST_BIN_DIR", REPO / "target/debug"))
RUN = Path("/data") / f"vs-s134-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


def checked(args, **kwargs):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, **kwargs)
    assert result.returncode == 0, f"{args[0]} failed: {result.stderr}"
    return result.stdout


def main():
    checked(["sudo", "-n", "mkdir", "-m", "755", RUN])
    checked(["sudo", "-n", "chown", f"{os.getuid()}:{os.getgid()}", RUN])
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    print(f"S134 artifacts: {RUN}", flush=True)
    state, roots = RUN / "state", RUN / "roots"
    state.mkdir(mode=0o700); roots.mkdir()
    documents, archive = roots / "documents", roots / "archive"
    documents.mkdir(); archive.mkdir()
    nested = documents / "文档" / "子目录"; nested.mkdir(parents=True)
    (documents / "空目录").mkdir()
    sample = nested / "report & '文档'.txt"; sample.write_bytes(b"S124 read-only browser\n")
    digest = hashlib.sha256(sample.read_bytes()).hexdigest()
    special = documents / "URL # % & 空 格" / "子 + %2F"
    special.mkdir(parents=True)
    (special / "deep.txt").write_bytes(b"URL names\n")
    long_directory = "目录-" + "a" * 120
    (documents / long_directory).mkdir()
    (documents / long_directory / "leaf.txt").write_bytes(b"long directory\n")
    (archive / "archive-only.txt").write_bytes(b"second root\n")
    for index in range(1005): (documents / f"file-{index:04}.txt").write_bytes(b"x")
    long_name = "long-" + "a" * 160 + ".txt"
    (documents / long_name).write_bytes(b"")
    password = secrets.token_urlsafe(24)
    checked([BIN / "vaultsyncd-admin", "init", "--state-dir", state], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten="127.0.0.1:0"\n[state]\ndir="{state}"\n[roots]\nallowed_bases=["{roots}"]\n[log]\nformat="json"\nlevel="info"\n')
    log = (RUN / "server.log").open("w")
    service = subprocess.Popen([str(BIN / "vaultsyncd-server"), "--config", str(config)], stdout=log, stderr=log)
    proxy = None
    faults = {"mode": None, "root": None}
    delayed, release = threading.Event(), threading.Event()
    requests = []
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

            def do_GET(self):
                if self.path.startswith("/v1/"): self.forward()
                else: super().do_GET()

            def forward(self):
                backend = http.client.HTTPConnection(address, timeout=30)
                path = self.path
                is_entries = "/snapshots/" in path and "/entries" in path
                should_delay = is_entries and faults["mode"] == "delay" and faults["root"] in path
                if is_entries and faults["mode"] == "expired":
                    parts = path.split("/"); parts[5] = "0" * 32; path = "/".join(parts)
                headers = {key: value for key, value in self.headers.items() if key.lower() not in ("connection", "transfer-encoding")}
                body = self.rfile.read(int(self.headers.get("content-length", "0")))
                try:
                    backend.request(self.command, path, body=body, headers=headers)
                    response = backend.getresponse(); payload = response.read()
                    requests.append({"path": self.path, "method": self.command, "status": response.status})
                    if should_delay:
                        delayed.set(); release.wait(timeout=30)
                    self.send_response(response.status)
                    for key, value in response.getheaders():
                        if key.lower() not in ("connection", "transfer-encoding", "content-length"): self.send_header(key, value)
                    self.send_header("content-length", str(len(payload))); self.end_headers()
                    self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # Browser cancellation is the intended network fault in the race case.
                finally: backend.close()

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=str(REPO / "apps/web/dist")))
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        origin = f"http://localhost:{proxy.server_port}"
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            context = browser.new_context(viewport={"width": 1440, "height": 1000})
            context.grant_permissions(["clipboard-read", "clipboard-write"], origin=origin)
            page = context.new_page(); errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.add_init_script("window.__csp=[];document.addEventListener('securitypolicyviolation',e=>window.__csp.push(e.effectiveDirective));")
            page.goto(origin + "/#/files")
            expect(page.get_by_role("heading", name="请先登录", exact=True)).to_be_visible()
            page.goto(origin + "/#/account")
            page.get_by_label(field_label("登录名")).fill("owner")
            page.get_by_label(field_label("密码")).fill(password)
            page.get_by_role("button", name="登录", exact=True).click()
            expect(page.get_by_text("已登录：owner", exact=True)).to_be_visible()
            page.goto(origin + "/#/files")
            expect(page.get_by_text("尚无授权 Root，请先在 Root 目录页面注册目录。", exact=True)).to_be_visible()

            def api(path, body=None):
                result = page.evaluate("""async ([path, body]) => {const r=await fetch(path,body===null?{}:{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});return {status:r.status,body:await r.json()}}""", [path, body])
                assert result["status"] in (200, 201), result
                return result["body"]

            doc_id = api("/v1/admin/roots", {"path": str(documents)})["root_id"]
            arc_id = api("/v1/admin/roots", {"path": str(archive)})["root_id"]
            for attempt in range(180):
                managed = api("/v1/admin/roots")["roots"]
                if len(managed) == 2 and all(item["collection"]["state"] == "complete" for item in managed): break
                if attempt % 20 == 0: print(f"waiting for real observer: {attempt}", flush=True)
                page.wait_for_timeout(500)
            else: raise AssertionError("observer did not complete initial import")
            # Existing observer imports regular files and their parents, not external
            # empty directories. Seed a committed empty directory through the public
            # operation API, then audit only requests made by the browser feature.
            (documents / "空目录").rmdir()
            seed = api(f"/v1/roots/{doc_id}/snapshots", {})
            cursor, root_entry = None, None
            while True:
                page_data = api(f"/v1/roots/{doc_id}/snapshots/{seed['snapshot_id']}/entries?limit=1000" + (f"&cursor={cursor}" if cursor else ""))
                root_entry = next((entry["entry_id"] for entry in page_data["entries"] if "parent_id" not in entry), root_entry)
                cursor = page_data.get("next_cursor")
                if cursor is None: break
            assert root_entry
            made = api(f"/v1/roots/{doc_id}/operations", {"kind": "create", "op_id": secrets.token_hex(16), "device_id": secrets.token_hex(16), "server_epoch": seed["server_epoch"], "parent_id": root_entry, "name": "空目录", "expected_absent": True})
            assert made["state"] == "committed", made
            browsing_start = len(requests)
            page.reload()

            def choose(name, root_id):
                page.get_by_role("combobox", name="浏览 Root", exact=True).click()
                page.get_by_role("option", name=f"{name} · {root_id[:8]}", exact=True).click()

            choose("documents", doc_id)
            table = page.get_by_role("table", name="文件与目录", exact=True)
            expect(table).to_be_visible(timeout=15000)
            assert len(table.locator("tbody tr").all()) == 100
            # S130：固定列宽（名称占 62%）、大小人类可读、修改日期本地化、链接悬停无下划线。
            assert [cell.inner_text() for cell in table.locator("thead th").all()] == ["名称", "大小", "修改日期"]
            layout = table.evaluate("""e => {
                const heads = [...e.querySelectorAll('thead th')].map(th => th.getBoundingClientRect().width);
                const cells = [...e.querySelectorAll('tbody tr:first-child td')].map(td => td.getBoundingClientRect().width);
                return {heads, cells, tableLayout: getComputedStyle(e).tableLayout};
            }""")
            assert layout["tableLayout"] == "fixed", layout
            shares = (.62, .14, .24)
            for widths in (layout["heads"], layout["cells"]):
                total = sum(widths)
                assert len(widths) == len(shares), widths
                for width, share in zip(widths, shares):
                    assert abs(width / total - share) < .02, (widths, share)
            file_link = table.get_by_role("button", name="file-0000.txt", exact=True).first
            assert file_link.evaluate("e => getComputedStyle(e).textDecorationLine") == "none"
            file_link.hover()
            page.wait_for_timeout(100)
            assert file_link.evaluate("e => getComputedStyle(e).textDecorationLine") == "none", "悬停不得出现下划线"
            file_cells = file_link.locator("xpath=ancestor::tr[1]").locator("td").all()
            assert len(file_cells) == 3, [cell.inner_text() for cell in file_cells]
            assert re.fullmatch(r"\d+(?:[.,]\d+)? (?:B|KB|MB|GB|TB|PB)", file_cells[1].inner_text()), file_cells[1].inner_text()
            assert re.fullmatch(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}", file_cells[2].inner_text()), file_cells[2].inner_text()
            folder_cells = page.get_by_role("button", name="空目录", exact=True).first.locator("xpath=ancestor::tr[1]").locator("td").all()
            assert folder_cells[1].inner_text() == "—", folder_cells[1].inner_text()
            # S132：最后一列“操作”已取消，改为 Mantine Menu 右键上下文菜单（含键盘 Shift+F10 与触屏长按）。
            assert table.locator("thead th").all_inner_texts() == ["名称", "大小", "修改日期"]
            expect(page.get_by_role("button", name=re.compile(r"^查看 .* 的属性$"))).to_have_count(0)
            file_row = file_link.locator("xpath=ancestor::tr[1]")
            assert file_row.evaluate("e => getComputedStyle(e).cursor") == "context-menu"
            box = file_row.bounding_box()
            file_row.click(button="right", position={"x": 40, "y": box["height"] / 2})
            menu = page.get_by_role("menu")
            expect(menu).to_be_visible()
            assert menu.get_by_role("menuitem").all_inner_texts() == ["属性", "复制路径", "复制", "剪切", "粘贴", "重命名", "删除"], menu.get_by_role("menuitem").all_inner_texts()
            menu_box = menu.bounding_box()
            assert abs(menu_box["x"] - (box["x"] + 40)) < 12, (menu_box, box)
            assert abs(menu_box["y"] - (box["y"] + box["height"] / 2)) < 16, (menu_box, box)
            page.keyboard.press("Escape")
            expect(menu).not_to_be_visible()
            # 同一时刻只允许一个行菜单：在另一行右键时前一个关闭。
            folder_row = page.get_by_role("button", name="空目录", exact=True).first.locator("xpath=ancestor::tr[1]")
            file_row.click(button="right")
            expect(page.get_by_role("menu")).to_have_count(1)
            folder_row.click(button="right")
            expect(page.get_by_role("menu")).to_have_count(1)
            assert menu.get_by_role("menuitem").all_inner_texts() == ["打开", "属性", "复制路径", "复制", "剪切", "粘贴", "重命名", "删除"], menu.get_by_role("menuitem").all_inner_texts()
            page.wait_for_timeout(150)
            page.screenshot(path=str(RUN / "context-menu-light-1440.png"))
            page.emulate_media(color_scheme="dark")
            page.wait_for_timeout(150)
            page.screenshot(path=str(RUN / "context-menu-dark-1440.png"))
            page.emulate_media(color_scheme="light")
            page.get_by_role("menuitem", name="属性", exact=True).click()
            dialog = page.get_by_role("dialog", name="文件属性", exact=True)
            expect(dialog).to_contain_text("空目录")
            expect(dialog).to_contain_text("直接子项")
            page.get_by_role("button", name="关闭", exact=True).click()
            expect(dialog).not_to_be_visible()
            expect(page.get_by_role("menu")).to_have_count(0)
            # 键盘：名称按钮上的 Shift+F10 与菜单键打开同一菜单，位置贴近该行。
            file_link.focus()
            page.keyboard.press("Shift+F10")
            expect(menu).to_be_visible()
            assert menu.get_by_role("menuitem").all_inner_texts() == ["属性", "复制路径", "复制", "剪切", "粘贴", "重命名", "删除"]
            menu_box, link_box = menu.bounding_box(), file_link.bounding_box()
            assert abs(menu_box["y"] - (link_box["y"] + link_box["height"] / 2)) < 16, (menu_box, link_box)
            notifications = page.locator(".mantine-Notification-root")

            def notification(message):
                return notifications.filter(has=page.get_by_text(message, exact=True))

            def copy_row(name):
                table.get_by_role("button", name=name, exact=True).locator("xpath=ancestor::tr[1]").click(button="right")
                page.get_by_role("menuitem", name="复制路径", exact=True).click()
                expect(menu).not_to_be_visible()

            # S134：真实 Clipboard API 成功；全局浮层不改变工具栏、表格与分页几何。
            layout_targets = [page.locator(selector) for selector in (".files-browser", ".files-sidebar", ".files-toolbar", ".files-list", ".files-table", ".files-pagination")]
            before_notification = [target.bounding_box() for target in layout_targets]
            copied_at = time.monotonic()
            page.get_by_role("menuitem", name="复制路径", exact=True).click()
            expect(menu).not_to_be_visible()
            notice = notification("已复制路径 /file-0000.txt")
            expect(notice).to_have_count(1)
            expect(notice).to_have_attribute("role", "status")
            assert page.evaluate("navigator.clipboard.readText()") == "/file-0000.txt"
            expect(page.locator('.mantine-Notifications-root[data-position="top-right"]')).to_be_visible()
            page.wait_for_timeout(250)
            assert [target.bounding_box() for target in layout_targets] == before_notification, "通知挤压了文件布局"
            # 相同文案再次复制创建新的完整四秒通知，不复用已经消耗的计时器。
            page.wait_for_timeout(2400)
            copy_row("file-0000.txt")
            expect(notice).to_have_count(2)
            expect(notice).to_have_count(1, timeout=2500)
            assert time.monotonic() - copied_at >= 3.8, "首条成功通知过早消失"
            page.wait_for_timeout(700)
            expect(notice).to_have_count(1)
            expect(notice).to_have_count(0, timeout=5000)
            assert time.monotonic() - copied_at < 10, "成功通知未按四秒自动清理"
            # limit=3：第四条进入队列，关闭一条后补位；成功通知也能手动关闭。
            for index in range(4): copy_row(f"file-{index:04}.txt")
            expect(notifications).to_have_count(3)
            queued = notification("已复制路径 /file-0003.txt")
            expect(queued).to_have_count(0)
            notification("已复制路径 /file-0000.txt").get_by_role("button", name="关闭通知", exact=True).click()
            expect(queued).to_be_visible()
            expect(notifications).to_have_count(3)
            for index in range(1, 4):
                notice_to_close = notification(f"已复制路径 /file-{index:04}.txt")
                notice_to_close.get_by_role("button", name="关闭通知", exact=True).click()
                expect(notice_to_close).to_have_count(0)
            # 拒绝 Chromium 原生剪贴板权限，不替换 navigator.clipboard 或应用 API。
            context.clear_permissions()
            cdp = context.new_cdp_session(page)
            browser_context_id = cdp.send("Target.getTargetInfo")["targetInfo"]["browserContextId"]
            for unsanitized in (False, True):
                cdp.send("Browser.setPermission", {"permission": {"name": "clipboard-write", "allowWithoutSanitization": unsanitized}, "setting": "denied", "origin": origin, "browserContextId": browser_context_id})
            clipboard_permission = page.evaluate("navigator.permissions.query({name:'clipboard-write'}).then(p=>p.state)")
            assert clipboard_permission == "denied", clipboard_permission
            copy_row("file-0000.txt")
            denied = notification("复制失败，请手动复制路径 /file-0000.txt")
            expect(denied).to_have_attribute("role", "alert")
            page.wait_for_timeout(4500)
            expect(denied).to_be_visible()
            page.screenshot(path=str(RUN / "notification-clipboard-denied.png"))
            denied.get_by_role("button", name="关闭通知", exact=True).focus()
            page.keyboard.press("Enter")
            expect(denied).to_have_count(0)
            cdp.send("Browser.resetPermissions", {"browserContextId": browser_context_id})
            context.grant_permissions(["clipboard-read", "clipboard-write"], origin=origin)
            restored_permission = page.evaluate("navigator.permissions.query({name:'clipboard-write'}).then(p=>p.state)")
            assert restored_permission == "granted", restored_permission
            cdp.detach()
            expect(notifications).to_have_count(0)
            expect(page.get_by_role("heading", name="文件浏览器", exact=True)).not_to_be_visible()
            expect(page.get_by_text("切换 Root、浏览目录并查看文件属性。", exact=True)).not_to_be_visible()
            expect(page.get_by_role("button", name="刷新 Root 列表", exact=True)).not_to_be_visible()
            expect(page.get_by_role("tree", name="目录树", exact=True)).to_be_visible()
            tree = page.get_by_role("tree", name="目录树", exact=True)
            expect(tree).to_have_attribute("data-with-lines", "true")
            docs_node = tree.get_by_text("文档", exact=True).locator("xpath=ancestor::li[1]")
            empty_node = tree.get_by_text("空目录", exact=True).locator("xpath=ancestor::li[1]")
            expect(empty_node).not_to_have_attribute("aria-expanded")
            expect(empty_node.get_by_role("button")).to_have_count(0)
            expect(tree.get_by_text("file-0000.txt", exact=True)).to_have_count(0)
            current_url, current_requests = page.url, len(requests)
            docs_node.get_by_role("button", name="展开 文档", exact=True).click()
            expect(docs_node).to_have_attribute("aria-expanded", "true")
            expect(tree.get_by_text("子目录", exact=True)).to_be_visible()
            open_folder_path = docs_node.get_by_role("button", name="收起 文档", exact=True).locator("path").get_attribute("d")
            docs_node.get_by_role("button", name="收起 文档", exact=True).focus()
            page.keyboard.press("Enter")
            expect(docs_node).to_have_attribute("aria-expanded", "false")
            expect(tree.get_by_text("子目录", exact=True)).not_to_be_visible()
            assert docs_node.get_by_role("button", name="展开 文档", exact=True).locator("path").get_attribute("d") != open_folder_path
            assert page.url == current_url and len(requests) == current_requests
            docs_node.focus(); page.keyboard.press("ArrowRight")
            expect(docs_node).to_have_attribute("aria-expanded", "true")
            page.keyboard.press("ArrowRight")
            child_node = tree.get_by_text("子目录", exact=True).locator("xpath=ancestor::li[1]")
            expect(child_node).to_be_focused()
            page.keyboard.press("ArrowLeft"); expect(docs_node).to_be_focused()
            page.keyboard.press("Enter")
            expect(page.get_by_role("textbox", name="目录地址", exact=True)).to_have_value("/文档")
            expect(docs_node).to_have_attribute("aria-selected", "true")
            docs_node.get_by_role("button", name="收起 文档", exact=True).click()
            expect(docs_node).to_have_attribute("aria-expanded", "false")
            expect(table.get_by_role("button", name="子目录", exact=True)).to_be_visible()
            docs_node.focus(); page.keyboard.press("Space")
            expect(docs_node).to_have_attribute("aria-expanded", "true")
            expect(page.get_by_role("textbox", name="目录地址", exact=True)).to_have_value("/文档")
            # History reveals the selected path again; folder toggles never add history.
            page.get_by_role("button", name="后退", exact=True).click()
            expect(page.get_by_role("textbox", name="目录地址", exact=True)).to_have_value("/")
            page.get_by_role("button", name="前进", exact=True).click()
            expect(docs_node).to_have_attribute("aria-selected", "true")
            expect(docs_node).to_have_attribute("aria-expanded", "true")
            page.get_by_role("button", name="返回上级", exact=True).click()
            tree_layout_cases = []
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 390, 768, 1440, 1920):
                    page.set_viewport_size({"width": width, "height": 900})
                    page.wait_for_timeout(250)
                    selected_label = tree.locator('.mantine-Tree-label[data-selected]')
                    expect(selected_label).to_have_count(1)
                    selected_bg = selected_label.evaluate("e => getComputedStyle(e).backgroundColor")
                    other_bg = docs_node.locator('.mantine-Tree-label').first.evaluate("e => getComputedStyle(e).backgroundColor")
                    assert selected_bg != other_bg, (scheme, width, "selection is not visible")
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width, "tree overflow")
                    assert tree.evaluate("e => e.scrollWidth > e.parentElement.clientWidth"), "long names must scroll within the sidebar"
                    for node in (docs_node, empty_node):
                        label = node.locator('.mantine-Tree-label').first
                        expect(label.locator("svg")).to_have_count(1)
                        geometry = node.evaluate("""e => {
                            const label = e.querySelector('.mantine-Tree-label');
                            const icon = label.querySelector('svg').getBoundingClientRect();
                            const text = label.lastElementChild.getBoundingClientRect();
                            const line = getComputedStyle(e, '::before');
                            const lineEnd = e.getBoundingClientRect().x + parseFloat(line.left) + parseFloat(line.width);
                            return { gap: icon.x - lineEnd, size: icon.width, textGap: text.x - icon.right };
                        }""")
                        assert abs(geometry["gap"] - 4) < 1, (scheme, width, geometry)
                        assert geometry["size"] == 14 and geometry["textGap"] == 6, (scheme, width, geometry)
                    page.screenshot(path=str(RUN / f"tree-{scheme}-{width}.png"), full_page=True)
                    tree_layout_cases.append([scheme, width])
            page.emulate_media(color_scheme="light")
            page.set_viewport_size({"width": 1440, "height": 1000})
            page.wait_for_timeout(250)
            panel = page.locator(".files-browser").bounding_box()
            sidebar = page.locator(".files-sidebar").bounding_box()
            toolbar = page.get_by_role("toolbar", name="文件浏览器工具栏").bounding_box()
            listing = page.locator(".files-list").bounding_box()
            assert abs(panel["x"] - sidebar["x"]) < 2 and toolbar["x"] >= sidebar["x"] + sidebar["width"] - 1
            assert listing["y"] >= toolbar["y"] + toolbar["height"] - 1
            assert abs(panel["y"] - 64) < 2 and abs(panel["y"] + panel["height"] - 984) < 2
            # Snapshot has >1000 entries; at least one real API continuation must have been read.
            assert any("cursor=" in request["path"] for request in requests)
            page.get_by_role("button", name="2", exact=True).click()
            assert len(table.locator("tbody tr").all()) == 100
            page.get_by_label("筛选当前目录", exact=True).fill("file-1004")
            expect(table.get_by_role("button", name="file-1004.txt", exact=True)).to_be_visible()
            assert len(table.locator("tbody tr").all()) == 1
            page.get_by_label("筛选当前目录", exact=True).fill("not-found")
            expect(page.get_by_text("当前目录没有匹配的名称。", exact=True)).to_be_visible()
            page.get_by_label("筛选当前目录", exact=True).fill("")
            table.get_by_role("button", name="空目录", exact=True).click()
            expect(page.get_by_text("此目录在已提交快照中为空。", exact=True)).to_be_visible()
            page.get_by_role("button", name="返回上级", exact=True).click()
            table.get_by_role("button", name="文档", exact=True).click()
            table.get_by_role("button", name="子目录", exact=True).click()
            table.get_by_role("button", name=sample.name, exact=True).click()
            dialog = page.get_by_role("dialog", name="文件属性", exact=True)
            expect(dialog).to_contain_text(digest)
            expect(dialog).to_contain_text(f"{sample.stat().st_size} 字节")
            expect(dialog).to_contain_text("/文档/子目录/" + sample.name)
            page.keyboard.press("Escape"); expect(dialog).not_to_be_visible()
            expect(table.get_by_role("button", name=sample.name, exact=True)).to_be_focused()
            assert page.url.endswith(f"#/files/{doc_id}/{quote('文档', safe='')}/{quote('子目录', safe='')}")
            expect(page.get_by_role("textbox", name="目录地址", exact=True)).to_have_value("/文档/子目录")
            page.reload()
            expect(table.get_by_role("button", name=sample.name, exact=True)).to_be_visible()
            expect(page.locator('[role="treeitem"][aria-selected="true"]')).to_have_count(1)
            snapshot_count = sum(request["method"] == "POST" and request["path"].endswith("/snapshots") for request in requests)
            page.get_by_role("button", name="返回上级", exact=True).click()
            expect(page.get_by_role("textbox", name="目录地址", exact=True)).to_have_value("/文档")
            page.get_by_role("button", name="后退", exact=True).click()
            expect(table.get_by_role("button", name=sample.name, exact=True)).to_be_visible()
            page.get_by_role("button", name="前进", exact=True).click()
            expect(table.get_by_role("button", name="子目录", exact=True)).to_be_visible()
            page.go_back()
            expect(table.get_by_role("button", name=sample.name, exact=True)).to_be_visible()
            page.go_forward()
            expect(table.get_by_role("button", name="子目录", exact=True)).to_be_visible()
            assert sum(request["method"] == "POST" and request["path"].endswith("/snapshots") for request in requests) == snapshot_count
            tree = page.get_by_role("tree", name="目录树", exact=True)
            tree.get_by_text("子目录", exact=True).click()
            expect(table.get_by_role("button", name=sample.name, exact=True)).to_be_visible()
            selected_tree = page.locator('[role="treeitem"][aria-selected="true"]')
            selected_tree.focus()
            page.keyboard.press("ArrowLeft")
            page.keyboard.press("Enter")
            expect(page.get_by_role("textbox", name="目录地址", exact=True)).to_have_value("/文档")
            expect(table.get_by_role("button", name="子目录", exact=True)).to_be_visible()
            page.get_by_role("button", name="当前目录属性", exact=True).click()
            expect(dialog).to_contain_text("直接子项")
            expect(dialog).to_contain_text("/文档")
            page.get_by_role("button", name="关闭", exact=True).click()
            page.get_by_role("button", name="返回上级", exact=True).click()
            address_input = page.get_by_role("textbox", name="目录地址", exact=True)
            address_input.fill("/URL # % & 空 格/子 + %2F")
            address_input.press("Enter")
            expect(table.get_by_role("button", name="deep.txt", exact=True)).to_be_visible()
            deep_url = page.url
            assert deep_url.endswith(f"#/files/{doc_id}/" + "/".join(quote(part, safe="") for part in special.relative_to(documents).parts))
            page.reload()
            expect(table.get_by_role("button", name="deep.txt", exact=True)).to_be_visible()
            address_input.fill("../")
            address_input.press("Enter")
            address_error = notifications.filter(has_text="请输入 Root 内的相对路径")
            expect(address_error).to_have_attribute("role", "alert")
            page.wait_for_timeout(4500)
            expect(address_error).to_be_visible()
            address_input.press("Escape")
            expect(address_input).to_have_value("/URL # % & 空 格/子 + %2F")
            expect(address_error).to_have_count(0)
            address_input.fill("/missing")
            page.get_by_role("button", name="转到目录", exact=True).click()
            missing_error = notifications.filter(has_text="目录不存在")
            expect(missing_error).to_have_attribute("role", "alert")
            expect(table).not_to_be_visible()
            page.get_by_role("button", name="返回上级", exact=True).click()
            expect(table).to_be_visible()
            expect(missing_error).to_have_count(0)
            expect(page.get_by_role("button", name="前进", exact=True)).to_be_disabled()
            for bad, message in [(f"{doc_id}/%2F", "目录 URL 无效"), (f"{doc_id}/%zz", "目录 URL 无效"), ("0" * 32, "Root 不存在或未获授权")]:
                page.goto(origin + "/#/files/" + bad)
                expect(notifications.filter(has_text=message)).to_have_attribute("role", "alert")
                expect(table).not_to_be_visible()
            # 离开错误页面会卸载 scoped Feedback，不让旧错误泄漏到其他页面。
            page.goto(origin + "/#/account")
            expect(page.get_by_text("已登录：owner", exact=True)).to_be_visible()
            expect(notifications).to_have_count(0)
            page.goto(deep_url)
            expect(table.get_by_role("button", name="deep.txt", exact=True)).to_be_visible()
            address_input.fill("/")
            address_input.press("Enter")
            expect(table).to_be_visible()
            page.get_by_label("筛选当前目录", exact=True).fill("long-")
            table.get_by_role("button", name=long_name, exact=True).click()
            expect(dialog).to_contain_text("0 字节")
            layout_cases = []
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 390, 768, 1440, 1920):
                    page.set_viewport_size({"width": width, "height": 900})
                    page.wait_for_timeout(250)  # AppShell's breakpoint padding transition is 200 ms.
                    page.screenshot(path=str(RUN / f"properties-{scheme}-{width}.png"), full_page=True)
                    if not page.evaluate("document.documentElement.scrollWidth <= innerWidth"):
                        bounds = page.evaluate("[...document.querySelectorAll('body *')].map(e=>({tag:e.tagName,class:e.className,left:e.getBoundingClientRect().left,right:e.getBoundingClientRect().right,width:e.getBoundingClientRect().width})).filter(e=>e.width>innerWidth || e.right>innerWidth+1)")
                        (RUN / "overflow.json").write_text(json.dumps(bounds, ensure_ascii=False, indent=2))
                        raise AssertionError((scheme, width, "dialog", bounds[:15]))
                    layout_cases.append([scheme, width])
            page.get_by_role("button", name="关闭", exact=True).click()
            notification_layout_cases = []
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 390):
                    page.set_viewport_size({"width": width, "height": 900})
                    page.wait_for_timeout(250)
                    before_copy = page.locator(".files-list").bounding_box()
                    copy_row(long_name)
                    long_notice = notification("已复制路径 /" + long_name)
                    expect(long_notice).to_be_visible()
                    page.wait_for_timeout(250)
                    assert page.locator(".files-list").bounding_box() == before_copy, (scheme, width, "notification shifted list")
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width, "notification overflow")
                    box = long_notice.bounding_box()
                    assert box["x"] >= 0 and box["x"] + box["width"] <= width + 1, (scheme, width, box)
                    description = long_notice.locator(".mantine-Notification-description")
                    assert description.evaluate("e=>e.scrollWidth <= e.clientWidth"), (scheme, width, "long path overflow")
                    assert page.locator("html").get_attribute("data-mantine-color-scheme") == scheme
                    page.screenshot(path=str(RUN / f"notification-{scheme}-{width}.png"))
                    long_notice.get_by_role("button", name="关闭通知", exact=True).click()
                    expect(long_notice).to_have_count(0)
                    notification_layout_cases.append([scheme, width])
            page.emulate_media(color_scheme="light")
            page.set_viewport_size({"width": 1440, "height": 1000})
            page.get_by_label("筛选当前目录", exact=True).fill("")
            choose("archive", arc_id)
            expect(table.get_by_role("button", name="archive-only.txt", exact=True)).to_be_visible()
            expect(tree.get_by_role("treeitem")).to_have_count(1)
            expect(tree.get_by_role("button")).to_have_count(0)
            expect(tree.locator('[aria-selected="true"]')).to_have_count(1)
            expect(table.get_by_role("button", name="文档", exact=True)).not_to_be_visible()
            expect(page.get_by_role("button", name="返回上级", exact=True)).to_be_disabled()
            page.get_by_role("button", name="后退", exact=True).click()
            expect(table.get_by_role("button", name="文档", exact=True)).to_be_visible()
            page.get_by_role("button", name="前进", exact=True).click()
            expect(table.get_by_role("button", name="archive-only.txt", exact=True)).to_be_visible()
            choose("documents", doc_id)
            expect(table.get_by_role("button", name="文档", exact=True)).to_be_visible()
            faults.update(mode="delay", root=doc_id)
            page.get_by_role("button", name="刷新文件列表", exact=True).click()
            for _ in range(100):
                if delayed.is_set(): break
                page.wait_for_timeout(50)
            assert delayed.is_set()
            choose("archive", arc_id)
            expect(table.get_by_role("button", name="archive-only.txt", exact=True)).to_be_visible()
            expect(tree.get_by_role("treeitem")).to_have_count(1)
            expect(tree.get_by_role("button")).to_have_count(0)
            expect(tree.locator('[aria-selected="true"]')).to_have_count(1)
            faults["mode"] = None; release.set()
            page.wait_for_timeout(500)
            expect(table.get_by_role("button", name="archive-only.txt", exact=True)).to_be_visible()
            expect(table.get_by_role("button", name="文档", exact=True)).not_to_be_visible()
            context.set_offline(True)
            page.get_by_role("button", name="刷新文件列表", exact=True).click()
            expect(page.get_by_role("alert")).to_contain_text("未能读取完整目录")
            expect(table).not_to_be_visible()
            expect(page.get_by_text("此目录在已提交快照中为空。", exact=True)).not_to_be_visible()
            context.set_offline(False)
            faults["mode"] = "expired"
            with page.expect_response(lambda response: response.status == 410 and "/snapshots/" in response.url):
                page.get_by_role("button", name="刷新文件列表", exact=True).click()
            expect(page.get_by_role("alert")).to_contain_text("未能读取完整目录")
            assert any(request["status"] == 410 for request in requests)
            faults["mode"] = None
            page.get_by_role("button", name="刷新文件列表", exact=True).click()
            expect(table.get_by_role("button", name="archive-only.txt", exact=True)).to_be_visible()
            (archive / "new.txt").write_bytes(b"new observation\n")
            for _ in range(60):
                page.get_by_role("button", name="刷新文件列表", exact=True).click()
                expect(table).to_be_visible()
                if table.get_by_role("button", name="new.txt", exact=True).is_visible(): break
                page.wait_for_timeout(500)
            else: raise AssertionError("refresh did not see new committed file")
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 390, 768, 1440, 1920):
                    page.set_viewport_size({"width": width, "height": 900})
                    page.wait_for_timeout(250)
                    page.get_by_role("combobox", name="浏览 Root", exact=True).click()
                    expect(page.get_by_role("option", name=f"documents · {doc_id[:8]}", exact=True)).to_be_visible()
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width, "select")
                    page.keyboard.press("Escape")
                    page.screenshot(path=str(RUN / f"browser-{scheme}-{width}.png"), full_page=True)
            assert not page.evaluate("window.__csp"), page.evaluate("window.__csp")
            assert not errors, errors
            assert not any("/content" in request["path"] or "/operations" in request["path"] for request in requests[browsing_start:])
            assert hashlib.sha256(sample.read_bytes()).hexdigest() == digest
            # Revocation is performed via the real UI; the browser route must discard its data.
            page.goto(origin + "/#/account")
            page.get_by_role("button", name="退出当前会话", exact=True).click()
            expect(page.get_by_text("未登录", exact=True)).to_be_visible()
            page.goto(origin + "/#/files")
            expect(page.get_by_role("heading", name="请先登录", exact=True)).to_be_visible()
            expect(table).not_to_be_visible()
            context.close(); browser.close()
            (RUN / "result.json").write_text(json.dumps({"passed": True, "checks": ["auth/empty-roots", "real-observer", "snapshot-multipage", "directory-pagination/filter", "nested/empty/parent/tree/keyboard", "mantine-lines/selection/folder-toggle-no-navigation/compact-spacing", "tree-empty-root/long-names/themes/responsive", "no-action-column", "row-context-menu/position/single-open/clipboard/keyboard", "notifications/top-right/limit-3/queue", "notifications/success-4s/repeated-copy-new-timer/manual-close", "notifications/native-clipboard-denied/sticky-error/keyboard-close", "notifications/no-layout-shift/long-path/narrow-light-dark", "notifications/scoped-error-state-and-unmount-cleanup", "URL-deep-link/reload/special-names", "address/missing/invalid/unauthorized", "toolbar/native-history/branch/root-history", "split-layout/no-redundant-copy", "properties/hash/size/path", "root-switch", "late-response", "offline-is-not-empty", "real-410", "refresh-observed-change", "light/dark-responsive-select/modal", "CSP", "no-content-or-operation", "source-hash", "logout"], "layout_cases": layout_cases, "tree_layout_cases": tree_layout_cases, "notification_layout_cases": notification_layout_cases, "requests": requests}, ensure_ascii=False, indent=2))
        print(f"S134 browser acceptance passed: {RUN}", flush=True)
    finally:
        release.set()
        if proxy: proxy.shutdown(); proxy.server_close()
        if service.poll() is None:
            service.terminate()
            try: service.wait(timeout=15)
            except subprocess.TimeoutExpired: service.kill(); service.wait()
        log.close()


if __name__ == "__main__": main()
