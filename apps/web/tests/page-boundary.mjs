import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

const server = await createServer({ root: fileURLToPath(new URL("../../..", import.meta.url)), configFile: false,
  server: { middlewareMode: true, hmr: false }, ssr: { noExternal: ["@vaultsyncd/ui"] } });
try {
  const { PageBoundary } = await server.ssrLoadModule("/apps/web/src/components/PageBoundary.tsx");
  const keys = ["window", "document", "navigator", "sessionStorage", "setTimeout", "clearTimeout"];
  const originals = keys.map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]);
  const now = Date.now;
  let clock = 1_000_000, reloads = 0, sequence = 0;
  const jobs = new Map(), storage = new Map();
  const window = Object.assign(new EventTarget(), { location: { reload() { reloads++; } } });
  const document = Object.assign(new EventTarget(), { visibilityState: "visible" });
  const navigator = { onLine: true };
  const globals = { window, document, navigator,
    sessionStorage: { getItem: key => storage.get(key) ?? null, setItem: (key, value) => storage.set(key, value) },
    setTimeout: (run, delay) => { const id = ++sequence; jobs.set(id, { run, delay }); return id; },
    clearTimeout: id => jobs.delete(id),
  };
  try {
    for (const [key, value] of Object.entries(globals)) Object.defineProperty(globalThis, key, { value, configurable: true });
    Date.now = () => clock;
    const page = new PageBoundary({ children: null });
    page.setState = state => { page.state = { ...page.state, ...state }; };
    page.componentDidMount();
    const fail = error => { page.state.failed = true; page.componentDidCatch(error); };
    const run = () => { assert.equal(jobs.size, 1); const [id, job] = [...jobs][0]; jobs.delete(id); job.run(); };
    const moduleError = new TypeError("Failed to fetch dynamically imported module: /assets/page.js");
    for (const delay of [2000, 4000, 8000, 16_000, 30_000, 30_000]) {
      fail(new Error("render failed"));
      assert.equal([...jobs.values()][0].delay, delay);
      run(); assert.equal(page.state.failed, false);
    }
    assert.equal(reloads, 0, "render failures never reload the document");
    fail(moduleError); run();
    assert.equal(reloads, 1);
    clock += 299_999;
    fail(moduleError); run();
    assert.equal(reloads, 1, "cooldown blocks reload before five minutes");
    clock++;
    fail(moduleError); run();
    assert.equal(reloads, 2, "cooldown permits recovery again at five minutes");
    document.visibilityState = "hidden";
    fail(new Error("render failed")); assert.equal(jobs.size, 0);
    document.visibilityState = "visible";
    document.dispatchEvent(new Event("visibilitychange")); assert.equal(jobs.size, 1);
    navigator.onLine = false;
    run(); assert.equal(jobs.size, 0); assert.equal(page.state.failed, true);
    navigator.onLine = true;
    window.dispatchEvent(new Event("online")); assert.equal(jobs.size, 1);
    page.componentWillUnmount(); assert.equal(jobs.size, 0);
    window.dispatchEvent(new Event("online"));
    document.dispatchEvent(new Event("visibilitychange")); assert.equal(jobs.size, 0, "unmount removes listeners");
    console.log("page boundary: bounded backoff, exact reload cooldown, hidden/offline pause and unmount cleanup passed");
  } finally {
    Date.now = now;
    for (const [key, descriptor] of originals) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor);
      else delete globalThis[key];
    }
  }
} finally { await server.close(); }
