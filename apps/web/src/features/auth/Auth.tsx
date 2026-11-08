import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import type { AccountInfo } from "@vaultsyncd/api-client";
import { Button, Grid, Paper, PasswordInput, Stack, Text, TextInput, Title, Group } from "@mantine/core";
import { useForm } from "@mantine/form";
import { StatusBadge } from "../../components/ui";
import { Feedback, showFeedback } from "../../components/Feedback";
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
  return <AuthContext.Provider value={{ account, client: client.current, busy, error, reportSignedOut: () => setAccount(null), run }}><Feedback message={error} />{children}</AuthContext.Provider>;
}
export function useAuth(): AuthState {
  const auth = useContext(AuthContext);
  if (!auth) throw new Error("账号上下文未装配");
  return auth;
}
export function AccountPanel() {
  const { account, busy, run } = useAuth();
  const loginForm = useForm({ mode: "uncontrolled", initialValues: { login: "", password: "" } });
  const passwordForm = useForm({ mode: "uncontrolled", initialValues: { current_password: "", new_password: "" } });
  async function submitLogin(values: typeof loginForm.values) {
    loginForm.reset();
    await run(async (client) => { const account = await client.login(values.login, values.password); showFeedback("登录成功"); return account; });
  }
  async function submitPassword(values: typeof passwordForm.values) {
    passwordForm.reset();
    await run(async (client) => { await client.changePassword(values.current_password, values.new_password); showFeedback("密码已修改，全部会话已撤销。请使用新密码重新登录。"); return null; });
  }
  return <Grid align="flex-start">
    <Grid.Col span={{ base: 12, lg: 8 }}>
      <Stack>
        <Paper withBorder p="lg">
          <Stack>
            <Group justify="space-between">
              <Title order={2} size="h4">账号与会话</Title>
              <StatusBadge tone={account ? "success" : "neutral"}>{busy ? "核验中" : account ? "会话有效" : "等待登录"}</StatusBadge>
            </Group>
            <Text size="sm" c="dimmed">使用 owner 账号管理当前服务，凭据通过同源安全会话传递。</Text>
            <Text role="status" fw={500}>{busy ? "正在核验账号…" : account ? `已登录：${account.login}` : "未登录"}</Text>
            {account ? <>
              <Text size="sm" c="dimmed">凭据由浏览器的安全 Cookie 携带，页面关闭不会撤销其他设备。</Text>
              <Group>
                <Button variant="default" disabled={busy} onClick={() => void run(async (client) => { await client.logout(); showFeedback("已退出当前会话"); return null; })}>退出当前会话</Button>
                <Button variant="light" color="red" disabled={busy} onClick={() => void run(async (client) => { await client.revokeAll(); showFeedback("全部会话已撤销，请重新登录。"); return null; })}>撤销全部会话</Button>
              </Group>
            </> : <form onSubmit={loginForm.onSubmit(submitLogin)}>
              <Stack maw={480}>
                <TextInput name="login" label="登录名" placeholder="输入 owner 登录名" required disabled={busy} autoComplete="username" maxLength={64}
                  key={loginForm.key("login")} {...loginForm.getInputProps("login")} />
                <PasswordInput name="password" label="密码" placeholder="输入账号密码" required disabled={busy} autoComplete="current-password" maxLength={1024}
                  key={loginForm.key("password")} {...loginForm.getInputProps("password")} />
                <Group><Button disabled={busy} type="submit">登录</Button></Group>
              </Stack>
            </form>}
          </Stack>
        </Paper>
        {account && <Paper withBorder p="lg">
          <Stack>
            <Title order={2} size="h4">修改密码</Title>
            <Text size="sm" c="dimmed">更新后，包含当前浏览器在内的全部会话将失效。</Text>
            <form onSubmit={passwordForm.onSubmit(submitPassword)}>
              <Stack maw={480}>
                <PasswordInput name="current_password" label="当前密码" required disabled={busy} autoComplete="current-password" maxLength={1024}
                  key={passwordForm.key("current_password")} {...passwordForm.getInputProps("current_password")} />
                <PasswordInput name="new_password" label="新密码" description="至少 12 个字符，请使用独立密码。" required disabled={busy} autoComplete="new-password" minLength={12} maxLength={1024}
                  key={passwordForm.key("new_password")} {...passwordForm.getInputProps("new_password")} />
                <Group><Button disabled={busy} type="submit">修改密码并撤销会话</Button></Group>
              </Stack>
            </form>
          </Stack>
        </Paper>}
      </Stack>
    </Grid.Col>
    <Grid.Col span={{ base: 12, lg: 4 }}>
      <Paper withBorder p="lg">
        <Stack>
          <Title order={2} size="h4">会话说明</Title>
          <div><Text fw={500}>当前浏览器</Text><Text size="sm" c="dimmed">退出当前会话只影响本次登录。管理完成后，可在共用设备上主动退出。</Text></div>
          <div><Text fw={500}>全部设备</Text><Text size="sm" c="dimmed">撤销全部会话会要求所有已登录设备重新认证，文件内容仍保留。</Text></div>
          <div><Text fw={500}>首次使用</Text><Text size="sm" c="dimmed">owner 由服务器管理员初始化；请使用管理员提供的账号登录。</Text></div>
        </Stack>
      </Paper>
    </Grid.Col>
  </Grid>;
}
