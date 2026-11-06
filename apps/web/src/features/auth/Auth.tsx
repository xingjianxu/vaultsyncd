import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import type { AccountInfo } from "@vaultsyncd/api-client";
import { Button } from "@vaultsyncd/ui";
import { BrowserAuth, SignedOut } from "../../services/auth";

type AuthState = { account: AccountInfo | null; client: BrowserAuth | null; busy: boolean; error: string; reportSignedOut: () => void; run: (action: (client: BrowserAuth) => Promise<AccountInfo | null>) => Promise<void> };
const AuthContext = createContext<AuthState | null>(null);
export function AuthProvider({ children }: { children: ReactNode }) {
  const client = useRef<BrowserAuth | null>(null);
  const [account, setAccount] = useState<AccountInfo | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  async function run(action: (client: BrowserAuth) => Promise<AccountInfo | null>) {
    if (!client.current || busy) return;
    setBusy(true); setError("");
    try { setAccount(await action(client.current)); }
    catch (cause) {
      if (cause instanceof SignedOut) setAccount(null);
      setError(cause instanceof Error ? cause.message : "请求失败，请检查服务连接。");
    } finally { setBusy(false); }
  }
  useEffect(() => {
    const current = client.current ?? new BrowserAuth(); client.current = current;
    let active = true;
    setBusy(true);
    current.account().then((result) => { if (active) setAccount(result); }).catch((cause) => {
      if (active && !(cause instanceof SignedOut)) setError("无法核验会话，请检查服务连接后重试。");
    }).finally(() => { if (active) setBusy(false); });
    return () => { active = false; };
  }, []);
  return <AuthContext.Provider value={{ account, client: client.current, busy, error, reportSignedOut: () => setAccount(null), run }}>{children}</AuthContext.Provider>;
}
export function useAuth(): AuthState {
  const auth = useContext(AuthContext);
  if (!auth) throw new Error("账号上下文未装配");
  return auth;
}
export function AccountPanel() {
  const { account, busy, error, run } = useAuth();
  const [message, setMessage] = useState("");
  async function submitLogin(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault(); setMessage("");
    const form = event.currentTarget; const fields = new FormData(form);
    const login = String(fields.get("login")); const password = String(fields.get("password"));
    form.reset();
    await run((client) => client.login(login, password));
  }
  async function submitPassword(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault(); setMessage("");
    const form = event.currentTarget; const fields = new FormData(form);
    const current = String(fields.get("current_password")); const next = String(fields.get("new_password"));
    form.reset();
    await run(async (client) => { await client.changePassword(current, next); setMessage("密码已修改，全部会话已撤销。请使用新密码重新登录。"); return null; });
  }
  return <section className="vs-card"><h2>账号与会话</h2>
    <p role="status">{busy ? "正在核验账号…" : account ? `已登录：${account.login}` : "未登录"}</p>
    {error && <p role="alert">{error}</p>}{message && <p role="status">{message}</p>}
    {account ? <><p>凭据由浏览器的安全 Cookie 携带，页面关闭不会撤销其他设备。</p>
      <Button disabled={busy} onClick={() => void run(async (client) => { await client.logout(); return null; })}>退出当前会话</Button>{" "}
      <Button disabled={busy} onClick={() => void run(async (client) => { await client.revokeAll(); setMessage("全部会话已撤销，请重新登录。"); return null; })}>撤销全部会话</Button>
      <form onSubmit={(event) => void submitPassword(event)}><h3>修改密码</h3>
        <label>当前密码<input name="current_password" type="password" autoComplete="current-password" required maxLength={1024} /></label>{" "}
        <label>新密码<input name="new_password" type="password" autoComplete="new-password" required minLength={12} maxLength={1024} /></label>{" "}
        <Button disabled={busy} type="submit">修改密码并撤销会话</Button>
      </form></> : <form onSubmit={(event) => void submitLogin(event)}>
      <label>登录名<input name="login" autoComplete="username" required maxLength={64} /></label>{" "}
      <label>密码<input name="password" type="password" autoComplete="current-password" required maxLength={1024} /></label>{" "}
      <Button disabled={busy} type="submit">登录</Button>
    </form>}
  </section>;
}
