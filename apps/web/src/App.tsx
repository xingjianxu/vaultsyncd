import { Component, type ReactNode, lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import { Alert, Anchor, Button, Card, Divider, Group, Loader, SimpleGrid, Stack, Text, ThemeIcon, Title } from "@mantine/core";
import { AppProvider, Icon, PageShell } from "@vaultsyncd/ui";
import logoUrl from "../../../logo.svg?no-inline";
import { showFeedback } from "./components/Feedback";
import { ServerIdentity } from "./components/ServerIdentity";
import { FileActionsProvider } from "./features/files/FileActions";
import { EmptyState, StatusBadge } from "./components/ui";
import { checkServer } from "./services/server";
import { useRoute } from "./route";
import { AccountPanel, AuthGate, AuthProvider, useAuth } from "./features/auth/Auth";
const ClientsPanel = lazy(() => import("./features/clients/Clients").then(module => ({ default: module.ClientsPanel })));
const ActivityPanel = lazy(() => import("./features/activity/Activity").then(module => ({ default: module.ActivityPanel })));
const BackupPanel = lazy(() => import("./features/backup/Backup").then(module => ({ default: module.BackupPanel })));
const RecoveryPanel = lazy(() => import("./features/history/Recovery").then(module => ({ default: module.RecoveryPanel })));
const VaultsPanel = lazy(() => import("./features/vaults/Vaults").then(module => ({ default: module.VaultsPanel })));
const FilesPanel = lazy(() => import("./features/files/Files").then(module => ({ default: module.FilesPanel })));

const items = [
  { path: "/overview", label: "服务概览", description: "服务状态与管理入口，一目了然。", icon: "M3 3h7v7H3z M14 3h7v7h-7z M3 14h7v7H3z M14 14h7v7h-7z" },
  { path: "/vaults", label: "Vault 目录", description: "管理服务器目录，核对挂载健康与采集进度。", icon: "M3 7h6l2 2h10v11H3z M3 7V4h6l2 3" },
  { path: "/files", label: "文件浏览器", description: "切换 Vault、浏览目录并查看文件属性。", icon: "M3 7h6l2 2h10v11H3z M3 7V4h6l2 3 M8 13h8 M8 16h5" },
  { path: "/recovery", label: "历史与恢复", description: "查找历史版本、冲突副本和回收站内容。", icon: "M3 11a9 9 0 1 1 2 7 M3 4v7h7 M12 7v5l3 2" },
  { path: "/backup", label: "备份管理", description: "准备一致备份，导出并验证独立归档。", icon: "M4 8h16v13H4z M3 3h18v5H3z M9 12h6" },
  { path: "/clients", label: "客户端", description: "查看连接记录与最近活动。", icon: "M3 4h18v12H3z M8 21h8 M12 16v5" },
  { path: "/activity", label: "同步与传输记录", description: "查看客户端上传、同步提交与下载内容记录。", icon: "M3 7h14 M13 3l4 4-4 4 M21 17H7 M11 13l-4 4 4 4" },
  { path: "/account", label: "账号与会话", description: "管理登录身份、密码与设备会话。", icon: "M16 7a4 4 0 1 1-8 0 4 4 0 0 1 8 0 M4 21v-2a8 8 0 0 1 16 0v2" },
];
class PageBoundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() { return { failed: true }; }
  render() {
    if (this.state.failed) return <EmptyState title="管理页面加载失败"><Stack><Text>请检查连接后重新加载页面。</Text><Button onClick={() => window.location.reload()}>重新加载页面</Button></Stack></EmptyState>;
    return this.props.children;
  }
}
const getStyleNonce = () => typeof document === "undefined" ? "" : document.querySelector<HTMLMetaElement>('meta[name="csp-nonce"]')?.content ?? "";
export function App() { return <AppProvider getStyleNonce={getStyleNonce}><AuthProvider><AuthGate><AppView /></AuthGate></AuthProvider></AppProvider>; }
function AppView() {
  const auth = useAuth();
  const location = useRoute("/overview");
  const path = location.startsWith("/files/") ? "/files" : location;
  const active = items.find(item => item.path === path);
  const [status, setStatus] = useState("正在检查");
  const [detail, setDetail] = useState("正在获取服务状态。");
  const [busy, setBusy] = useState(false);
  const [checkedAt, setCheckedAt] = useState<string>();
  const controller = useRef<AbortController | null>(null);
  const connect = useCallback(async () => {
    if (controller.current) return;
    const current = new AbortController(); controller.current = current;
    setBusy(true); setStatus("正在检查");
    try {
      const result = await checkServer(undefined, current.signal);
      if (current.signal.aborted) return;
      setStatus(result.status === "ok" ? "服务可用" : result.status === "degraded" ? "服务降级" : "需要恢复");
      setDetail("健康检查已返回。");
      setCheckedAt(new Date().toLocaleTimeString("zh-CN"));
      showFeedback(result.status === "ok" ? "服务可用" : result.status === "degraded" ? "服务降级" : "需要恢复", result.status === "ok" ? "green" : "yellow");
    } catch (error) {
      if (current.signal.aborted) return;
      const message = error instanceof Error ? error.message : "连接失败，请检查服务地址。";
      setStatus("未连接"); setDetail(message); showFeedback(message, "red");
    } finally { if (!current.signal.aborted) { controller.current = null; setBusy(false); } }
  }, []);
  useEffect(() => {
    void connect();
    return () => { controller.current?.abort(); controller.current = null; };
  }, [connect]);
  const connection = <StatusBadge tone={status === "服务可用" ? "success" : status === "服务降级" || status === "需要恢复" ? "warning" : "neutral"}>{status}</StatusBadge>;
  return <PageShell product="服务管理" brandIcon={<img src={logoUrl} alt="" width={28} height={28} />} items={items.map(item => ({ ...item, icon: <Icon path={item.icon} /> }))} activePath={path} status={connection}
    sidebarTitle="服务端控制台" sidebarFooter={<Text size="xs" c="dimmed">文件同步与恢复 · 服务管理</Text>}
    headerActions={<Anchor href="#/account" size="sm" visibleFrom="xs">{auth.account?.login ?? "登录账号"}</Anchor>}
    contentWidth={path === "/files" ? "100%" : "xl"}>
        <FileActionsProvider key={auth.account?.user_id ?? "signed-out"}><Stack gap="lg" className="app-content">{path !== "/files" && path !== "/clients" && <Stack gap={4}><Title order={1} size="h2">{active?.label ?? "页面不存在"}</Title>{path !== "/vaults" && path !== "/activity" && <Text c="dimmed" size="sm">{active?.description ?? "请从导航选择页面。"}</Text>}</Stack>}
          <PageBoundary key={path}><Suspense fallback={<Group role="status"><Loader size="sm" /><Text>正在加载管理页面…</Text></Group>}>{path === "/overview" ? <>
            <Card withBorder><Stack><Group justify="space-between"><div><Title order={2} size="h3">服务连接</Title><Text size="sm" c="dimmed">健康检查与目录采集状态分别核对</Text></div><div role="status">{connection}</div></Group><Text size="sm" aria-live="polite">{detail}</Text>{checkedAt && <Text size="xs" c="dimmed">最近成功检查 · {checkedAt}</Text>}<ServerIdentity /><Divider /><Group justify="space-between"><Text size="xs" c="dimmed">{busy ? "正在获取服务状态" : "按需检查当前连接"}</Text><Button disabled={busy} onClick={() => void connect()}>{busy ? "检查中…" : "检查服务连接"}</Button></Group></Stack></Card>
            <Group justify="space-between"><Title order={2} size="h3">管理工作空间</Title><Text size="xs" c="dimmed">{items.length - 1} 个管理入口</Text></Group>
            <SimpleGrid cols={{ base: 1, sm: 2, xl: 4 }}>{items.slice(1).map(item => <Card withBorder key={item.path}><Stack h="100%"><Group><ThemeIcon variant="light" size="lg"><Icon path={item.icon} /></ThemeIcon><Title order={3} size="h4">{item.label}</Title></Group><Text size="sm" c="dimmed" flex={1}>{item.description}</Text><Anchor href={`#${item.path}`} size="sm">进入管理 →</Anchor></Stack></Card>)}</SimpleGrid>
            <Alert>以服务端实际状态为准。连接可用不代表文件同步完成，备份下载完成后仍需重读验证。</Alert>
          </> : path === "/vaults" ? <VaultsPanel /> : path === "/files" ? <FilesPanel location={location} /> : path === "/recovery" ? <RecoveryPanel /> : path === "/backup" ? <BackupPanel /> : path === "/clients" ? <ClientsPanel /> : path === "/activity" ? <ActivityPanel /> : path === "/account" ? <AccountPanel /> : <EmptyState title="页面不存在">请从导航选择页面。</EmptyState>}</Suspense></PageBoundary>
          {path !== "/files" && <><Divider /><Group component="footer" justify="space-between"><Text size="xs" c="dimmed">vaultsyncd</Text><Text size="xs" c="dimmed">文件同步与恢复 · 服务管理</Text></Group></>}
        </Stack></FileActionsProvider>
  </PageShell>;
}
