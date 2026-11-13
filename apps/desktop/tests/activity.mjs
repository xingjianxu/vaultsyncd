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
  const { AppProvider, ActivityAction, activityOutcome } = await server.ssrLoadModule("/packages/ui/src/index.ts");
  const { disconnectedAgent } = await server.ssrLoadModule("/apps/desktop/src/services/agent.ts");
  const { decodeManagementResult } = await server.ssrLoadModule("/packages/local-ipc-types/src/index.ts");
  const row = { id: "9007199254740993", occurred_at: "1794300000", client_label: "客户端A", client_kind: "native", direction: "download", action: "download", status: "served", path: "目录/文件.txt", size: "9007199254740993", bytes: "0", offset: "9007199254740993" };
  assert.equal(activityResult(row), "已提供下载内容");
  assert.doesNotMatch(activityResult(row), /下载完成|已同步|已应用/);
  assert.equal(activityResult({ ...row, action: "upload_finalize", status: "ready" }), "上传内容已校验");
  assert.equal(activityResult({ ...row, action: "sync_commit", status: "committed" }), "同步已提交");
  const actions = [
    ["create_directory", "新建目录", "目录已创建"],
    ["move", "移动/重命名", "移动/重命名已提交"],
    ["delete", "删除", "删除已提交"],
    ["restore_version", "恢复历史版本", "历史版本已恢复"],
    ["restore_deleted", "从回收站恢复", "回收站条目已恢复"],
  ];
  for (const [action, label, expected] of actions) {
    const operation = { ...row, direction: "operation", action, status: "committed" };
    assert.equal(activityResult(operation), expected);
    assert.equal(activityOutcome(operation), expected);
    assert.doesNotMatch(expected, /上传|下载|客户端.*已/);
    assert.deepEqual(JSON.parse(JSON.stringify(decodeManagementResult({ activity: [operation] }).activity[0])), operation);
    const icon = renderToStaticMarkup(createElement(ActivityAction, operation));
    assert.match(icon, new RegExp(`aria-label="${label}"`));
    assert.doesNotMatch(icon, /aria-label="上传"|aria-label="下载"/);
    assert.equal(activityOutcome({ ...operation, status: "pending" }), `${action} · pending`);
  }
  for (const action of ["future", "constructor", "toString"]) {
    assert.equal(activityOutcome({ action, status: "committed" }), `${action} · committed`);
    assert.equal(renderToStaticMarkup(createElement(ActivityAction, { direction: "operation", action })), action);
  }
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
  assert.match(html, /<option value="operation">文件操作<\/option>/);
  assert.doesNotMatch(html, /下载完成|同步成功/);
  await assert.rejects(disconnectedAgent.listActivity("11".repeat(16)), /IPC 测试替身/);
  console.log("desktop activity: 服务端观察语义、未知值、无损计数、schema拒绝错误类型、初始页面与未连接错误通过");
} finally { await server.close(); }
