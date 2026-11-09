#!/usr/bin/env python3
"""S145: built Web, real Chromium, disconnected static origin; no API fixtures.

Run: nix develop .#web-tests --command python3 tests/web/admin-ui-browser.py
Authenticated business workflows against isolated real services remain in the
other *-browser.py suites. Screenshots/results are stored outside Git.
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
    output = Path(os.environ.get("VAULTSYNCD_UI_ARTIFACTS", Path("/data/vaultsyncd-temp/evidence/S145"))) / f"ui-{time.time_ns()}"
    output.mkdir(parents=True)
    print(f"S145 UI artifacts: {output}", flush=True)

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
            errors, violations, api_responses = [], [], []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.on("console", lambda message: errors.append(message.text)
                    if "Content Security Policy" in message.text else None)
            server_origin = f"http://localhost:{server.server_port}"
            page.on("response", lambda response: api_responses.append({
                "path": response.url.split(server_origin, 1)[-1], "status": response.status,
            }) if response.url.startswith(server_origin + "/v1/") else None)
            page.expose_function("recordCspViolation", lambda violation: violations.append(violation))
            page.add_init_script("""document.addEventListener('securitypolicyviolation', event => window.recordCspViolation({directive:event.effectiveDirective, blocked:event.blockedURI}));""")
            login_name = page.get_by_label(field_label("登录名"))
            password = page.get_by_label(field_label("密码"))
            login = page.get_by_role("button", name="登录", exact=True)

            def assert_gate(url):
                expect(page.get_by_role("heading", name="登录 vaultsyncd", level=1, exact=True)).to_be_visible()
                expect(login_name).to_be_enabled()
                expect(password).to_be_enabled()
                expect(login).to_be_enabled()
                expect(page.locator(".mantine-AppShell-root")).to_have_count(0)
                expect(page.get_by_role("navigation", name="主要导航")).to_have_count(0)
                expect(page.get_by_role("button", name="检查服务连接", exact=True)).to_have_count(0)
                expect(page.get_by_role("heading", name="页面不存在", exact=True)).to_have_count(0)
                assert page.url == url, (page.url, url, "login gate rewrote the requested hash")

            response = page.goto(server_origin)
            nonce = page.locator('meta[name="csp-nonce"]').get_attribute("content")
            assert response and nonce and f"'nonce-{nonce}'" in response.headers["content-security-policy"]
            assert_gate(server_origin + "/")
            # A static origin really cannot verify a session. Failure must expose
            # an actionable error, never the overview or other management UI.
            expect(page.locator(".mantine-Alert-root")).to_be_visible()
            retry = page.get_by_role("button", name="重新核验会话", exact=True)
            with page.expect_response(lambda response: response.url.endswith("/v1/auth/account")) as retried:
                retry.click()
            assert retried.value.status == 404
            expect(retry).to_be_enabled()
            expect(page.locator(".mantine-Alert-root")).to_be_visible()
            assert_gate(server_origin + "/")
            # Standalone login replaces the shell's skip-link flow. Credentials
            # must still be reachable in normal keyboard order.
            page.reload()
            assert_gate(server_origin + "/")
            page.keyboard.press("Tab")
            expect(login_name).to_be_focused()
            page.keyboard.press("Tab")
            expect(password).to_be_focused()
            login_name.fill("owner")
            password.fill("S145-offline-not-a-real-secret")
            with page.expect_response(lambda response: response.url.endswith("/v1/auth/browser/login")) as attempted:
                login.click()
            assert attempted.value.status >= 400
            expect(login).to_be_enabled()
            expect(page.locator(".mantine-Alert-root")).to_be_visible()
            expect(password).to_have_value("")
            assert_gate(server_origin + "/")

            cases = []
            routes = ("", "/overview", "/vaults", "/files",
                      "/files/00000000000000000000000000000000/%E7%9B%AE%E5%BD%95",
                      "/recovery", "/backup", "/account", "/missing")
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                for width in (320, 390, 768, 1440, 1920):
                    page.set_viewport_size({"width": width, "height": 900})
                    for route in routes:
                        url = server_origin + "/" + ("#" + route if route else "")
                        response = page.goto(url)
                        if response is None:  # Hash-only navigation does not fetch a document.
                            response = page.reload()
                        assert_gate(url)
                        expect(page.locator(".mantine-Alert-root")).to_be_visible()
                        expect(page.locator("html")).to_have_attribute("data-mantine-color-scheme", scheme)
                        nonce = page.locator('meta[name="csp-nonce"]').get_attribute("content")
                        assert response and nonce and f"'nonce-{nonce}'" in response.headers["content-security-policy"]
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (scheme, width, route)
                        assert page.evaluate("[...document.querySelectorAll('style')].every(style => style.nonce === document.querySelector('meta[name=csp-nonce]').content)"), (scheme, width, route, "missing-style-nonce")
                        cases.append({"theme": scheme, "width": width, "route": route or "/"})
                    page.screenshot(path=str(output / f"login-{scheme}-{width}.png"), full_page=True)
            assert api_responses and all(item["status"] >= 400 for item in api_responses), api_responses
            assert all(item["path"] in ("/v1/auth/account", "/v1/auth/browser/login") for item in api_responses), api_responses
            assert not violations, violations
            assert not errors, errors
            browser.close()
        (output / "result.json").write_text(json.dumps({
            "passed": True,
            "checks": ["all-management-hashes-gated", "deep-link-and-unknown-hash-preserved",
                       "keyboard-login", "failed-session-verification-and-real-retry",
                       "offline-login-fails-closed", "no-management-api-requests",
                       "light/dark-320/390/768/1440/1920-no-overflow",
                       "all-style-nonces", "production-csp", "no-pageerror"],
            "cases": cases,
            "api_responses": api_responses,
        }, ensure_ascii=False, indent=2))
        print(f"S145 UI acceptance passed: {output}")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
