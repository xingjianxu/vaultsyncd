import { ArrowLeftRight, Folder, FolderSearch, History as HistoryIcon, Link, TriangleAlert } from "lucide-react";
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

declare const __APP_VERSION__: string;

const items = [
  { path: "/bindings", label: "同步目录", icon: <Icon icon={Folder} /> },
  { path: "/connections", label: "连接设置", icon: <Icon icon={Link} /> },
  { path: "/files", label: "文件与操作", icon: <Icon icon={FolderSearch} /> },
  { path: "/history", label: "历史与回收站", icon: <Icon icon={HistoryIcon} /> },
  { path: "/activity", label: "同步与传输记录", icon: <Icon icon={ArrowLeftRight} /> },
  { path: "/problems", label: "问题与恢复", icon: <Icon icon={TriangleAlert} /> },
];
const getStyleNonce = () => typeof document === "undefined" ? "" : document.querySelector<HTMLMetaElement>('meta[name="csp-nonce"]')?.content ?? "";
export function App({ agent = nativeAgent }: { agent?: AgentClient }) {
  return <AppProvider getStyleNonce={getStyleNonce}><AppView agent={agent} /></AppProvider>;
}
function AppView({ agent }: { agent: AgentClient }) {
  const route = useRoute("/bindings");
  const addingServer = route === "/connections/add";
  const path = addingServer ? "/connections" : route;
  const [message, setMessage] = useState("");
  const [connectionError, setConnectionError] = useState("");
  const [busy, setBusy] = useState(false);
  const [connected, setConnected] = useState(false);
  const [snapshot, setSnapshot] = useState<GetStatusResult>();
  const executing = useRef(false); const mounted = useRef(true);
  const statusRead = useRef(0);
  const refresh = useCallback(async () => {
    const request = ++statusRead.current;
    try {
      const result = await agent.getStatus();
      if (!result.management?.connections || !result.management.bindings || !result.bindings) {
        throw new Error("agent 未返回完整连接与绑定状态。");
      }
      if (mounted.current && request === statusRead.current) {
        setSnapshot(result);
        setConnected(true);
        setConnectionError("");
      }
    } catch (error) {
      if (mounted.current && request === statusRead.current) throw error;
    }
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
  const management = snapshot?.management;
  const connections = management?.connections ?? [];
  // Detachment keeps recovery records in the agent, not active desktop entries.
  const bindings = management?.bindings?.filter(binding => !binding.detached);
  // Reset selected bindings, cached pages and in-flight reads when membership changes.
  const bindingScope = JSON.stringify(bindings?.map(binding => binding.binding_id).sort());
  return <PageShell product="桌面客户端" items={items} activePath={path} homePath="/bindings"
    sidebarFooter={<Text size="xs" c="dimmed" data-client-version>vaultsyncd {__APP_VERSION__}</Text>} status={<StatusBadge tone={connected ? "success" : "neutral"}>{connected ? "agent 已连接" : "未连接"}</StatusBadge>}><Stack gap="lg" className="app-content desktop-content">
    <Group justify="space-between" align="center"><Text size="sm" c="dimmed" role="status" data-agent-feedback>{connectionError || message || (connected ? "本机 agent 状态已读取。" : "正在读取本机 agent 状态。")}</Text><Button disabled={busy} onClick={() => run(refresh)}>检查本机连接</Button></Group>
    {path === "/connections" ? <Connections agent={agent} connections={connections} adding={addingServer} read={connected} changed={refresh} run={run} busy={busy} />
    : path === "/bindings" ? <Bindings agent={agent} connections={connections} bindings={bindings} states={snapshot?.bindings} read={connected} changed={refresh} run={run} busy={busy} />
    : path === "/files" ? <Files key={bindingScope} agent={agent} bindings={bindings} connections={connected ? connections : []} changed={refresh} run={run} busy={busy} />
    : path === "/history" ? <History key={bindingScope} agent={agent} bindings={bindings} connections={connected ? connections : []} changed={refresh} run={run} busy={busy} />
    : path === "/activity" ? <Activity key={bindingScope} agent={agent} bindings={bindings} connections={connected ? connections : []} />
    : path === "/problems" ? <Problems agent={agent} current={management?.problems} notices={management?.notices} run={run} busy={busy} />
    : <EmptyState title="页面不存在">请从导航选择页面。</EmptyState>}
  </Stack></PageShell>;
}
