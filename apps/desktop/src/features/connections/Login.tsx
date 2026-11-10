import { useState } from "react";
import { Alert, Button, Modal, PasswordInput, Stack, Text, TextInput } from "@mantine/core";
import type { ConnectionInfo } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";
import { ConnectionForm } from "./Connections";

type LoginProps = {
  agent: AgentClient; connection?: ConnectionInfo; changed(): Promise<void>;
  run(action: () => Promise<void>): void; busy: boolean; error: string; retry(): void;
};
export function Login(props: LoginProps) {
  return <Modal opened onClose={() => {}} title="登录" centered size="sm" withCloseButton={false} closeOnEscape={false} closeOnClickOutside={false}>
    <Stack>
      {props.error && <Alert color="red" role="alert">{props.error}</Alert>}
      {props.connection ? <LoginForm {...props} connection={props.connection} /> : <>
        <Text role="status">{props.error ? "无法读取登录状态。" : "正在读取登录状态…"}</Text>
        {props.error && <Button disabled={props.busy} onClick={props.retry}>重试</Button>}
      </>}
    </Stack>
  </Modal>;
}
function LoginForm({ agent, connection, changed, run, busy }: LoginProps & { connection: ConnectionInfo }) {
  const [username, setUsername] = useState(connection.username ?? "");
  const [password, setPassword] = useState("");
  async function authenticate() {
    const secret = password;
    setPassword("");
    const result = await agent.login(username, secret);
    if (!result.connection?.authenticated) throw new Error("登录未确认，请重新登录。");
  }
  return <ConnectionForm agent={agent} connection={connection} changed={changed} run={run} busy={busy} authenticate={authenticate}>
    <TextInput label="用户名" name="username" autoComplete="username" required value={username} disabled={busy} onChange={event => setUsername(event.currentTarget.value)} />
    <PasswordInput label="密码" name="password" autoComplete="current-password" required value={password} disabled={busy} onChange={event => setPassword(event.currentTarget.value)} />
  </ConnectionForm>;
}
