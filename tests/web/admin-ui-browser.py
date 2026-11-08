#!/usr/bin/env python3
"""S115: built Web, real Chromium, disconnected static origin; no API fixtures.

Run: nix develop .#web-tests --command python3 tests/web/admin-ui-browser.py
Business workflows against an isolated real service remain in the four existing
*-browser.py suites. Screenshots/results are stored outside Git.
"""
import functools
import http.server
import json
import re
from pathlib import Path
import time
import threading

from playwright.sync_api import expect, sync_playwright

REPO = Path(__file__).resolve().parents[2]
POLICY = re.search(r'CONTENT_SECURITY_POLICY\s*,\s*HeaderValue::from_static\("([^"\n]+)"\)', (REPO / "apps/server/src/web.rs").read_text()).group(1)


def main():
    output = REPO / "artifacts" / "S115" / f"ui-{time.time_ns()}"
    output.mkdir(parents=True)
    print(f"S115 UI artifacts: {output}", flush=True)

    class Handler(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def end_headers(self):
            # Same policy as apps/server/src/web.rs; do not loosen production CSP.
            self.send_header("Content-Security-Policy", POLICY)
            super().end_headers()

    server = http.server.ThreadingHTTPServer(
        ("127.0.0.1", 0),
        functools.partial(Handler, directory=str(REPO / "apps/web/dist")),
    )
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            page = browser.new_page(viewport={"width": 1440, "height": 1000})
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.on("console", lambda message: errors.append(message.text)
                    if "Content Security Policy" in message.text else None)
            origin = f"http://localhost:{server.server_port}"
            page.goto(origin)
            expect(page.get_by_role("heading", name="服务概览", exact=True)).to_be_visible()
            page.keyboard.press("Tab")
            expect(page.get_by_role("link", name="跳到主要内容")).to_be_focused()
            page.keyboard.press("Enter")
            expect(page.locator("main")).to_be_focused()
            page.get_by_role("button", name="检查服务连接", exact=True).click()
            expect(page.get_by_role("button", name="检查服务连接", exact=True)).to_be_enabled()
            assert "未连接" in page.locator("body").inner_text()
            page.screenshot(path=str(output / "overview-light.png"), full_page=True)
            theme = page.get_by_role("button", name="切换主题", exact=True)
            before = page.locator("html").get_attribute("data-theme")
            theme.click()
            expect(page.locator("html")).not_to_have_attribute("data-theme", before or "")
            page.screenshot(path=str(output / "overview-dark.png"), full_page=True)
            for width in (390, 768, 1440):
                page.set_viewport_size({"width": width, "height": 900})
                for label, heading in (("服务概览", "服务概览"), ("Root 目录", "Root 目录"),
                                       ("历史与恢复", "历史与恢复"), ("备份管理", "备份管理"),
                                       ("账号与会话", "账号与会话")):
                    page.get_by_role("link", name=label, exact=True).click()
                    expect(page.get_by_role("heading", name=heading, level=1, exact=True)).to_be_visible()
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (width, label)
                expect(page.get_by_label("登录名", exact=True)).to_be_visible()
                page.screenshot(path=str(output / f"account-{width}.png"), full_page=True)
            page.goto(origin + "/#/missing")
            expect(page.get_by_role("heading", name="页面不存在", level=1)).to_be_visible()
            assert not errors, errors
            browser.close()
        (output / "result.json").write_text(json.dumps({
            "passed": True,
            "checks": ["five-routes", "unknown-route", "skip-focus", "offline-health",
                       "theme", "390/768/1440-no-overflow", "no-pageerror"],
        }, ensure_ascii=False, indent=2))
        print(f"S115 UI acceptance passed: {output}")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
