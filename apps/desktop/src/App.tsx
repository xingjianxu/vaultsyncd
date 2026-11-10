import { useCallback, useEffect, useRef, useState } from "react";
import { AppProvider, EmptyState, Icon, PageShell, StatusBadge } from "@vaultsyncd/ui";
import { Button, Group, Stack, Text } from "@mantine/core";
import { notifications } from "@mantine/notifications";
import type { GetStatusResult } from "@vaultsyncd/local-ipc-types";
import { nativeAgent, type AgentClient } from "./services/agent";
import { Connections } from "./features/connections/Connections";
import { Files } from "./features/files/Files";
import { History } from "./features/history/History";
import { Activity } from "./features/activity/Activity";
import { Problems } from "./features/problems/Problems";
import { Bindings } from "./features/bindings/Bindings";
import { useRoute } from "./route";
const items = [
  { path: "/connections", label: "连接设置", icon: <Icon path="M10 13a5 5 0 0 0 7 0l4-4a5 5 0 0 0-7-7l-2 2 M14 11a5 5 0 0 0-7 0l-4 4a5 5 0 0 0 7 7l2-2" /> },
  { path: "/bindings", label: "同步目录", icon: <Icon path="M3 7h6l2 2h10v11H3z M3 7V4h6l2 3" /> },
  { path: "/files", label: "文件与操作", icon: <Icon path="M3 7h6l2 2h10v11H3z M8 13h8 M8 16h5" /> },
  { path: "/history", label: "历史与回收站", icon: <Icon path="M3 11a9 9 0 1 1 2 7 M3 4v7h7 M12 7v5l3 2" /> },
  { path: "/activity", label: "同步与传输记录", icon: <Icon path="M3 6h18 M3 12h18 M3 18h18 M7 3v6 M17 9v6 M7 15v6" /> },
  { path: "/problems", label: "问题与恢复", icon: <Icon path="M12 3 2 21h20z M12 9v5 M12 17v1" /> },
];
const getStyleNonce = () => typeof document === "undefined" ? "" : document.querySelector<HTMLMetaElement>('meta[name="csp-nonce"]')?.content ?? "";
export function App({ agent = nativeAgent }: { agent?: AgentClient }) {
  return <AppProvider getStyleNonce={getStyleNonce}><AppView agent={agent} /></AppProvider>;
}
function AppView({ agent }: { agent: AgentClient }) {
  const path = useRoute("/connections");
  const [message, setMessage] = useState("");
  const [connectionError, setConnectionError] = useState("");
  const [busy, setBusy] = useState(false);
  const [connected, setConnected] = useState(false);
  const [snapshot, setSnapshot] = useState<GetStatusResult>();
  const executing = useRef(false); const mounted = useRef(true);
  const refresh = useCallback(async () => {
    const result = await agent.getStatus();
    if (!result.management?.connection || !result.management.bindings || !result.bindings) throw new Error("agent 未返回完整连接与绑定状态。");
    if (mounted.current) { setSnapshot(result); setConnected(true); setConnectionError(""); }
  }, [agent]);
  function failure(error: unknown) { if (mounted.current) { setConnected(false); setConnectionError(error instanceof Error ? error.message : "本机 agent 请求失败。"); } }
  function run(action: () => Promise<void>) {
    if (executing.current) return;
    executing.current = true; setBusy(true); setMessage("操作正在执行，等待 agent 确认。");
    const notification = notifications.show({ message: "操作正在执行，等待 agent 确认。", loading: true, autoClose: false, withCloseButton: false });
    void action().then(() => {
      if (mounted.current) {
        setMessage("操作已由 agent 确认。");
        notifications.update({ id: notification, message: "操作已由 agent 确认。", color: "green", loading: false, autoClose: 4000, withCloseButton: true });
      }
    }).catch(async error => {
      if (!mounted.current) return;
      const detail = error instanceof Error ? error.message : "本机 agent 请求失败。";
      setMessage(detail);
      notifications.update({ id: notification, message: detail, color: "red", loading: false, autoClose: false, withCloseButton: true });
      // A rejected business operation can still have a live local IPC connection.
      try { await refresh(); } catch (connectionError) { failure(connectionError); }
    }).finally(() => { executing.current = false; if (mounted.current) setBusy(false); });
  }
  useEffect(() => {
    mounted.current = true; let reading = false;
    async function poll() { if (reading || executing.current) return; reading = true; try { await refresh(); } catch (error) { failure(error); } finally { reading = false; } }
    void poll(); const timer = window.setInterval(() => void poll(), 2000);
    return () => { mounted.current = false; window.clearInterval(timer); };
  }, [refresh]);
  return <PageShell product="桌面客户端" items={items} activePath={path} homePath="/connections" status={<StatusBadge tone={connected ? "success" : "neutral"}>{connected ? "agent 已连接" : "未连接"}</StatusBadge>}><Stack gap="lg" className="app-content desktop-content">
    <Group justify="space-between" align="center"><Text size="sm" c="dimmed" role="status" data-agent-feedback>{connectionError || message || (connected ? "本机 agent 状态已读取。" : "正在读取本机 agent 状态。")}</Text><Button disabled={busy} onClick={() => run(refresh)}>检查本机连接</Button></Group>
    {path === "/connections" ? <Connections agent={agent} connection={snapshot?.management?.connection} changed={refresh} run={run} busy={busy} />
    : path === "/bindings" ? <Bindings agent={agent} bindings={snapshot?.management?.bindings} states={snapshot?.bindings} read={connected} changed={refresh} run={run} busy={busy} />
    : path === "/files" ? <Files agent={agent} bindings={snapshot?.management?.bindings} changed={refresh} run={run} busy={busy} />
    : path === "/history" ? <History agent={agent} bindings={snapshot?.management?.bindings} changed={refresh} run={run} busy={busy} />
    : path === "/activity" ? <Activity key={`${connected}:${snapshot?.management?.connection?.server_id}:${snapshot?.management?.connection?.server_url}:${snapshot?.management?.connection?.authenticated}`} agent={agent} bindings={snapshot?.management?.bindings} authenticated={connected && snapshot?.management?.connection?.authenticated === true} />
    : path === "/problems" ? <Problems agent={agent} current={snapshot?.management?.problems} notices={snapshot?.management?.notices} run={run} busy={busy} />
    : <EmptyState title="页面不存在">请从导航选择页面。</EmptyState>}
  </Stack></PageShell>;
}
