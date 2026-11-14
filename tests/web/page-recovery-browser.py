#!/usr/bin/env python3
"""S211: built page boundary in Chromium; controlled module/render faults only.

API fixtures isolate recovery from authentication/storage; auth-browser.py covers
the unchanged real backend/session lifecycle separately.
"""
import functools
import http.server
import json
import os
from pathlib import Path
import threading

from playwright.sync_api import expect, sync_playwright
from csp_support import CspStaticFiles

REPO = Path(__file__).resolve().parents[2]
OUTPUT = Path(os.environ["VAULTSYNCD_UI_ARTIFACTS"])


def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    class Handler(CspStaticFiles):
        def log_message(self, *_): pass
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=str(REPO / "apps/web/dist")))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    checks = []
    try:
        with sync_playwright() as pw:
            browser = pw.chromium.launch(headless=True)
            for scenario in ["render", "module", "persistent", "navigate", "offline", "hidden", "storage"]:
                print("checking " + scenario, flush=True)
                context = browser.new_context()
                page = context.new_page()
                navigations, attempts, violations = [], [], []
                page.on("framenavigated", lambda frame: navigations.append(frame.url) if frame == page.main_frame else None)
                page.add_init_script("window.__csp=[];document.addEventListener('securitypolicyviolation',e=>window.__csp.push(e.effectiveDirective))")
                if scenario == "storage":
                    page.add_init_script("Object.defineProperty(window, 'sessionStorage', {get(){throw new Error('blocked storage')}})")

                def api(route):
                    path = route.request.url.split("/v1/", 1)[1]
                    if path == "auth/account": body = {"user_id": "ab" * 16, "login": "owner", "session_id": "cd" * 16}
                    elif path == "health": body = {"status": "ok"}
                    else: body = {}
                    route.fulfill(json=body)
                page.route("**/v1/**", api)

                def module(route):
                    attempts.append(route.request.url)
                    if scenario == "render":
                        body = "export function ClientsPanel(){window.__renders=(window.__renders||0)+1;if(!window.__recoverRender)throw new Error('controlled render failure');return '自动恢复成功'}"
                        route.fulfill(body=body, content_type="text/javascript")
                    elif scenario in ["persistent", "navigate", "offline", "hidden", "storage"] or len(attempts) == 1:
                        route.abort()
                    else:
                        route.fulfill(body="export function ClientsPanel(){return '自动恢复成功'}", content_type="text/javascript")
                page.route("**/assets/Clients-*.js", module)
                page.goto(f"http://localhost:{server.server_port}/#/clients")
                failed = page.get_by_role("heading", name="管理页面加载失败", exact=True)
                expect(failed).to_be_visible()
                expect(page.get_by_text("正在自动重新连接…", exact=True)).to_be_visible()
                expect(page.get_by_role("button", name="重新加载页面", exact=True)).to_have_count(0)

                if scenario == "render":
                    first_renders = page.evaluate("window.__renders")
                    page.wait_for_timeout(1900)
                    assert page.evaluate("window.__renders") == first_renders
                    page.wait_for_timeout(1100)
                    expect(failed).to_be_visible()
                    assert page.evaluate("window.__renders") > first_renders
                    page.evaluate("window.__recoverRender=true")
                    page.wait_for_timeout(5000)
                    expect(page.locator(".app-content")).to_contain_text("自动恢复成功")
                    assert len(navigations) == 1, "render recovery reloaded the document"
                    assert len(attempts) == 1, "render recovery fetched an already loaded module"
                    checks.append("render-remount-2s-4s-no-document-or-module-reload")
                elif scenario == "navigate":
                    page.evaluate("location.hash='/account'")
                    expect(page.get_by_text("已登录：owner", exact=True)).to_be_visible()
                    count = len(navigations)
                    page.wait_for_timeout(7000)
                    assert len(navigations) == count and len(attempts) == 1
                    checks.append("navigation-cancels-recovery")
                elif scenario in ["offline", "hidden"]:
                    if scenario == "offline": context.set_offline(True)
                    else: page.evaluate("Object.defineProperty(document, 'visibilityState', {configurable:true,get:()=>'hidden'});document.dispatchEvent(new Event('visibilitychange'))")
                    page.wait_for_timeout(7000)
                    assert len(navigations) == 1 and len(attempts) == 1
                    if scenario == "offline": context.set_offline(False)
                    else: page.evaluate("Object.defineProperty(document, 'visibilityState', {configurable:true,get:()=>'visible'});document.dispatchEvent(new Event('visibilitychange'))")
                    page.wait_for_timeout(3000)
                    expect(failed).to_be_visible()
                    page.wait_for_timeout(5000)
                    page.wait_for_function("sessionStorage.getItem('vaultsyncd.page-recovery-reload') !== null")
                    assert len(navigations) == 2
                    checks.append(scenario + "-pauses-and-resumes")
                else:
                    page.wait_for_timeout(3000)
                    if scenario != "module": expect(failed).to_be_visible()
                    page.wait_for_timeout(5000)
                    if scenario == "module":
                        expect(page.locator(".app-content")).to_contain_text("自动恢复成功")
                        assert len(navigations) == 1 and len(attempts) == 2
                        assert page.evaluate("location.hash") == "#/clients"
                        checks.append("transient-module-fetch-recovers-locally-and-preserves-route")
                    elif scenario == "storage":
                        page.wait_for_timeout(7000)
                        assert len(navigations) == 1
                        expect(failed).to_be_visible()
                        checks.append("blocked-storage-never-reloads-without-cooldown")
                    else:
                        page.wait_for_function("sessionStorage.getItem('vaultsyncd.page-recovery-reload') !== null")
                        expect(failed).to_be_visible()
                        page.wait_for_timeout(7000)
                        assert len(navigations) == 2 and 3 <= len(attempts) <= 6, "persistent module fault caused reload loop"
                        expect(failed).to_be_visible()
                        checks.append("persistent-module-failure-five-minute-reload-cooldown")
                assert page.evaluate("window.__csp") == []
                page.screenshot(path=str(OUTPUT / f"{scenario}.png"))
                context.close()
            browser.close()
        (OUTPUT / "result.json").write_text(json.dumps({"passed": True, "checks": checks}, ensure_ascii=False, indent=2))
        print("S211 browser checks passed: " + ", ".join(checks), flush=True)
    finally:
        server.shutdown(); server.server_close()


if __name__ == "__main__": main()
