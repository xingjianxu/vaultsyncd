import { useState } from "react";
import { Button, EmptyState, PageShell, StatusBadge, Table } from "@vaultsyncd/ui";
import { disconnectedAgent, type AgentClient } from "./services/agent";
import { useRoute } from "./route";

const items = [{ path: "/connections", label: "连接设置" }, { path: "/bindings", label: "同步目录" }, { path: "/problems", label: "问题与恢复" }];
export function App({ agent = disconnectedAgent }: { agent?: AgentClient }) {
  const path = useRoute("/connections");
  const [message, setMessage] = useState("未连接本机 agent。");
  const [busy, setBusy] = useState(false);
  async function check() {
    setBusy(true);
    try {
      const result = await agent.getStatus();
      setMessage(`agent ${result.agent_version} 已响应；绑定状态需通过管理页面读取。`);
    } catch (error) { setMessage(error instanceof Error ? error.message : "本机 agent 请求失败。"); }
    finally { setBusy(false); }
  }
  return <PageShell product="桌面客户端" items={items} activePath={path}
    status={<StatusBadge>未连接</StatusBadge>}>
    {path === "/connections" ? <><div className="vs-eyebrow">CLIENT / CONNECTIONS</div><h1>连接设置</h1>
      <p>桌面界面通过本机 agent 管理同步。</p><section className="vs-card"><h2>本机连接</h2>
      <p aria-live="polite">{message}</p><StatusBadge tone="warning">IPC 测试替身</StatusBadge>
      <p>此应用壳尚未接入 Tauri 桥接。</p>
      <Button disabled={busy} onClick={() => void check()}>{busy ? "检查中…" : "检查本机连接"}</Button></section></>
    : path === "/bindings" ? <><h1>同步目录</h1><Table<{ id: string; name: string }> caption="本地 Binding"
      columns={[{ key: "name", title: "目录", render: row => row.name }]} rows={[]} rowKey={row => row.id}
      emptyMessage="未连接，尚未读取同步目录。" /></>
    : path === "/problems" ? <><h1>问题与恢复</h1><EmptyState title="尚未读取问题">连接本机 agent 后可查看问题原因和恢复操作。</EmptyState></>
    : <EmptyState title="页面不存在">请从导航选择页面。</EmptyState>}
  </PageShell>;
}
