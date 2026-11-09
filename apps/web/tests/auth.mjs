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
  assert.equal(auth.sessionGeneration, 0, "普通请求及cookie刷新不改变文件任务认证代际");
  const scoped = new BrowserAuth({ ...options, fetch: async () => { throw new Error("offline"); } });
  for (const action of [() => scoped.login("owner", "password"), () => scoped.logout(), () => scoped.changePassword("old", "new"), () => scoped.revokeAll()]) {
    const before = scoped.sessionGeneration;
    const request = action();
    assert.equal(scoped.sessionGeneration, before + 1, "认证动作发出前即使旧任务失效");
    await assert.rejects(request, /offline/);
  }
  const unauthorized = () => Response.json({ error: { code: "AUTH_EXPIRED", message: "expired", request_id: "auth", retryable: false } }, { status: 401 });
  let signedOutCount = 0;
  const expired = new BrowserAuth({ ...options, fetch: async () => unauthorized() }, () => signedOutCount++);
  await assert.rejects(expired.account(), SignedOut);
  assert.equal(signedOutCount, 1, "最终401全局收回界面");
  const offline = new BrowserAuth({ ...options, fetch: async () => { throw new Error("offline"); } }, () => signedOutCount++);
  await assert.rejects(offline.account(), /offline/);
  const refreshOffline = new BrowserAuth({ ...options, fetch: async url => {
    if (url.endsWith("/browser/refresh")) throw new Error("refresh offline");
    return unauthorized();
  } }, () => signedOutCount++);
  await assert.rejects(refreshOffline.account(), /refresh offline/);
  assert.equal(signedOutCount, 1, "传输失败不能冒充凭据失效");

  for (const method of ["logout", "revokeAll", "changePassword"]) {
    let called = 0;
    const current = new BrowserAuth({ ...options, fetch: async () => Response.json({ sessions_revoked: "1", revoked_at: "1000", session_id: "session" }) }, () => called++);
    await current[method]("old password", "new password");
    assert.equal(called, 1, `${method}成功后立即全局退出`);
  }
  // An old successful request and an old 401 are both fenced after a new login.
  for (const lateUnauthorized of [false, true]) {
    let release;
    const late = new Promise(resolve => { release = resolve; });
    let first = true, callbacks = 0;
    const racing = new BrowserAuth({ ...options, fetch: async url => {
      if (url.endsWith("/browser/login")) return Response.json({ session_id: "new", access_expires_at: "2000" });
      if (first) { first = false; return late; }
      return Response.json({ user_id: "owner", login: "owner", session_id: "new" });
    } }, () => callbacks++);
    const oldRequest = racing.account();
    const oldResult = assert.rejects(oldRequest, /会话已变更/);
    assert.equal((await racing.login("owner", "password")).session_id, "new");
    release(lateUnauthorized ? unauthorized() : Response.json({ user_id: "old", login: "old", session_id: "old" }));
    await oldResult;
    assert.equal(callbacks, 0, "旧请求不可注销新会话");
  }
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
