import { useState, type FormEvent } from "react";
import { Group, PasswordInput, Stack, TextInput } from "@mantine/core";
import { Button } from "@vaultsyncd/ui";
import type { ConnectionInfo } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";

export function Login({ agent, connection, run, busy, onSaved, onCancel }: {
  agent: AgentClient; connection: ConnectionInfo; run(action: () => Promise<void>): void;
  busy: boolean; onSaved(): Promise<void>; onCancel(): void;
}) {
  const [password, setPassword] = useState("");

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (busy) return;
    const username = new FormData(event.currentTarget).get("username");
    if (typeof username !== "string" || !username || !password) return;
    const secret = password;
    setPassword("");
    run(async () => {
      const result = await agent.login(connection.server_id, username, secret);
      if (result.connection?.server_id !== connection.server_id || !result.connection.authenticated) {
        throw new Error("登录未确认，请重新登录。");
      }
      await onSaved();
    });
  }

  return <form onSubmit={submit}><Stack>
    <TextInput label="用户名" name="username" autoComplete="username" required defaultValue={connection.username} disabled={busy} />
    <PasswordInput label="密码" name="password" autoComplete="current-password" required value={password}
      disabled={busy} onChange={event => setPassword(event.currentTarget.value)} />
    <Group>
      <Button type="submit" disabled={busy}>登录</Button>
      <Button variant="default" disabled={busy} onClick={onCancel}>取消</Button>
    </Group>
  </Stack></form>;
}
