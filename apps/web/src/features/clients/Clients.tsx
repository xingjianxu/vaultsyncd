import { useEffect, useRef, useState } from "react";
import { Alert, Badge, Button, Group, Loader, ScrollArea, Stack, Table, Text } from "@mantine/core";
import { listClients, type ClientConnection, type ClientConnectionPage } from "@vaultsyncd/api-client";
import { useAuth } from "../auth/Auth";
import { showFeedback } from "../../components/Feedback";

const states = { recent: "最近活动", inactive: "未活跃", revoked: "已撤销", expired: "已过期", unknown: "会话已清理" };
const kinds = { client: "客户端", browser: "浏览器", unknown: "未知" };
function time(value?: string) {
  if (value === undefined) return "未知";
  const date = new Date(Number(value) * 1000);
  return Number.isNaN(date.getTime()) ? "未知" : date.toLocaleString("zh-CN");
}

export function ClientsPanel() {
  const { client } = useAuth();
  const [page, setPage] = useState<ClientConnectionPage>();
  const [rows, setRows] = useState<ClientConnection[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const controller = useRef<AbortController | null>(null);
  async function load(after?: string) {
    if (!client || controller.current) return;
    const current = new AbortController(); controller.current = current;
    setBusy(true); setError("");
    try {
      const result = await client.request(options => listClients(options, after ? { after } : undefined, { signal: current.signal }));
      if (current.signal.aborted) return;
      setRows(previous => after ? [...previous, ...result.connections.filter(row => !previous.some(old => old.connection_id === row.connection_id))] : result.connections);
      setPage(result);
    } catch (cause) {
      if (current.signal.aborted) return;
      const message = cause instanceof Error ? cause.message : "读取客户端连接失败";
      setError(message); showFeedback(message, "red");
    } finally {
      if (controller.current === current) { controller.current = null; setBusy(false); }
    }
  }
  useEffect(() => { void load(); return () => { controller.current?.abort(); controller.current = null; }; }, [client]);
  return <Stack>
    <Group justify="space-between"><Text size="sm" c="dimmed">{page ? `更新于 ${time(page.observed_at)}` : "客户端连接记录"}</Text><Button onClick={() => void load()} disabled={busy}>刷新</Button></Group>
    <Text size="sm" c="dimmed">最近活动指五分钟内的认证请求，不代表实时在线或同步完成。</Text>
    {error && <Alert color="red" title="读取失败"><Stack gap="xs"><Text size="sm">{error}</Text><Button variant="light" disabled={busy} onClick={() => void load()}>重试</Button></Stack></Alert>}
    {busy && <Group role="status"><Loader size="sm" /><Text size="sm">正在读取连接记录…</Text></Group>}
    {page && rows.length === 0 && !error && <Text>暂无客户端连接记录。</Text>}
    {rows.length > 0 && <ScrollArea><Table miw={920} aria-label="客户端连接记录" striped highlightOnHover>
      <Table.Thead><Table.Tr>{["设备", "类型", "状态", "登录时间", "最近活动", "会话到期"].map(label => <Table.Th key={label}>{label}</Table.Th>)}</Table.Tr></Table.Thead>
      <Table.Tbody>{rows.map(row => <Table.Tr key={row.connection_id}>
        <Table.Td><Stack gap={2}><Group gap="xs"><Text size="sm" style={{ overflowWrap: "anywhere", maxWidth: 240 }}>{row.device_label}</Text>{row.current && <Badge size="xs" variant="light">当前连接</Badge>}</Group><Text size="xs" c="dimmed" title="连接身份">{row.connection_id}</Text></Stack></Table.Td>
        <Table.Td>{kinds[row.client_kind]}</Table.Td><Table.Td><Badge variant="light" color={row.state === "recent" ? "green" : "gray"}>{states[row.state]}</Badge></Table.Td>
        <Table.Td>{time(row.created_at)}</Table.Td><Table.Td>{time(row.last_seen_at)}</Table.Td><Table.Td>{time(row.refresh_expires_at)}</Table.Td>
      </Table.Tr>)}</Table.Tbody>
    </Table></ScrollArea>}
    {page?.next_cursor && <Group justify="center"><Button variant="default" disabled={busy} onClick={() => void load(page.next_cursor)}>加载更多</Button></Group>}
  </Stack>;
}
