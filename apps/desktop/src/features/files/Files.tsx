import { Download } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { ActionIcon, Card, Group, NativeSelect, SimpleGrid, Stack, Text, TextInput, Title } from "@mantine/core";
import { Button, Dialog, StatusBadge, Table } from "@vaultsyncd/ui";
import type { BindingInfo, ConnectionInfo, FileStatus } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";

const labels: Record<string, string> = { unavailable: "不可用", failed: "失败", conflict: "冲突", local_changes: "待上传本地修改", syncing: "正在同步", downloaded: "已下载", cloud_only: "仅云端" };
export function Files({ agent, bindings = [], connections, run, busy, changed }: { agent: AgentClient; bindings?: BindingInfo[]; connections: ConnectionInfo[]; run(action: () => Promise<void>): void; busy: boolean; changed(): Promise<void> }) {
  const [binding, select] = useState(""); const [rows, setRows] = useState<FileStatus[]>();
  const [cursor, setCursor] = useState<string>(); const [next, setNext] = useState<string>();
  const [message, setMessage] = useState("请选择绑定后读取实际状态。"); const [fresh, setFresh] = useState(false);
  const [selected, setSelected] = useState<FileStatus>(); const [search, setSearch] = useState("");
  const sequence = useRef(0); const reading = useRef(false); const operations = useRef(new Map<string, string>());
  const current = bindings.find(b => b.binding_id === binding);
  const authenticated = connections.some(server => server.server_id === current?.server_id && server.authenticated);
  const load = useCallback(async (propagateError = false) => {
    if (!binding || reading.current) return; reading.current = true; const version = sequence.current;
    try {
      const result = await agent.listFiles(binding, cursor); if (!result.files) throw new Error("agent 未返回文件页。");
      if (version === sequence.current) { setRows(result.files); setNext(result.next_cursor); setFresh(true); setMessage("当前页实际状态；执行操作时后端仍会重新核对。"); }
    } catch (error) {
      if (version === sequence.current) { setFresh(false); setMessage(error instanceof Error ? error.message : "文件状态读取失败。"); }
      if (propagateError) throw error;
    } finally { reading.current = false; }
  }, [agent, binding, cursor]);
  useEffect(() => { sequence.current++; setRows(undefined); setFresh(false); void load(); }, [load]);
  useEffect(() => { if (busy) return; const timer = window.setInterval(() => void load(), 2000); return () => window.clearInterval(timer); }, [load, busy]);
  async function action(row: FileStatus, kind: "download" | "release") {
    if (!row.entry_id) throw new Error("文件尚未确认身份。"); const key = `${binding}:${row.entry_id}:${kind}`;
    let op = operations.current.get(key); if (!op) { op = crypto.randomUUID().replaceAll("-", ""); operations.current.set(key, op); }
    await (kind === "download" ? agent.downloadEntry(binding, row.entry_id, op) : agent.releaseEntry(binding, row.entry_id, op));
    operations.current.delete(key); await load(); await changed();
  }
  return <Stack gap="lg">
    <Title order={1} size="h2">文件状态与操作</Title>
    <Card withBorder>
      <Stack>
        <SimpleGrid cols={{ base: 1, sm: 2 }}>
          <NativeSelect label="绑定" name="files_binding" value={binding} onChange={event => { select(event.target.value); setCursor(undefined); setSelected(undefined); }}>
            <option value="">请选择绑定</option>{bindings.map(b => <option key={b.binding_id} value={b.binding_id}>{b.local_path}{b.detached ? "（已解除）" : ""}</option>)}
          </NativeSelect>
          <TextInput label="搜索当前页" name="file_search" value={search} onChange={event => setSearch(event.target.value)} />
        </SimpleGrid>
        <Group justify="space-between">
          <Text size="sm" role="status">{message}</Text>
          <Button variant="default" disabled={busy || !binding} onClick={() => run(() => load(true))}>刷新文件状态</Button>
        </Group>
      </Stack>
    </Card>
    <Table<FileStatus> caption="文件当前页" rows={(rows ?? []).filter(row => row.path.toLowerCase().includes(search.toLowerCase()))} rowKey={row => row.path} emptyMessage={rows ? "当前页没有匹配文件。" : "尚未读取文件。"} columns={[
      { key: "path", title: "路径", render: row => row.path },
      { key: "state", title: "状态", render: row => <Stack gap={4}>
        <Group><StatusBadge tone={row.status === "failed" || row.status === "conflict" ? "danger" : row.pending || row.dirty || row.busy || row.status === "unavailable" ? "warning" : "neutral"}>{labels[row.status] ?? "未知状态"}</StatusBadge></Group>
        {(row.pending || row.busy || row.availability === "offline" || row.remote_apply !== "current" || row.synchronized && !row.pending && !row.dirty) && <Text size="sm">
          {[row.pending && "待确认/未完成", row.busy && "正在使用或保护未知", row.availability === "offline" && "当前离线", row.remote_apply !== "current" && "远端/基线仍需核对", row.synchronized && !row.pending && !row.dirty && "基线已确认"].filter(Boolean).join(" · ")}
        </Text>}
      </Stack> },
      { key: "actions", title: "操作", render: row => <Stack gap="xs">
        <Group gap="xs">
          <ActionIcon variant="filled" size="md" aria-label="下载" title="下载" disabled={busy || !authenticated || !fresh || !row.entry_id || row.dirty || current?.paused || current?.detached} onClick={() => run(() => action(row, "download"))}><Download aria-hidden="true" size={18} strokeWidth={1.6} /></ActionIcon>
          <Button size="xs" variant="default" disabled={busy || !fresh || !row.release_allowed || current?.detached} onClick={() => setSelected(row)}>释放空间</Button>
        </Group>
        {row.release_reason && <Text size="xs" c="dimmed">{row.release_reason}</Text>}
      </Stack> },
    ]} />
    <Group justify="flex-end">
      <Button variant="default" disabled={busy || !cursor} onClick={() => setCursor(undefined)}>回到第一页</Button>
      <Button variant="default" disabled={busy || !next} onClick={() => setCursor(next)}>下一页</Button>
    </Group>
    <Dialog opened={selected !== undefined} title="确认释放本地内容" onClose={() => setSelected(undefined)}>
      <Text>{selected?.path} 的本地内容将移除，远端数据保留；离线将不可读取。未确认或忙文件不会释放。</Text>
      <Group><Button color="red" disabled={busy || !selected} onClick={() => {
        if (!selected) return;
        const row = selected; setSelected(undefined); run(() => action(row, "release"));
      }}>确认释放</Button></Group>
    </Dialog>
  </Stack>;
}
