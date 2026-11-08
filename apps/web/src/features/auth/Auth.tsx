import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import type { AccountInfo } from "@vaultsyncd/api-client";
import { Button } from "@heroui/react/button";
import { Card } from "@heroui/react/card";
import { Description } from "@heroui/react/description";
import { Input } from "@heroui/react/input";
import { Label } from "@heroui/react/label";
import { TextField } from "@heroui/react/textfield";
import { StatusBadge } from "../../components/ui";
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
  return <div className="grid items-start gap-4 xl:grid-cols-[minmax(0,1.5fr)_minmax(16rem,1fr)]">
    <div className="space-y-4">
      <Card className="p-4 sm:p-5">
        <Card.Header className="gap-3">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <Card.Title render={props => <h2 {...props} />}>账号与会话</Card.Title>
            <StatusBadge tone={account ? "success" : "neutral"}>{busy ? "核验中" : account ? "会话有效" : "等待登录"}</StatusBadge>
          </div>
          <Card.Description>使用 owner 账号管理当前服务，凭据通过同源安全会话传递。</Card.Description>
        </Card.Header>
        <Card.Content className="mt-3 space-y-3">
          <p role="status" className="font-medium">{busy ? "正在核验账号…" : account ? `已登录：${account.login}` : "未登录"}</p>
          {error && <p role="alert" className="rounded-md bg-danger-soft p-4 text-sm text-danger">{error}</p>}
          {message && <p role="status" className="rounded-md bg-success-soft p-4 text-sm text-success">{message}</p>}
          {account ? <>
            <p className="text-sm text-muted">凭据由浏览器的安全 Cookie 携带，页面关闭不会撤销其他设备。</p>
            <div className="flex flex-wrap gap-3">
              <Button size="sm" variant="secondary" isDisabled={busy} onPress={() => void run(async (client) => { await client.logout(); return null; })}>退出当前会话</Button>
              <Button size="sm" variant="danger-soft" isDisabled={busy} onPress={() => void run(async (client) => { await client.revokeAll(); setMessage("全部会话已撤销，请重新登录。"); return null; })}>撤销全部会话</Button>
            </div>
          </> : <form className="max-w-lg space-y-3" onSubmit={(event) => void submitLogin(event)}>
            <TextField name="login" isRequired isDisabled={busy} autoComplete="username" maxLength={64}>
              <Label>登录名</Label><Input className="w-full" placeholder="输入 owner 登录名" />
            </TextField>
            <TextField name="password" type="password" isRequired isDisabled={busy} autoComplete="current-password" maxLength={1024}>
              <Label>密码</Label><Input className="w-full" placeholder="输入账号密码" />
            </TextField>
            <Button size="sm" className="w-full sm:w-auto" isDisabled={busy} type="submit">登录</Button>
          </form>}
        </Card.Content>
      </Card>
      {account && <Card className="p-4 sm:p-5">
        <Card.Header><Card.Title render={props => <h2 {...props} />}>修改密码</Card.Title>
          <Card.Description>更新后，包含当前浏览器在内的全部会话将失效。</Card.Description>
        </Card.Header>
        <Card.Content className="mt-3">
          <form className="max-w-lg space-y-3" onSubmit={(event) => void submitPassword(event)}>
            <TextField name="current_password" type="password" isRequired isDisabled={busy} autoComplete="current-password" maxLength={1024}>
              <Label>当前密码</Label><Input className="w-full" />
            </TextField>
            <TextField name="new_password" type="password" isRequired isDisabled={busy} autoComplete="new-password" minLength={12} maxLength={1024}>
              <Label>新密码</Label><Input className="w-full" /><Description>至少 12 个字符，请使用独立密码。</Description>
            </TextField>
            <Button size="sm" isDisabled={busy} type="submit">修改密码并撤销会话</Button>
          </form>
        </Card.Content>
      </Card>}
    </div>
    <Card variant="secondary" className="p-4 sm:p-5">
      <Card.Header><Card.Title render={props => <h2 {...props} />}>会话说明</Card.Title></Card.Header>
      <Card.Content className="mt-3 space-y-3 text-sm text-muted">
        <div><h3 className="mb-1 font-medium text-foreground">当前浏览器</h3><p>退出当前会话只影响本次登录。管理完成后，可在共用设备上主动退出。</p></div>
        <div><h3 className="mb-1 font-medium text-foreground">全部设备</h3><p>撤销全部会话会要求所有已登录设备重新认证，文件内容仍保留。</p></div>
        <div><h3 className="mb-1 font-medium text-foreground">首次使用</h3><p>owner 由服务器管理员初始化；请使用管理员提供的账号登录。</p></div>
      </Card.Content>
    </Card>
  </div>;
}
