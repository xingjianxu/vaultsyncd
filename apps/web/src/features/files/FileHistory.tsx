import { useEffect, useState } from "react";
import { listEntryHistory, type RecoveryPage, type SnapshotEntry } from "@vaultsyncd/api-client";
import { Alert, Badge, Button, Group, Modal, Stack, Table, Text } from "@mantine/core";
import { useAuth } from "../auth/Auth";
import { SignedOut } from "../../services/auth";
import { fileSize } from "./browser";

/** 仅查询已提交版本；挂载身份绑定 Vault/Entry，关闭即取消在途请求。 */
export function FileHistory({ vaultId, entry, onClose }: { vaultId: string; entry: SnapshotEntry; onClose: () => void }) {
  const auth = useAuth();
  const client = auth.client!;
  const [data, setData] = useState<RecoveryPage | null>(null);
  const [cursors, setCursors] = useState<(string | undefined)[]>([undefined]);
  const [revision, setRevision] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const after = cursors[cursors.length - 1];
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true); setError(""); setData(null);
    client.request(options => listEntryHistory(options, vaultId, entry.entry_id, { after }, { signal: controller.signal }))
      .then(result => { if (!controller.signal.aborted) setData(result); })
      .catch(cause => {
        if (controller.signal.aborted) return;
        if (cause instanceof SignedOut) auth.reportSignedOut();
        setError(cause instanceof Error ? cause.message : "读取版本历史失败。");
      }).finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [client, vaultId, entry.entry_id, after, revision]);
  return <Modal opened onClose={onClose} title="版本历史" size="xl" centered>
    <Stack>
      <Group justify="space-between" align="start" wrap="nowrap">
        <Text fw={500} className="break-all">{entry.name}</Text>
        <Button variant="default" disabled={loading} onClick={() => { setCursors([undefined]); setRevision(value => value + 1); }}>刷新</Button>
      </Group>
      {loading && <Text role="status" size="sm">正在读取版本历史…</Text>}
      {error && <Alert color="red" role="alert"><Stack gap="xs"><Text size="sm">{error}</Text><Button variant="default" onClick={() => setRevision(value => value + 1)}>重试</Button></Stack></Alert>}
      {data && <Table.ScrollContainer minWidth={580}>
        <Table aria-label="文件版本历史" verticalSpacing="sm">
          <Table.Thead><Table.Tr><Table.Th>版本 / SHA-256</Table.Th><Table.Th>大小</Table.Th><Table.Th>保留至</Table.Th></Table.Tr></Table.Thead>
          <Table.Tbody>{data.items.length ? data.items.map(item => <Table.Tr key={item.id}>
            <Table.Td><Stack gap={4}>
              <Text component="code" size="sm" className="break-all">{item.version_id ?? item.id}</Text>
              <Text component="code" size="xs" c="dimmed" className="break-all">{item.hash ?? "—"}</Text>
            </Stack></Table.Td>
            <Table.Td><Text size="sm" style={{ whiteSpace: "nowrap" }}>{fileSize({ ...entry, size: item.size })}</Text></Table.Td>
            <Table.Td>{item.expires_at ? <Text size="sm">{new Date(Number(item.expires_at) * 1000).toLocaleString("zh-CN", { hour12: false })}</Text> : <Badge variant="light">当前版本</Badge>}</Table.Td>
          </Table.Tr>) : <Table.Tr><Table.Td colSpan={3}><Text ta="center" c="dimmed" py="md">暂无版本记录。</Text></Table.Td></Table.Tr>}</Table.Tbody>
        </Table>
      </Table.ScrollContainer>}
      <Group justify="space-between">
        <Group gap="xs">
          <Button variant="default" disabled={loading || cursors.length === 1} onClick={() => setCursors(value => value.slice(0, -1))}>上一页</Button>
          <Button variant="default" disabled={loading || !data?.next_cursor} onClick={() => { if (data?.next_cursor) setCursors(value => [...value, data.next_cursor]); }}>下一页</Button>
        </Group>
        <Button data-autofocus variant="default" onClick={onClose}>关闭</Button>
      </Group>
    </Stack>
  </Modal>;
}
