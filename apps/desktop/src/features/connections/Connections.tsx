import { useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import { Alert, Card, Group, SimpleGrid, Stack, Text, Textarea, TextInput, Title } from "@mantine/core";
import { Button } from "@vaultsyncd/ui";
import type { ConnectionInfo } from "@vaultsyncd/local-ipc-types";
import type { AgentClient, ConnectionInput } from "../../services/agent";

export function Connections({ agent, connection, changed, run, busy }: {
  agent: AgentClient; connection?: ConnectionInfo; changed(): Promise<void>;
  run(action: () => Promise<void>): void; busy: boolean;
}) {
  return <Stack gap="lg">
    <Title order={1} size="h2">连接设置</Title>
    <SimpleGrid cols={{ base: 1, lg: 2 }}>
      <Card component="section" withBorder>
        <Stack>
          <Title order={2} size="h3">服务器</Title>
          {connection && <ConnectionForm key={`${connection.server_id}:${connection.server_url}:${connection.ca_file ?? ""}`}
            agent={agent} connection={connection} changed={changed} run={run} busy={busy} />}
        </Stack>
      </Card>
      <Card component="section" withBorder>
        <Stack>
          <Title order={2} size="h3">账号</Title>
          <Text size="sm">当前账号：{connection ? connection.authenticated ? "已登录（持有凭据，连接仍需核验）" : "未登录" : "尚未读取"}</Text>
          {connection?.username && <Text size="sm">{connection.username}</Text>}
          <Group><Button type="button" variant="default" disabled={busy || !connection?.authenticated} onClick={() => run(async () => { await agent.logout(); await changed(); })}>退出登录</Button></Group>
          <Text size="sm" c="dimmed">登录凭据由本机 agent 保存；关闭界面后同步继续。</Text>
        </Stack>
      </Card>
    </SimpleGrid>
  </Stack>;
}

export function ConnectionForm({ agent, connection, changed, run, busy, children, authenticate }: {
  agent: AgentClient; connection: ConnectionInfo; changed(): Promise<void>;
  run(action: () => Promise<void>): void; busy: boolean; children?: ReactNode;
  authenticate?(): Promise<void>;
}) {
  const [serverUrl, setServerUrl] = useState(connection.server_url);
  const [caFile, setCaFile] = useState(connection.ca_file ?? "");
  const [candidate, setCandidate] = useState<ConnectionInput>();
  const [reading, setReading] = useState(false);
  const [error, setError] = useState("");
  const [retry, setRetry] = useState(0);
  const generation = useRef(0);
  function invalidate() { generation.current++; setCandidate(undefined); setError(""); setReading(false); }
  useEffect(() => {
    const current = ++generation.current;
    setCandidate(undefined); setError("");
    if (!serverUrl || !URL.canParse(serverUrl)) { setReading(false); return; }
    setReading(true);
    const input = { serverUrl, caFile: caFile || undefined };
    const timer = window.setTimeout(() => {
      void agent.discoverConnection(input).then(result => {
        if (generation.current !== current) return;
        const info = result.connection;
        if (!info || !/^[0-9a-f]{32}$/.test(info.server_id) || info.server_url !== input.serverUrl || info.ca_file !== input.caFile) {
          throw new Error("未取得有效服务器身份，请重新读取。");
        }
        setCandidate({ ...input, serverId: info.server_id });
      }).catch(cause => {
        if (generation.current === current) setError(cause instanceof Error ? cause.message : "无法读取 Server ID。");
      }).finally(() => { if (generation.current === current) setReading(false); });
    }, 500);
    return () => { generation.current++; window.clearTimeout(timer); };
  }, [agent, serverUrl, caFile, retry]);
  function configure(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!candidate || reading || busy) return;
    const confirmed = candidate;
    run(async () => {
      try { await agent.setConnection(confirmed); if (authenticate) await authenticate(); await changed(); }
      finally { invalidate(); if (authenticate) setRetry(value => value + 1); }
    });
  }
  return <form onSubmit={configure}><Stack>
    <TextInput label="服务器地址" required name="server_url" type="url" value={serverUrl}
      disabled={busy} onChange={event => { invalidate(); setServerUrl(event.currentTarget.value); }} placeholder="https://sync.example.com" />
    <details>
      <Text component="summary" size="sm">开发证书</Text>
      <Stack gap="xs" mt="sm">
        <TextInput label="附加 CA 文件（本机绝对路径）" name="ca_file" value={caFile} disabled={busy}
          onChange={event => { invalidate(); setCaFile(event.currentTarget.value); }} />
        <Text size="sm" c="dimmed">仅添加明确选择的 CA，始终校验证书和服务器名称。</Text>
      </Stack>
    </details>
    {reading && <Text size="sm" role="status">正在读取 Server ID…</Text>}
    {error && <Alert color="red" role="alert">{error}</Alert>}
    {candidate && <Textarea label="Server ID" name="server_id" readOnly autosize minRows={1} value={candidate.serverId} />}
    {children}
    <Group>
      <Button type="submit" disabled={busy || reading || !candidate}>{authenticate ? "登录" : "确认并保存连接"}</Button>
      <Button type="button" variant="default" disabled={busy || reading || !serverUrl} onClick={() => { invalidate(); setRetry(value => value + 1); }}>重新读取</Button>
      {candidate && <Button type="button" variant="subtle" disabled={busy} onClick={invalidate}>取消</Button>}
    </Group>
  </Stack></form>;
}
