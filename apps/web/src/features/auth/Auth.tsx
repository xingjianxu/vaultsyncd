import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import type { AccountInfo } from "@vaultsyncd/api-client";
import { Alert, Button, Grid, Loader, Paper, PasswordInput, Stack, Text, TextInput, Title, Group } from "@mantine/core";
import { useForm } from "@mantine/form";
import { StatusBadge } from "../../components/ui";
import { Feedback, showFeedback } from "../../components/Feedback";
import { BrowserAuth, SignedOut } from "../../services/auth";
import logoUrl from "../../../../../logo.svg?no-inline";

type AuthState = { account: AccountInfo | null; client: BrowserAuth | null; initializing: boolean; busy: boolean; error: string; verify: () => Promise<void>; reportSignedOut: () => void; run: (action: (client: BrowserAuth) => Promise<AccountInfo | null>) => Promise<void> };
const AuthContext = createContext<AuthState | null>(null);
export function AuthProvider({ children }: { children: ReactNode }) {
  const client = useRef<BrowserAuth | null>(null);
  const mounted = useRef(false);
  const running = useRef(false);
  const operation = useRef(0);
  const firstCheck = useRef(true);
  const [account, setAccount] = useState<AccountInfo | null>(null);
  const [initializing, setInitializing] = useState(true);
  const [busy, setBusy] = useState(true);
  const [error, setError] = useState("");
  const reportSignedOut = useCallback((cause?: SignedOut) => {
    if (!mounted.current) return;
    operation.current++;
    running.current = false;
    setAccount(null); setBusy(false); setInitializing(false);
    setError(firstCheck.current ? "" : cause?.message ?? "");
    firstCheck.current = false;
  }, []);
  const run = useCallback(async (action: (client: BrowserAuth) => Promise<AccountInfo | null>) => {
    const current = client.current;
    if (!current || running.current || !mounted.current) return;
    const id = ++operation.current;
    running.current = true;
    setBusy(true); setError("");
    try {
      const pending = action(current);
      const generation = current.sessionGeneration;
      const result = await pending;
      if (mounted.current && operation.current === id && current.sessionGeneration === generation) setAccount(result);
    } catch (cause) {
      if (mounted.current && operation.current === id) {
        if (cause instanceof SignedOut) setAccount(null);
        setError(cause instanceof Error ? cause.message : "请求失败，请检查服务连接。");
      }
    } finally {
      if (mounted.current && operation.current === id) {
        running.current = false;
        setBusy(false); setInitializing(false);
        firstCheck.current = false;
      }
    }
  }, []);
  const verify = useCallback(() => run(current => current.account()), [run]);
  useEffect(() => {
    mounted.current = true;
    client.current ??= new BrowserAuth(undefined, reportSignedOut);
    void verify();
    return () => { mounted.current = false; operation.current++; running.current = false; };
  }, [reportSignedOut, verify]);
  // 页面消费者可能仍握有旧闭包；旧请求不能注销后来建立的新会话。
  const generation = client.current?.sessionGeneration;
  return <AuthContext.Provider value={{ account, client: client.current, initializing, busy, error, verify, reportSignedOut: () => {
    if (client.current?.sessionGeneration === generation) reportSignedOut(new SignedOut());
  }, run }}><Feedback message={error} />{children}</AuthContext.Provider>;
}
export function useAuth(): AuthState {
  const auth = useContext(AuthContext);
  if (!auth) throw new Error("账号上下文未装配");
  return auth;
}
/** 登录前不挂载管理页面，也不改写 hash 深链。 */
export function AuthGate({ children }: { children: ReactNode }) {
  const { account, initializing } = useAuth();
  if (initializing) return <main className="auth-screen"><Stack align="center" role="status" aria-live="polite"><Loader size="sm" /><Text>正在核验会话…</Text></Stack></main>;
  if (!account) return <LoginPage />;
  return children;
}
function LoginPage() {
  const { busy, error, run, verify } = useAuth();
  const form = useForm({ mode: "uncontrolled", initialValues: { login: "", password: "" } });
  async function submitLogin({ login, password }: typeof form.values) {
    form.setFieldValue("password", "");
    await run(async client => { const account = await client.login(login, password); showFeedback("登录成功"); return account; });
  }
  return <main className="auth-screen">
    <Paper withBorder p="xl" radius="md" className="auth-panel">
      <Stack gap="lg">
        <Group gap="sm"><img src={logoUrl} alt="" width={40} height={40} /><Title order={1} size="h2">登录 vaultsyncd</Title></Group>
        {error && <Alert color="red" role="alert">{error}</Alert>}
        <form onSubmit={form.onSubmit(submitLogin)} aria-busy={busy}>
          <Stack>
            <TextInput name="login" label="登录名" required disabled={busy} autoComplete="username" autoCapitalize="none" spellCheck={false} maxLength={64}
              key={form.key("login")} {...form.getInputProps("login")} />
            <PasswordInput name="password" label="密码" required disabled={busy} autoComplete="current-password" maxLength={1024}
              key={form.key("password")} {...form.getInputProps("password")} />
            <Button type="submit" loading={busy} disabled={busy} fullWidth>登录</Button>
          </Stack>
        </form>
        {error && <Button variant="subtle" disabled={busy} onClick={() => void verify()}>重新核验会话</Button>}
      </Stack>
    </Paper>
  </main>;
}
export function AccountPanel() {
  const { account, busy, run } = useAuth();
  const passwordForm = useForm({ mode: "uncontrolled", initialValues: { current_password: "", new_password: "" } });
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
            {account && <>
              <Text size="sm" c="dimmed">凭据由浏览器的安全 Cookie 携带，页面关闭不会撤销其他设备。</Text>
              <Group>
                <Button variant="default" disabled={busy} onClick={() => void run(async (client) => { await client.logout(); showFeedback("已退出当前会话"); return null; })}>退出当前会话</Button>
                <Button variant="light" color="red" disabled={busy} onClick={() => void run(async (client) => { await client.revokeAll(); showFeedback("全部会话已撤销，请重新登录。"); return null; })}>撤销全部会话</Button>
              </Group>
            </>}
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
                <PasswordInput name="new_password" label="新密码" description="请使用独立密码。" required disabled={busy} autoComplete="new-password" minLength={1} maxLength={1024}
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
