import { useCallback, useEffect, useRef, useState } from "react";
import { Button, EmptyState, PageShell, StatusBadge } from "@vaultsyncd/ui";
import type { GetStatusResult } from "@vaultsyncd/local-ipc-types";
import { nativeAgent, type AgentClient } from "./services/agent";
import { Connections } from "./features/connections/Connections";
import { Files } from "./features/files/Files";
import { Problems } from "./features/problems/Problems";
import { Bindings } from "./features/bindings/Bindings";
import { useRoute } from "./route";
const items = [{ path: "/connections", label: "连接设置" }, { path: "/bindings", label: "同步目录" }, { path: "/files", label: "文件与操作" }, { path: "/problems", label: "问题与恢复" }];
export function App({ agent = nativeAgent }: { agent?: AgentClient }) {
  const path = useRoute("/connections");
  const [message, setMessage] = useState("未连接本机 agent。");
  const [busy, setBusy] = useState(false);
  const [connected, setConnected] = useState(false);
  const [snapshot, setSnapshot] = useState<GetStatusResult>();
  const executing = useRef(false); const mounted = useRef(true);
  const refresh = useCallback(async () => {
    const result = await agent.getStatus();
    if (!result.management?.connection || !result.management.bindings || !result.bindings) throw new Error("agent 未返回完整连接与绑定状态。");
    if (mounted.current) { setSnapshot(result); setConnected(true); }
  }, [agent]);
  function failure(error: unknown) { if (mounted.current) { setConnected(false); setMessage(error instanceof Error ? error.message : "本机 agent 请求失败。"); } }
  function run(action: () => Promise<void>) {
    if (executing.current) return;
    executing.current = true; setBusy(true); setMessage("操作正在执行，等待 agent 确认。");
    void action().then(() => { if (mounted.current) setMessage("操作已由 agent 确认。"); }).catch(failure).finally(() => { executing.current = false; if (mounted.current) setBusy(false); });
  }
  useEffect(() => {
    mounted.current = true; let reading = false;
    async function poll() { if (reading || executing.current) return; reading = true; try { await refresh(); } catch (error) { failure(error); } finally { reading = false; } }
    void poll(); const timer = window.setInterval(() => void poll(), 2000);
    return () => { mounted.current = false; window.clearInterval(timer); };
  }, [refresh]);
  return <PageShell product="桌面客户端" items={items} activePath={path} status={<StatusBadge>{connected ? "agent 已连接" : "未连接"}</StatusBadge>}>
    <p role="status">{message}</p><Button disabled={busy} onClick={() => run(refresh)}>检查本机连接</Button>
    {path === "/connections" ? <Connections agent={agent} connection={snapshot?.management?.connection} changed={refresh} run={run} busy={busy} />
    : path === "/bindings" ? <Bindings agent={agent} bindings={snapshot?.management?.bindings} states={snapshot?.bindings} read={connected} changed={refresh} run={run} busy={busy} />
    : path === "/files" ? <Files agent={agent} bindings={snapshot?.management?.bindings} changed={refresh} run={run} busy={busy} />
    : path === "/problems" ? <Problems agent={agent} run={run} busy={busy} />
    : <EmptyState title="页面不存在">请从导航选择页面。</EmptyState>}
  </PageShell>;
}
