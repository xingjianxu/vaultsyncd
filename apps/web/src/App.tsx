import { Component, type ReactNode, lazy, Suspense, useEffect, useRef, useState } from "react";
import { ActionIcon, Alert, Anchor, AppShell, Breadcrumbs, Burger, Button, Card, Container, Divider, Group, Loader, MantineProvider, NavLink, SimpleGrid, Stack, Text, ThemeIcon, Title, useComputedColorScheme, useMantineColorScheme } from "@mantine/core";
import { useDisclosure } from "@mantine/hooks";
import { EmptyState, StatusBadge } from "./components/ui";
import { checkServer } from "./services/server";
import { useRoute } from "./route";
import { AccountPanel, AuthProvider, useAuth } from "./features/auth/Auth";
const BackupPanel = lazy(() => import("./features/backup/Backup").then(module => ({ default: module.BackupPanel })));
const RecoveryPanel = lazy(() => import("./features/history/Recovery").then(module => ({ default: module.RecoveryPanel })));
const RootsPanel = lazy(() => import("./features/roots/Roots").then(module => ({ default: module.RootsPanel })));

const items = [
  { path: "/overview", label: "服务概览", description: "服务状态与管理入口，一目了然。", icon: "M3 3h7v7H3z M14 3h7v7h-7z M3 14h7v7H3z M14 14h7v7h-7z" },
  { path: "/roots", label: "Root 目录", description: "管理服务器目录，核对挂载健康与采集进度。", icon: "M3 7h6l2 2h10v11H3z M3 7V4h6l2 3" },
  { path: "/recovery", label: "历史与恢复", description: "查找历史版本、冲突副本和回收站内容。", icon: "M3 11a9 9 0 1 1 2 7 M3 4v7h7 M12 7v5l3 2" },
  { path: "/backup", label: "备份管理", description: "准备一致备份，导出并验证独立归档。", icon: "M4 8h16v13H4z M3 3h18v5H3z M9 12h6" },
  { path: "/account", label: "账号与会话", description: "管理登录身份、密码与设备会话。", icon: "M16 7a4 4 0 1 1-8 0 4 4 0 0 1 8 0 M4 21v-2a8 8 0 0 1 16 0v2" },
];
function Icon({ path }: { path: string }) { return <svg className="app-icon" aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round"><path d={path} /></svg>; }
class PageBoundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() { return { failed: true }; }
  render() {
    if (this.state.failed) return <EmptyState title="管理页面加载失败"><Stack><Text>请检查连接后重新加载页面。</Text><Button onClick={() => window.location.reload()}>重新加载页面</Button></Stack></EmptyState>;
    return this.props.children;
  }
}
const getStyleNonce = () => typeof document === "undefined" ? "" : document.querySelector<HTMLMetaElement>('meta[name="csp-nonce"]')?.content ?? "";
export function App() { return <MantineProvider defaultColorScheme="auto" getStyleNonce={getStyleNonce}><AuthProvider><AppView /></AuthProvider></MantineProvider>; }
function AppView() {
  const auth = useAuth();
  const path = useRoute("/overview");
  const active = items.find(item => item.path === path);
  const [opened, { toggle, close }] = useDisclosure();
  const { toggleColorScheme } = useMantineColorScheme();
  const colorScheme = useComputedColorScheme("light");
  const [status, setStatus] = useState("未连接");
  const [detail, setDetail] = useState("尚未检查服务连接。");
  const [busy, setBusy] = useState(false);
  const [checkedAt, setCheckedAt] = useState<string>();
  const controller = useRef<AbortController | null>(null);
  useEffect(() => () => controller.current?.abort(), []);
  async function connect() {
    if (controller.current) return;
    const current = new AbortController(); controller.current = current;
    setBusy(true); setStatus("正在检查");
    try {
      const result = await checkServer(undefined, current.signal);
      if (current.signal.aborted) return;
      setStatus(result.status === "ok" ? "服务可用" : result.status === "degraded" ? "服务降级" : "需要恢复");
      setDetail("健康检查已返回；账号与 Root 状态需登录后查看。");
      setCheckedAt(new Date().toLocaleTimeString("zh-CN"));
    } catch (error) {
      if (current.signal.aborted) return;
      setStatus("未连接"); setDetail(error instanceof Error ? error.message : "连接失败，请检查服务地址。");
    } finally { if (!current.signal.aborted) { controller.current = null; setBusy(false); } }
  }
  const connection = <StatusBadge tone={status === "服务可用" ? "success" : status === "服务降级" || status === "需要恢复" ? "warning" : "neutral"}>{status}</StatusBadge>;
  return <>
    <a className="skip-link" href="#main-content" onClick={event => { event.preventDefault(); document.getElementById("main-content")?.focus(); }}>跳到主要内容</a>
    <AppShell header={{ height: 48 }} navbar={{ width: 240, breakpoint: "sm", collapsed: { mobile: !opened } }} padding="md">
      <AppShell.Header className="app-header">
        <Group className="app-brand" h="100%" px="md" gap="sm" wrap="nowrap"><Burger opened={opened} onClick={toggle} hiddenFrom="sm" size="sm" aria-label="切换导航" aria-controls="main-navigation" /><Anchor href="#/overview" fw={700} c="inherit" underline="never">vaultsyncd</Anchor></Group>
        <Group className="app-topbar" h="100%" px="md" gap="sm" wrap="nowrap">
          <Breadcrumbs visibleFrom="sm" aria-label="页面位置"><Anchor href="#/overview" size="sm">服务管理</Anchor><Text size="sm">{active?.label ?? "页面不存在"}</Text></Breadcrumbs>
          <Group gap="sm" wrap="nowrap"><div role="status">{connection}</div><ActionIcon aria-label="切换主题" variant="default" onClick={() => toggleColorScheme()}><Icon path={colorScheme === "dark" ? "M12 3v2 M12 19v2 M3 12h2 M19 12h2 M5.6 5.6 7 7 M17 17l1.4 1.4 M5.6 18.4 7 17 M17 7l1.4-1.4 M16 12a4 4 0 1 1-8 0 4 4 0 0 1 8 0" : "M20.9 13A9 9 0 0 1 11 3.1 9 9 0 1 0 20.9 13"} /></ActionIcon><Anchor href="#/account" size="sm" visibleFrom="xs">{auth.account?.login ?? "登录账号"}</Anchor></Group>
        </Group>
      </AppShell.Header>
      <AppShell.Navbar p="md" id="main-navigation"><AppShell.Section grow><Text size="xs" c="dimmed" mb="sm">服务端控制台</Text><nav aria-label="主要导航">{items.map(item => <NavLink href={`#${item.path}`} key={item.path} label={item.label} leftSection={<Icon path={item.icon} />} active={path === item.path} aria-current={path === item.path ? "page" : undefined} onClick={close} />)}</nav></AppShell.Section><AppShell.Section><Divider my="md" /><Text size="xs" c="dimmed">文件同步与恢复 · 服务管理</Text></AppShell.Section></AppShell.Navbar>
      <AppShell.Main id="main-content" tabIndex={-1}><Container size="xl" px={0}>
        <Stack gap="lg" className="app-content"><Stack gap={4}><Title order={1} size="h2">{active?.label ?? "页面不存在"}</Title><Text c="dimmed" size="sm">{active?.description ?? "请从导航选择页面。"}</Text></Stack>
          <PageBoundary key={path}><Suspense fallback={<Group role="status"><Loader size="sm" /><Text>正在加载管理页面…</Text></Group>}>{path === "/overview" ? <>
            <Card withBorder><Stack><Group justify="space-between"><div><Title order={2} size="h3">服务连接</Title><Text size="sm" c="dimmed">健康检查与目录采集状态分别核对</Text></div><div role="status">{connection}</div></Group><Text size="sm" aria-live="polite">{detail}</Text>{checkedAt && <Text size="xs" c="dimmed">最近成功检查 · {checkedAt}</Text>}<Divider /><Group justify="space-between"><Text size="xs" c="dimmed">{busy ? "正在获取服务状态" : "按需检查当前连接"}</Text><Button disabled={busy} onClick={() => void connect()}>{busy ? "检查中…" : "检查服务连接"}</Button></Group></Stack></Card>
            <Group justify="space-between"><Title order={2} size="h3">管理工作空间</Title><Text size="xs" c="dimmed">4 个管理入口</Text></Group>
            <SimpleGrid cols={{ base: 1, sm: 2, xl: 4 }}>{items.slice(1).map(item => <Card withBorder key={item.path}><Stack h="100%"><Group><ThemeIcon variant="light" size="lg"><Icon path={item.icon} /></ThemeIcon><Title order={3} size="h4">{item.label}</Title></Group><Text size="sm" c="dimmed" flex={1}>{item.description}</Text><Anchor href={`#${item.path}`} size="sm">进入管理 →</Anchor></Stack></Card>)}</SimpleGrid>
            <Alert>以服务端实际状态为准。连接可用不代表文件同步完成，备份下载完成后仍需重读验证。</Alert>
          </> : path === "/roots" ? <RootsPanel /> : path === "/recovery" ? <RecoveryPanel /> : path === "/backup" ? <BackupPanel /> : path === "/account" ? <AccountPanel /> : <EmptyState title="页面不存在">请从导航选择页面。</EmptyState>}</Suspense></PageBoundary>
          <Divider /><Group component="footer" justify="space-between"><Text size="xs" c="dimmed">vaultsyncd</Text><Text size="xs" c="dimmed">文件同步与恢复 · 服务管理</Text></Group>
        </Stack>
      </Container></AppShell.Main>
    </AppShell>
  </>;
}
