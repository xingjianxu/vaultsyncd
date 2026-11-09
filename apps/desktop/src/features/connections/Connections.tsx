import { useState, type FormEvent } from "react";
import { Card, Group, PasswordInput, SimpleGrid, Stack, Text, TextInput, Title } from "@mantine/core";
import { Button } from "@vaultsyncd/ui";
import type { ConnectionInfo } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";

export function Connections({ agent, connection, changed, run, busy }: {
  agent: AgentClient; connection?: ConnectionInfo; changed(): Promise<void>;
  run(action: () => Promise<void>): void; busy: boolean;
}) {
  const [password, setPassword] = useState("");
  function configure(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); const form = new FormData(event.currentTarget);
    run(async () => {
      await agent.setConnection({ serverId: String(form.get("server_id")), serverUrl: String(form.get("server_url")), caFile: String(form.get("ca_file") || "") || undefined });
      await changed();
    });
  }
  function login(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); const form = new FormData(event.currentTarget);
    const username = String(form.get("username")); const secret = password;
    setPassword("");
    run(async () => { await agent.login(username, secret); await changed(); });
  }
  return <Stack gap="lg">
    <Title order={1} size="h2">连接设置</Title>
    <SimpleGrid cols={{ base: 1, lg: 2 }}>
      <Card component="section" withBorder>
        <Stack>
          <Title order={2} size="h3">服务器</Title>
          <form onSubmit={configure} key={`${connection?.server_id}:${connection?.server_url}`}><Stack>
            <TextInput label="服务器地址" required name="server_url" type="url" defaultValue={connection?.server_url} placeholder="https://sync.example.com" />
            <TextInput label="服务器身份（管理员提供）" required name="server_id" pattern="[0-9a-f]{32}" defaultValue={connection?.server_id} />
            <details>
              <Text component="summary" size="sm">开发证书</Text>
              <Stack gap="xs" mt="sm">
                <TextInput label="附加 CA 文件（本机绝对路径）" name="ca_file" defaultValue={connection?.ca_file} />
                <Text size="sm" c="dimmed">仅添加明确选择的 CA，始终校验证书和服务器名称。</Text>
              </Stack>
            </details>
            <Group><Button type="submit" disabled={busy}>保存连接</Button></Group>
          </Stack></form>
        </Stack>
      </Card>
      <Card component="section" withBorder>
        <Stack>
          <Title order={2} size="h3">账号</Title>
          <Text size="sm">当前账号：{connection ? connection.authenticated ? "已登录（持有凭据，连接仍需核验）" : "未登录" : "尚未读取"}</Text>
          <form onSubmit={login} autoComplete="off"><Stack>
            <TextInput label="账号" required name="username" autoComplete="username" />
            <PasswordInput label="密码" required name="password" autoComplete="current-password" value={password} onChange={event => setPassword(event.target.value)} />
            <Group>
              <Button type="submit" disabled={busy}>登录</Button>
              <Button type="button" variant="default" disabled={busy || !connection?.authenticated} onClick={() => run(async () => { await agent.logout(); await changed(); })}>退出登录</Button>
            </Group>
          </Stack></form>
          <Text size="sm" c="dimmed">登录凭据由本机 agent 保存；关闭界面后同步继续。</Text>
        </Stack>
      </Card>
    </SimpleGrid>
  </Stack>;
}
