import { useEffect, useRef, useState, type FormEvent } from "react";
import { Alert, Button as LinkButton, Card, Group, Stack, Text, Textarea, TextInput, Title } from "@mantine/core";
import { notifications } from "@mantine/notifications";
import { Button, Dialog, StatusBadge } from "@vaultsyncd/ui";
import type { ConnectionInfo } from "@vaultsyncd/local-ipc-types";
import type { AgentClient, ConnectionInput } from "../../services/agent";
import { Login } from "./Login";

type Editor = { kind: "edit" | "login"; serverId: string };

export function Connections({ agent, connections, adding, read, changed, run, busy }: {
  agent: AgentClient; connections: ConnectionInfo[]; adding: boolean; read: boolean; changed(): Promise<void>;
  run(action: () => Promise<void>): void; busy: boolean;
}) {
  const [editor, setEditor] = useState<Editor>();
  const [removing, setRemoving] = useState<string>();
  const [confirmRemoval, setConfirmRemoval] = useState(false);
  const removeTarget = connections.find(connection => connection.server_id === removing);
  const cancel = () => setEditor(undefined);
  const closeAdd = () => { window.location.hash = "/connections"; };
  return <Stack gap="lg">
    <Group justify="space-between">
      <Title order={1} size="h2">连接设置</Title>
      {!adding && (busy || !read
        ? <Button disabled>添加服务器</Button>
        : <LinkButton component="a" href="#/connections/add" onClick={cancel}>添加服务器</LinkButton>)}
    </Group>
    {adding && <Card component="section" withBorder>
      <Stack>
        <Title order={2} size="h3">添加服务器</Title>
        <ConnectionForm agent={agent} run={run} busy={busy || !read} onCancel={closeAdd} onSave={async candidate => {
          if (connections.some(connection => connection.server_id === candidate.serverId)) {
            throw new Error("此服务器已添加，请在现有连接中修改地址或登录。");
          }
          await agent.setConnection(candidate);
          await changed();
          setEditor({ kind: "login", serverId: candidate.serverId });
          closeAdd();
        }} />
      </Stack>
    </Card>}
    {read && !connections.length && !adding && <Text c="dimmed">尚未添加服务器。</Text>}
    {connections.map(connection => <Card component="section" withBorder key={connection.server_id}>
      <Stack>
        <Group justify="space-between">
          <Title order={2} size="h3">{connection.server_url}</Title>
          <StatusBadge tone={connection.authenticated ? "success" : "neutral"}>{connection.authenticated ? "已登录" : "未登录"}</StatusBadge>
        </Group>
        {connection.username && <Text size="sm">{connection.username}</Text>}
        <Text size="sm" c="dimmed">Server ID {connection.server_id}</Text>
        {!adding && editor?.kind === "edit" && editor.serverId === connection.server_id
          ? <ConnectionForm agent={agent} connection={connection} run={run} busy={busy || !read} onCancel={cancel} onSave={async candidate => {
            await agent.setConnection(candidate);
            await changed();
            cancel();
          }} />
          : !adding && editor?.kind === "login" && editor.serverId === connection.server_id
            ? <Login agent={agent} connection={connection} run={run} busy={busy || !read} onCancel={cancel} onSaved={async () => {
              await changed();
              cancel();
            }} />
            : <Group>
              {connection.authenticated
                ? <Button variant="default" disabled={busy || !read || adding} onClick={() => run(async () => {
                  await agent.logout(connection.server_id);
                  await changed();
                })}>退出登录</Button>
                : <Button variant="default" disabled={busy || !read || adding} onClick={() => setEditor({ kind: "login", serverId: connection.server_id })}>登录</Button>}
              <Button variant="default" disabled={busy || !read || adding} onClick={() => setEditor({ kind: "edit", serverId: connection.server_id })}>修改地址</Button>
              <Button color="red" variant="light" disabled={busy || !read || adding} onClick={() => { setRemoving(connection.server_id); setConfirmRemoval(true); }}>移除服务器</Button>
            </Group>}
      </Stack>
    </Card>)}
    {/* Always mounted so Mantine can return focus to the card button after closing. */}
    <Dialog opened={confirmRemoval && !!removeTarget} title="确认移除服务器" onClose={() => setConfirmRemoval(false)}
      footer={removeTarget && <Group justify="flex-end">
        <Button data-autofocus variant="default" disabled={busy} onClick={() => setConfirmRemoval(false)}>取消</Button>
        <Button color="red" disabled={busy} onClick={() => {
          const serverId = removeTarget.server_id;
          setConfirmRemoval(false);
          run(async () => {
            const result = await agent.removeConnection(serverId);
            await changed();
            // The card is gone after this refresh; keep an unconfirmed revocation visible.
            if (result.session_revoked === false) notifications.show({ message: "已移除本地连接、登录资料与凭据；服务器未确认会话已撤销。", color: "yellow", autoClose: false, withCloseButton: true });
            else if (result.session_revoked !== true) throw new Error("agent 未返回移除结果。");
          });
        }}>移除服务器</Button>
      </Group>}>
      {removeTarget && <Text>移除后本机不再保留此服务器的地址、登录资料与凭据；不删除服务器文件或本地目录内容。仍有正在管理的绑定时不能移除，请先在「同步目录」解除绑定。</Text>}
    </Dialog>
  </Stack>;
}

