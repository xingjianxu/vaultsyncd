import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "vite";

const server = await createServer({
  root: fileURLToPath(new URL("../../..", import.meta.url)), configFile: false,
  server: { middlewareMode: true, hmr: false }, ssr: { noExternal: ["@vaultsyncd/ui"] },
});
try {
  const { Activity, activityResult, activityTime, activityFile } = await server.ssrLoadModule("/apps/desktop/src/features/activity/Activity.tsx");
  const { AppProvider } = await server.ssrLoadModule("/packages/ui/src/index.ts");
  const { disconnectedAgent } = await server.ssrLoadModule("/apps/desktop/src/services/agent.ts");
  const { decodeManagementResult } = await server.ssrLoadModule("/packages/local-ipc-types/src/index.ts");
  const row = { id: "9007199254740993", occurred_at: "1794300000", client_label: "客户端A", client_kind: "native", direction: "download", action: "download", status: "served", path: "目录/文件.txt", size: "9007199254740993", bytes: "0", offset: "9007199254740993" };
  assert.equal(activityResult(row), "已提供下载内容");
  assert.doesNotMatch(activityResult(row), /下载完成|已同步|已应用/);
  assert.equal(activityResult({ ...row, action: "upload_finalize", status: "ready" }), "上传内容已校验");
  assert.equal(activityResult({ ...row, action: "sync_commit", status: "committed" }), "同步已提交");
  assert.equal(activityResult({ ...row, status: "failed" }), "失败");
  assert.equal(activityResult({ ...row, action: "future", status: "future" }), "future · future");
  assert.equal(activityFile({ ...row, path: undefined, action: "upload_finalize" }), "待提交的上传内容");
  assert.equal(activityTime("9007199254740993"), "UTC秒 9007199254740993");
  assert.equal(activityTime("unknown"), "UTC秒 unknown");
  assert.notEqual(activityTime("1794300000"), "UTC秒 1794300000");
  assert.deepEqual(JSON.parse(JSON.stringify(decodeManagementResult({ activity: [row], next_cursor: row.id }).activity[0])), row);
  assert.throws(() => decodeManagementResult({ activity: [{ ...row, bytes: 4 }] }), /bytes/);
  const html = renderToStaticMarkup(createElement(AppProvider, {}, createElement(Activity, { agent: disconnectedAgent })));
  assert.match(html, /同步与传输记录/);
  assert.match(html, /服务端记录/);
  assert.match(html, /所有客户端/);
  assert.match(html, /请选择绑定/);
  assert.doesNotMatch(html, /下载完成|同步成功/);
  await assert.rejects(disconnectedAgent.listActivity("11".repeat(16)), /IPC 测试替身/);
  console.log("desktop activity: 服务端观察语义、未知值、无损计数、schema拒绝错误类型、初始页面与未连接错误通过");
} finally { await server.close(); }
