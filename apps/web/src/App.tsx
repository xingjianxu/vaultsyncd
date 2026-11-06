import { useEffect, useRef, useState } from "react";
import { Button, EmptyState, PageShell, StatusBadge } from "@vaultsyncd/ui";
import { checkServer } from "./services/server";
import { useRoute } from "./route";
import { RecoveryPanel } from "./features/history/Recovery";
import { RootsPanel } from "./features/roots/Roots";
import { AccountPanel, AuthProvider, useAuth } from "./features/auth/Auth";

const items = [{ path: "/overview", label: "服务概览" }, { path: "/roots", label: "Root 目录" }, { path: "/recovery", label: "历史与恢复" }, { path: "/account", label: "账号与会话" }];
export function App() { return <AuthProvider><AppView /></AuthProvider>; }
function AppView() {
  const auth = useAuth();
  const path = useRoute("/overview");
  const [status, setStatus] = useState("未连接");
  const [detail, setDetail] = useState("尚未检查服务连接。");
  const [busy, setBusy] = useState(false);
  const controller = useRef<AbortController | null>(null);
  useEffect(() => () => controller.current?.abort(), []);
  async function connect() {
    if (controller.current) return;
    const current = new AbortController();
    controller.current = current;
    setBusy(true); setStatus("正在检查");
    try {
      const result = await checkServer(undefined, current.signal);
      if (current.signal.aborted) return;
      setStatus(result.status === "ok" ? "服务可用" : result.status === "degraded" ? "服务降级" : "需要恢复");
      setDetail("健康检查已返回；账号与 Root 状态需登录后查看。");
    } catch (error) {
      if (current.signal.aborted) return;
      setStatus("未连接"); setDetail(error instanceof Error ? error.message : "连接失败，请检查服务地址。");
    } finally { if (!current.signal.aborted) { controller.current = null; setBusy(false); } }
  }
  return <PageShell product="服务管理" items={items} activePath={path}
    status={<StatusBadge tone={status === "未连接" ? "neutral" : "info"}>{auth.account && status === "未连接" ? "账号会话已连接" : status}</StatusBadge>}>
    {path === "/overview" ? <><div className="vs-eyebrow">SERVER / OVERVIEW</div><h1>服务概览</h1>
      <p>查看服务连接，再管理远端目录与文件恢复。</p><section className="vs-card">
        <h2>服务连接</h2><p aria-live="polite">{detail}</p>
        <Button disabled={busy} onClick={() => void connect()}>{busy ? "检查中…" : "检查服务连接"}</Button>
      </section></> : path === "/roots" ? <><h1>Root 目录</h1>
      <RootsPanel /></>
      : path === "/recovery" ? <><h1>历史与恢复</h1><RecoveryPanel /></> : path === "/account" ? <><h1>账号管理</h1><AccountPanel /></> : <EmptyState title="页面不存在">请从导航选择页面。</EmptyState>}
  </PageShell>;
}