function ConnectionForm({ agent, connection, onSave, onCancel, run, busy }: {
  agent: AgentClient; connection?: ConnectionInfo; onSave(input: ConnectionInput): Promise<void>;
  onCancel(): void; run(action: () => Promise<void>): void; busy: boolean;
}) {
  const [serverUrl, setServerUrl] = useState(connection?.server_url ?? "");
  const [caFile, setCaFile] = useState(connection?.ca_file ?? "");
  const [candidate, setCandidate] = useState<ConnectionInput>();
  const [reading, setReading] = useState(false);
  const [error, setError] = useState("");
  const [retry, setRetry] = useState(0);
  const generation = useRef(0);
  const expectedServerId = connection?.server_id;
  const mismatch = !!(candidate && expectedServerId && candidate.serverId !== expectedServerId);
  function invalidate() { generation.current++; setCandidate(undefined); setError(""); setReading(false); }
  useEffect(() => {
    const current = ++generation.current;
    if (!serverUrl || !URL.canParse(serverUrl)) return;
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

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!candidate || mismatch || reading || busy) return;
    run(() => onSave(candidate));
  }
  return <form onSubmit={submit}><Stack>
    <TextInput label="服务器地址" required name="server_url" type="url" value={serverUrl}
      disabled={busy} onChange={event => { invalidate(); setServerUrl(event.currentTarget.value); }} placeholder="https://sync.example.com" />
    <details>
      <Text component="summary" size="sm">开发证书</Text>
      <TextInput mt="sm" label="附加 CA 文件（本机绝对路径）" name="ca_file" value={caFile} disabled={busy}
        onChange={event => { invalidate(); setCaFile(event.currentTarget.value); }} />
    </details>
    {reading && <Text size="sm" role="status">正在读取 Server ID…</Text>}
    {error && <Alert color="red" role="alert">{error}</Alert>}
    {candidate && <Textarea label="Server ID" name="server_id" readOnly autosize minRows={1} value={candidate.serverId} />}
    {mismatch && <Alert color="red" role="alert">此地址返回了不同的 Server ID。请核对地址，或将其添加为另一台服务器。</Alert>}
    <Group>
      <Button type="submit" disabled={busy || reading || !candidate || mismatch}>确认并保存连接</Button>
      <Button variant="default" disabled={busy || reading || !serverUrl} onClick={() => { invalidate(); setRetry(value => value + 1); }}>重新读取</Button>
      <Button variant="subtle" disabled={busy} onClick={onCancel}>取消</Button>
    </Group>
  </Stack></form>;
}
