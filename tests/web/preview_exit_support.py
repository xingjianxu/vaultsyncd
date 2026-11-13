"""Real animation-frame assertions shared by the preview Chromium integrations.

Observe the normal Mantine transition, without changing its duration, mocking
browser APIs or sleeping until the modal has disappeared.
"""
import json
from playwright.sync_api import expect


class PreviewExit:
    def __init__(self, page, evidence):
        self.page, self.evidence = page, evidence
        self.results = []
        self.deletes, self.aborts = [], []
        # Wall-clock times can be compared to the browser's Date.now samples.
        import time
        page.on("request", lambda request: self.deletes.append(int(time.time() * 1000))
                if request.method == "DELETE" and "/office-viewer/" in request.url else None)
        page.on("requestfailed", lambda request: self.aborts.append(int(time.time() * 1000))
                if request.failure and "ERR_ABORTED" in request.failure
                and ("/content" in request.url or "/office-viewer" in request.url) else None)

    def arm(self, selector):
        self.page.wait_for_function("""() => {
            const node = document.querySelector('[role="dialog"]');
            return node && getComputedStyle(node).opacity === '1';
        }""")
        blob = self.page.get_by_role("dialog").locator(selector).get_attribute("src")
        self.blob_url = blob if blob and blob.startswith("blob:") else None
        self.blob_probe = self.page.context.new_page() if self.blob_url else None
        self.page.evaluate("""selector => {
            const dialog = document.querySelector('[role="dialog"]');
            const title = dialog.querySelector('.mantine-Modal-title');
            const body = dialog.querySelector('.mantine-Modal-body');
            const content = dialog.querySelector(selector);
            if (!content) throw Error('No preview content to observe: ' + selector);
            const frame = content.matches('iframe') ? content : content.querySelector('iframe');
            const canvas = content.matches('canvas') ? content : content.querySelector('canvas');
            const pixels = () => canvas?.width && canvas?.height ? Array.from(canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data)
                .reduce((sum, value, index) => (sum + value * (index % 251 + 1)) % 2147483647, 0) : null;
            const measure = () => ({
                title: title.textContent, text: content.textContent,
                html: content.matches('.cm-content,.file-preview-markdown') ? content.innerHTML : null,
                width: dialog.offsetWidth, height: dialog.offsetHeight, bodyHeight: body.offsetHeight,
                contentWidth: content.offsetWidth, contentHeight: content.offsetHeight,
                canvasWidth: canvas?.width, canvasHeight: canvas?.height, pixels: pixels(),
                src: frame?.getAttribute('src'),
            });
            const baseline = measure();
            const state = window.__previewExit = {baseline, frames: [], done: false, stopped: false, blobReadable: null};
            const tick = () => {
                if (state.stopped) return;
                if (!dialog.isConnected) { state.done = true; return; }
                const opacity = Number(getComputedStyle(dialog).opacity);
                if (opacity > 0.05 && opacity < 0.99) {
                    state.frames.push({...measure(), opacity, time: Date.now(),
                        sameContent: dialog.querySelector(selector) === content && content.isConnected,
                        sameFrame: !frame || frame.isConnected});
                }
                requestAnimationFrame(tick);
            };
            requestAnimationFrame(tick);
        }""", selector)
        self.delete_start, self.abort_start = len(self.deletes), len(self.aborts)

    def trigger(self, method="x"):
        if method == "escape":
            # Browser-owned PDF toolbar fields consume Escape inside their
            # extension frame. Exercise the host modal's keyboard handler.
            self.page.get_by_role("button", name="关闭预览", exact=True).focus()
            self.page.keyboard.press("Escape")
        elif method == "overlay":
            self.page.locator(".mantine-Modal-overlay").click(position={"x": 2, "y": 2})
        else:
            self.page.get_by_role("button", name="关闭预览", exact=True).click()

    def verify(self, label, *, reopened=False, require_abort=False):
        if not reopened:
            self.page.wait_for_function("() => window.__previewExit.done")
            expect(self.page.get_by_role("dialog")).not_to_be_visible()
        state = self.page.evaluate("""() => {
            window.__previewExit.stopped = true;
            const dialog = document.querySelector('[role="dialog"]');
            return {...window.__previewExit, observedAt: Date.now(),
                opacityAtVerification: dialog ? Number(getComputedStyle(dialog).opacity) : null};
        }""")
        # Keep failure evidence too, including the actual sampled opacity and geometry.
        self.results.append({"label": label, "requestAborts": self.aborts[self.abort_start:], **state})
        (self.evidence / "exit-frames.json").write_text(json.dumps(self.results, ensure_ascii=False, indent=2))
        assert state["frames"], f"{label}: no real intermediate exit frame observed"
        if require_abort:
            assert self.aborts[self.abort_start:], f"{label}: in-flight request was not aborted during exit"
            if reopened:
                # The failure event has arrived while the old modal is still
                # visibly exiting; do not compare cross-process sub-ms clocks.
                assert 0 < state["opacityAtVerification"] < 1, (label, state)
            else:
                assert self.aborts[self.abort_start] <= state["frames"][-1]["time"], (label, state)
        for sample in state["frames"]:
            assert sample["sameContent"] and sample["sameFrame"], (label, sample)
            assert {key: sample.get(key) for key in state["baseline"]} == state["baseline"], (label, state)
        assert state["blobReadable"] in (None, True), (label, state)
        # Office's server capability must survive the visible transition too.
        assert all(timestamp >= state["frames"][-1]["time"] for timestamp in self.deletes[self.delete_start:]), (label, state)
        return state

    def close(self, selector, label, method="x", *, require_abort=False):
        self.arm(selector)
        self.trigger(method)
        if self.blob_probe:
            # A separate real navigation checks Blob liveness without loosening
            # connect-src or using fetch(blob:) (which the app's CSP forbids).
            self.page.wait_for_function("() => window.__previewExit.frames.length > 0")
            try:
                response = self.blob_probe.goto(self.blob_url, wait_until="commit")
                assert response and response.status == 200
                self.page.evaluate("() => { window.__previewExit.blobReadable = true; }")
            finally:
                self.blob_probe.close()
        return self.verify(label, require_abort=require_abort)

    def reopen(self, selector, name, label, *, require_abort=False):
        self.arm(selector)
        self.trigger("escape")
        self.page.wait_for_function("() => window.__previewExit.frames.length > 0")
        self.verify(label, reopened=True, require_abort=require_abort)
        # The background is still covered by the exiting overlay. Dispatch the
        # same real button's click to exercise a reopen before exit completion.
        self.page.get_by_role("table", name="文件与目录", exact=True).get_by_role("button", name=name, exact=True).evaluate("node => node.click()")
        expect(self.page.get_by_role("dialog", name=f"预览 · {name}", exact=True)).to_be_visible()
        self.page.wait_for_function("""() => {
            const node = document.querySelector('[role="dialog"]');
            return node && getComputedStyle(node).opacity === '1';
        }""")
        # Observe actual animation frames after the new enter completes.
        self.page.evaluate("() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))")
