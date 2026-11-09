#!/usr/bin/env python3
"""S119: built Web, real Chromium, disconnected static origin; no API fixtures.

Run: nix develop .#web-tests --command python3 tests/web/admin-ui-browser.py
Business workflows against an isolated real service remain in the four existing
*-browser.py suites. Screenshots/results are stored outside Git.
"""
import functools
import http.server
import json
import os
from pathlib import Path
import time
import threading

from playwright.sync_api import expect, sync_playwright
from csp_support import CspStaticFiles, field_label

REPO = Path(__file__).resolve().parents[2]


def main():
    output = Path(os.environ.get("VAULTSYNCD_UI_ARTIFACTS", REPO / "artifacts" / "S119")) / f"ui-{time.time_ns()}"
    output.mkdir(parents=True)
    print(f"S119 UI artifacts: {output}", flush=True)

    class Handler(CspStaticFiles):
        def log_message(self, *_):
            pass


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
            page.add_init_script("""window.__cspViolations=[]; document.addEventListener('securitypolicyviolation', event => window.__cspViolations.push({directive:event.effectiveDirective, blocked:event.blockedURI}));""")
            response = page.goto(origin)
            nonce = page.locator('meta[name="csp-nonce"]').get_attribute("content")
            assert response and nonce and f"'nonce-{nonce}'" in response.headers["content-security-policy"]
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
            before = page.locator("html").get_attribute("data-mantine-color-scheme")
            theme.click()
            expect(page.locator("html")).not_to_have_attribute("data-mantine-color-scheme", before or "")
            page.screenshot(path=str(output / "overview-dark.png"), full_page=True)
            cases = []
            for scheme in ("light", "dark"):
                if page.locator("html").get_attribute("data-mantine-color-scheme") != scheme:
                    theme.click()
                for width in (320, 390, 768, 1440, 1920):
                    page.set_viewport_size({"width": width, "height": 900})
                    for label in ("服务概览", "Vault 目录", "历史与恢复", "备份管理", "账号与会话"):
                        if width < 768:
                            page.get_by_role("button", name="切换导航", exact=True).click()
                        page.get_by_role("link", name=label, exact=True).click()
                        expect(page.get_by_role("heading", name=label, level=1, exact=True)).to_be_visible()
                        if width < 768:
                            page.wait_for_function("() => document.querySelector('.mantine-AppShell-navbar').getBoundingClientRect().right <= 0")
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width, label)
                        assert page.evaluate("[...document.querySelectorAll('style')].every(style => style.nonce === document.querySelector('meta[name=csp-nonce]').content)"), (scheme, width, label, "missing-style-nonce")
                        cases.append({"theme":scheme, "width":width, "route":label})
                    expect(page.get_by_label(field_label("登录名"))).to_be_visible()
                    page.screenshot(path=str(output / f"account-{scheme}-{width}.png"), full_page=True)
            assert not page.evaluate("window.__cspViolations"), page.evaluate("window.__cspViolations")
            page.goto(origin + "/#/missing")
            expect(page.get_by_role("heading", name="页面不存在", level=1)).to_be_visible()
            assert not errors, errors
            browser.close()
        (output / "result.json").write_text(json.dumps({
            "passed": True,
            "checks": ["five-routes", "unknown-route", "skip-focus", "offline-health",
                       "theme", "320/390/768/1440/1920-no-overflow", "all-style-nonces", "production-csp", "no-pageerror"],
            "cases":cases,
        }, ensure_ascii=False, indent=2))
        print(f"S119 UI acceptance passed: {output}")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
