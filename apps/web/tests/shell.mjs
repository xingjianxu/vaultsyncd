import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "vite";

const root = fileURLToPath(new URL("../../..", import.meta.url));
const server = await createServer({
  root, configFile: false,
  define: { __APP_VERSION__: JSON.stringify(JSON.parse(readFileSync(new URL("../../desktop/package.json", import.meta.url), "utf8")).version) },
  server: { middlewareMode: true, hmr: false },
  ssr: { noExternal: ["@vaultsyncd/ui"] },
});
try {
  for (const app of ["web", "desktop"]) {
    const { App } = await server.ssrLoadModule(`/apps/${app}/src/App.tsx`);
    const html = renderToStaticMarkup(createElement(App));
    assert.doesNotMatch(html, /同步成功|已同步|无问题/);
    if (app === "web") {
      assert.match(html, /role="status"/);
      assert.match(html, /核验/);
      assert.doesNotMatch(html, /mantine-AppShell|主要导航|aria-current="page"|服务概览|检查服务连接|name="password"/);
      console.log("web: 会话初始化仅显示核验状态，不渲染管理壳或登录表单");
    } else {
      assert.match(html, /mantine-AppShell|主要导航/);
      assert.match(html, /同步目录/);
      assert.doesNotMatch(html, /name="password"/);
      console.log("desktop: 启动直接显示同步目录，登录对话框按连接操作打开");
    }
  }
  const { disconnectedAgent } = await server.ssrLoadModule("/apps/desktop/src/services/agent.ts");
  await assert.rejects(disconnectedAgent.getStatus(), /IPC 测试替身/);
  const { checkServer } = await server.ssrLoadModule("/apps/web/src/services/server.ts");
  let requested;
  const options = {
    base_url: "https://vault.invalid",
    request_id: () => "s076-request",
    fetch: async (url, init) => {
      requested = { url, init };
      return Response.json({ status: "degraded" });
    },
  };
  const controller = new AbortController();
  assert.equal((await checkServer(options, controller.signal)).status, "degraded");
  assert.equal(requested.url, "https://vault.invalid/v1/health");
  assert.equal(requested.init.headers.get("x-request-id"), "s076-request");
  assert.equal(requested.init.signal, controller.signal);
  await assert.rejects(checkServer({ ...options, fetch: async () => Response.json({ status: "made-up" }) }), /HealthStatus/);
  await assert.rejects(checkServer({ ...options, fetch: async () => { throw new Error("offline"); } }), /offline/);
  console.log("API 服务：真实生成客户端路径、request_id、取消、降级与无效响应/离线均验证");
} finally {
  await server.close();
}
