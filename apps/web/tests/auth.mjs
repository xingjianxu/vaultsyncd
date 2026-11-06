import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createServer, loadConfigFromFile } from "vite";
const server = await createServer({ root: fileURLToPath(new URL("../../..", import.meta.url)), configFile: false,
  server: { middlewareMode: true, hmr: false }, ssr: { noExternal: ["@vaultsyncd/ui", "@vaultsyncd/api-client"] } });
try {
  const { BrowserAuth, SignedOut } = await server.ssrLoadModule("/apps/web/src/services/auth.ts");
  let refreshed = false, refreshCount = 0, accountCount = 0;
  const options = { base_url: "https://vault.test", request_id: () => "auth-test", fetch: async (url, init) => {
    assert.equal(init.credentials, "same-origin"); assert.equal(init.cache, "no-store");
    if (url.endsWith("/browser/refresh")) {
      refreshCount++; await new Promise((resolve) => setTimeout(resolve, 20)); refreshed = true;
      return Response.json({ session_id: "session", access_expires_at: "2000" });
    }
    accountCount++;
    return refreshed ? Response.json({ user_id: "owner", login: "owner", session_id: "session" })
      : Response.json({ error: { code: "AUTH_EXPIRED", message: "expired", request_id: "auth", retryable: false } }, { status: 401 });
  } };
  const auth = new BrowserAuth(options);
  const accounts = await Promise.all([auth.account(), auth.account(), auth.account()]);
  assert.equal(accounts.length, 3); assert.equal(refreshCount, 1); assert.equal(accountCount, 6);
  const expired = new BrowserAuth({ ...options, fetch: async () => Response.json({ error: { code: "AUTH_EXPIRED", message: "expired", request_id: "auth", retryable: false } }, { status: 401 }) });
  await assert.rejects(expired.account(), SignedOut);
  const offline = new BrowserAuth({ ...options, fetch: async () => { throw new Error("offline"); } });
  await assert.rejects(offline.account(), /offline/);
  const configPath = fileURLToPath(new URL("../vite.config.ts", import.meta.url));
  const oldTarget = process.env.VAULTSYNCD_DEV_API;
  try {
    process.env.VAULTSYNCD_DEV_API = "http://127.0.0.1:8765";
    const config = await loadConfigFromFile({ command: "serve", mode: "development" }, configPath);
    assert.equal(config.config.server.cors, false);
    assert.equal(config.config.server.proxy["/v1"].changeOrigin, false);
    process.env.VAULTSYNCD_DEV_API = "https://evil.invalid";
    await assert.rejects(loadConfigFromFile({ command: "serve", mode: "development" }, configPath), /只接受/);
  } finally {
    if (oldTarget === undefined) delete process.env.VAULTSYNCD_DEV_API;
    else process.env.VAULTSYNCD_DEV_API = oldTarget;
  }
  console.log("auth: 同源cookie传输、并发401只刷新一次、失效登录与网络故障分类通过");
} finally { await server.close(); }
