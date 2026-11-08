import { Component, type ReactNode, lazy, Suspense, useEffect, useRef, useState } from "react";
import { Card } from "@heroui/react/card";
import { Breadcrumbs } from "@heroui/react/breadcrumbs";
import { Link } from "@heroui/react/link";
import { Button, EmptyState, StatusBadge } from "./components/ui";
import { checkServer } from "./services/server";
import { useRoute } from "./route";
const BackupPanel = lazy(() => import("./features/backup/Backup").then(module => ({ default: module.BackupPanel })));
const RecoveryPanel = lazy(() => import("./features/history/Recovery").then(module => ({ default: module.RecoveryPanel })));
const RootsPanel = lazy(() => import("./features/roots/Roots").then(module => ({ default: module.RootsPanel })));
import { AccountPanel, AuthProvider, useAuth } from "./features/auth/Auth";

const items = [
  { path: "/overview", label: "服务概览", en: "OVERVIEW", description: "服务状态与管理入口，一目了然。", icon: "M3 3h7v7H3z M14 3h7v7h-7z M3 14h7v7H3z M14 14h7v7h-7z" },
  { path: "/roots", label: "Root 目录", en: "ROOT DIRECTORIES", description: "管理服务器目录，核对挂载健康与采集进度。", icon: "M3 7h6l2 2h10v11H3z M3 7V4h6l2 3" },
  { path: "/recovery", label: "历史与恢复", en: "HISTORY & RECOVERY", description: "查找历史版本、冲突副本和回收站内容。", icon: "M3 11a9 9 0 1 1 2 7 M3 4v7h7 M12 7v5l3 2" },
  { path: "/backup", label: "备份管理", en: "BACKUP", description: "准备一致备份，导出并验证独立归档。", icon: "M4 8h16v13H4z M3 3h18v5H3z M9 12h6" },
  { path: "/account", label: "账号与会话", en: "ACCOUNT & SESSIONS", description: "管理登录身份、密码与设备会话。", icon: "M16 7a4 4 0 1 1-8 0 4 4 0 0 1 8 0 M4 21v-2a8 8 0 0 1 16 0v2" },
];
function Icon({ path }: { path: string }) { return <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round"><path d={path} /></svg>; }
class PageBoundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() { return { failed: true }; }
  render() {
    if (this.state.failed) return <EmptyState title="管理页面加载失败"><span>请检查连接后重新加载页面。</span><Button className="mt-4" onClick={() => window.location.reload()}>重新加载页面</Button></EmptyState>;
    return this.props.children;
  }
}
export function App() { return <AuthProvider><AppView /></AuthProvider>; }
function AppView() {
  const auth = useAuth();
  const path = useRoute("/overview");
  const active = items.find(item => item.path === path);
  const [status, setStatus] = useState("未连接");
  const [detail, setDetail] = useState("尚未检查服务连接。");
  const [busy, setBusy] = useState(false);
  const [checkedAt, setCheckedAt] = useState<string>();
  const [dark, setDark] = useState(() => typeof window !== "undefined" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  const controller = useRef<AbortController | null>(null);
  useEffect(() => () => controller.current?.abort(), []);
  useEffect(() => { document.documentElement.classList.toggle("dark", dark); document.documentElement.dataset.theme = dark ? "dark" : "light"; }, [dark]);
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
  return <div className="admin-shell">
    <a className="skip-link" href="#main-content" onClick={event => { event.preventDefault(); document.getElementById("main-content")?.focus(); }}>跳到主要内容</a>
    <aside className="sidebar">
      <Link href="#/overview" className="brand"><span className="brand-mark"><Icon path="m5 8 7-4 7 4v8l-7 4-7-4z M5 8l7 4 7-4 M12 12v8" /></span><span>vaultsyncd<small>服务端控制台</small></span></Link>
      <div className="nav-label">工作空间</div>
      <nav aria-label="主要导航">{items.map(item => <Link href={`#${item.path}`} key={item.path} aria-current={path === item.path ? "page" : undefined}><Icon path={item.icon} /><span>{item.label}</span>{path === item.path && <span className="nav-indicator" />}</Link>)}</nav>
      <div className="sidebar-foot"><span className="sidebar-foot-label">文件同步与恢复</span><span>vaultsyncd · 服务管理</span></div>
    </aside>
    <div className="workspace">
      <header className="topbar"><Breadcrumbs aria-label="页面位置"><Breadcrumbs.Item href="#/overview">服务管理</Breadcrumbs.Item><Breadcrumbs.Item>{() => <span aria-current="page">{active?.label ?? "页面不存在"}</span>}</Breadcrumbs.Item></Breadcrumbs><div className="topbar-actions"><div role="status">{connection}</div><Button aria-label="切换主题" variant="ghost" isIconOnly onClick={() => setDark(value => !value)}><Icon path={dark ? "M12 3v2 M12 19v2 M3 12h2 M19 12h2 M5.6 5.6 7 7 M17 17l1.4 1.4 M5.6 18.4 7 17 M17 7l1.4-1.4 M16 12a4 4 0 1 1-8 0 4 4 0 0 1 8 0" : "M20.9 13A9 9 0 0 1 11 3.1 9 9 0 1 0 20.9 13"} /></Button><Link href="#/account" className="account-link">{auth.account?.login ?? "登录账号"}</Link></div></header>
      <main id="main-content" tabIndex={-1}>
        <div className="page-heading"><div><h1>{active?.label ?? "页面不存在"}</h1><p>{active?.description ?? "请从导航选择页面。"}</p></div><span className="page-tag">{active?.en ?? "NOT FOUND"}</span></div>
        <div className="page-content"><PageBoundary key={path}><Suspense fallback={<p role="status">正在加载管理页面…</p>}>{path === "/overview" ? <>
          <Card className="connection-card"><Card.Header><div className="connection-title"><span className="quick-icon"><Icon path="M4 9a12 12 0 0 1 16 0 M7 13a7 7 0 0 1 10 0 M10 17a3 3 0 0 1 4 0 M12 21h.01" /></span><div><Card.Title render={props => <h2 {...props} />}>服务连接</Card.Title><Card.Description>健康检查与目录采集状态分别核对</Card.Description></div></div><div role="status">{connection}</div></Card.Header>
            <Card.Content><p aria-live="polite">{detail}</p>{checkedAt && <small>最近成功检查 · {checkedAt}</small>}</Card.Content>
            <Card.Footer><span className="text-xs text-muted">{busy ? "正在获取服务状态" : "按需检查当前连接"}</span><Button variant="primary" disabled={busy} onClick={() => void connect()}><Icon path="M20 7v5h-5 M4 17v-5h5 M5.5 8a7 7 0 0 1 11.6-3L20 8 M4 16l2.9 3a7 7 0 0 0 11.6-3" />{busy ? "检查中…" : "检查服务连接"}</Button></Card.Footer>
          </Card>
          <div className="section-heading"><h2>管理工作空间</h2><span className="text-xs text-muted">4 个管理入口</span></div>
          <div className="quick-grid">{items.slice(1).map(item => <Card className="quick-card" key={item.path}><Card.Header><div className="quick-top"><span className="quick-icon"><Icon path={item.icon} /></span><Card.Title>{item.label}</Card.Title></div><Card.Description>{item.description}</Card.Description></Card.Header><Card.Footer><Link href={`#${item.path}`}>进入管理 <span aria-hidden="true">→</span></Link></Card.Footer></Card>)}</div>
          <div className="overview-note"><Icon path="M12 3 4 6v6c0 5 8 9 8 9s8-4 8-9V6z M8 12l3 3 5-6" /><p>以服务端实际状态为准。连接可用不代表文件同步完成，备份下载完成后仍需重读验证。</p></div>
        </> : path === "/roots" ? <RootsPanel /> : path === "/recovery" ? <RecoveryPanel /> : path === "/backup" ? <BackupPanel /> : path === "/account" ? <AccountPanel /> : <EmptyState title="页面不存在">请从导航选择页面。</EmptyState>}</Suspense></PageBoundary></div>
        <footer className="page-footer"><span>vaultsyncd</span><span>文件同步与恢复 · 服务管理</span></footer>
      </main>
    </div>
  </div>;
}
