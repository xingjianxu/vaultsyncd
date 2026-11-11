import { useState } from "react";
import { Alert, Card, Group, NativeSelect, SimpleGrid, Stack, Text, Title } from "@mantine/core";
import { Button, Dialog, Table } from "@vaultsyncd/ui";
import type { BindingInfo, ConnectionInfo, FileStatus, RemoteItem } from "@vaultsyncd/local-ipc-types";
import type { AgentClient, RestoreInput } from "../../services/agent";

interface Job { input: RestoreInput; state: "unknown" | "pending" | "committed" | "rejected"; path?: string }
const key = (binding: string) => `vaultsyncd.desktop.restore.${binding}`;
export function History({ agent, bindings = [], connections, run, busy, changed }: { agent: AgentClient; bindings?: BindingInfo[]; connections: ConnectionInfo[]; run(action: () => Promise<void>): void; busy: boolean; changed(): Promise<void> }) {
  const [binding, setBinding] = useState(""); const [source, setSource] = useState<"history" | "trash">("history");
  const [files, setFiles] = useState<FileStatus[]>(); const [filesNext, setFilesNext] = useState<string>(); const [entry, setEntry] = useState("");
  const [items, setItems] = useState<RemoteItem[]>(); const [cursor, setCursor] = useState<string>(); const [next, setNext] = useState<string>();
  const [chosen, setChosen] = useState<RemoteItem>(); const [job, setJob] = useState<Job>(); const [message, setMessage] = useState("请选择绑定与历史或回收站。");
  const current = bindings.find(item => item.binding_id === binding);
  const server = connections.find(item => item.server_id === current?.server_id);
  const authenticated = server?.authenticated === true;
  function save(value: Job) { sessionStorage.setItem(key(value.input.bindingId), JSON.stringify(value)); setJob(value); }
  function selectBinding(value: string) {
    setBinding(value); setItems(undefined); setFiles(undefined); setFilesNext(undefined); setEntry(""); setChosen(undefined);
    setMessage("请选择来源并读取当前清单。"); setCursor(undefined); setNext(undefined); setJob(undefined);
    try { const text = sessionStorage.getItem(key(value)); if (text) { const saved = JSON.parse(text) as Job; if (saved.input?.bindingId === value) setJob(saved); } }
    catch { setMessage("恢复记录无法读取，请到问题页核对agent保留的原操作。"); }
  }
  async function load(after?: string) {
    const result = source === "history" ? await agent.listHistory(binding, entry, after) : await agent.listTrash(binding, after);
    if (!result.items) throw new Error("agent 未返回恢复清单。"); setItems(result.items); setNext(result.next_cursor); setCursor(after);
    setMessage("清单来自已授权服务器；到期为最早回收下限，恢复仍由服务器核验。");
  }
  async function restore(value: Job) {
    save(value);
    try {
      const result = await agent.restoreEntry(value.input); if (!result.operation_id) throw new Error("agent 未返回committed操作身份。");
      save({ ...value, state: "committed", path: result.restored_path });
      setMessage(result.restored_path ? `恢复已提交，实际位置：${result.restored_path}` : "恢复已提交，当前位置尚未取得；可用原操作再次核对。");
    } catch (error) {
      const text = error instanceof Error ? error.message : "恢复结果未确认";
      save({ ...value, state: text.includes("RESTORE_REJECTED") ? "rejected" : text.includes("PENDING") ? "pending" : "unknown" });
      const message = `${text}；内容与原操作保留，请核对原操作，勿另建恢复请求。`;
      setMessage(message);
      throw new Error(message);
    }
    try { await changed(); }
    catch (error) { throw new Error(`恢复已提交，但本机状态刷新失败：${error instanceof Error ? error.message : "请重新核对状态。"}`); }
  }
  return <Stack gap="lg">
    <Title order={1} size="h2">历史与回收站</Title>
    <Card withBorder>
      <Stack>
        <SimpleGrid cols={{ base: 1, sm: 2 }}>
          <NativeSelect label="绑定" name="history_binding" disabled={busy} value={binding} onChange={event => selectBinding(event.target.value)}>
            <option value="">请选择绑定</option>{bindings.map(b => <option key={b.binding_id} value={b.binding_id}>{b.local_path}</option>)}
          </NativeSelect>
          <NativeSelect label="来源" name="recovery_source" disabled={busy} value={source} onChange={event => {
            setSource(event.target.value === "trash" ? "trash" : "history"); setItems(undefined); setCursor(undefined); setNext(undefined); setChosen(undefined);
          }}><option value="history">文件历史</option><option value="trash">回收站</option></NativeSelect>
        </SimpleGrid>
        {source === "history" && <Stack gap="sm">
          <Group>
            <Button variant="default" disabled={busy || !binding} onClick={() => run(async () => {
              const result = await agent.listFiles(binding); if (!result.files) throw new Error("agent未返回文件页。"); setFiles(result.files); setFilesNext(result.next_cursor);
            })}>读取文件当前页</Button>
            <Button variant="default" disabled={busy || !filesNext} onClick={() => run(async () => {
              const result = await agent.listFiles(binding, filesNext); if (!result.files) throw new Error("agent未返回文件页。"); setFiles(result.files); setFilesNext(result.next_cursor); setEntry("");
            })}>下一文件页</Button>
          </Group>
          <NativeSelect label="文件" name="history_entry" disabled={busy} value={entry} onChange={event => { setEntry(event.target.value); setItems(undefined); setCursor(undefined); setNext(undefined); setChosen(undefined); }}>
            <option value="">请选择文件</option>{files?.filter(file => file.entry_id).map(file => <option key={file.entry_id} value={file.entry_id}>{file.path}</option>)}
          </NativeSelect>
        </Stack>}
        <Group><Button disabled={busy || !authenticated || source === "history" && !entry} onClick={() => run(() => load())}>读取恢复清单</Button></Group>
        {server && !authenticated && <Alert color="yellow">请先登录此绑定的服务器。</Alert>}
        <Text size="sm" role="status">{message}</Text>
      </Stack>
    </Card>
    {job && <Card component="section" withBorder>
      <Stack>
        <Title order={2} size="h4">原操作：{job.input.operationId}</Title>
        <Alert color={job.state === "committed" ? "green" : job.state === "rejected" ? "red" : "yellow"}>
          {job.state === "committed" ? `committed${job.path ? ` · ${job.path}` : " · 位置待核对"}` : job.state === "rejected" ? "rejected：服务器未接受恢复，请重新读取清单" : job.state === "pending" ? "pending：尚未提交，不能显示已恢复" : "结果未知：不创建新操作"}
        </Alert>
        <Group><Button variant="default" disabled={busy || !authenticated} onClick={() => run(() => restore(job))}>核对原恢复操作</Button></Group>
      </Stack>
    </Card>}
    <Table<RemoteItem> caption="恢复候选当前页" rows={items ?? []} rowKey={row => row.id} emptyMessage={items ? "当前页没有恢复候选。" : "尚未读取恢复候选。"} columns={[
      { key: "name", title: "内容", render: row => <>{row.name}{row.conflict_source ? " · 冲突副本（双方内容保留）" : ""}</> },
      { key: "version", title: "版本/大小", render: row => <Stack gap={4}><Text size="sm">{row.version_id ?? "目录"}</Text><Text size="xs" c="dimmed">{row.size ?? "—"} 字节</Text></Stack> },
      { key: "expiry", title: "到期下限", render: row => row.expires_at ? `UTC秒 ${row.expires_at}` : "当前活跃版本不按年龄到期" },
      { key: "restore", title: "操作", render: row => <Button size="xs" disabled={busy || !authenticated || !row.entry_id || !row.revision || !!job && job.state !== "committed" && job.state !== "rejected"} onClick={() => setChosen(row)}>恢复此内容</Button> },
    ]} />
    <Group justify="flex-end"><Button variant="default" disabled={busy || !authenticated || !cursor} onClick={() => run(() => load())}>第一页</Button><Button variant="default" disabled={busy || !authenticated || !next} onClick={() => run(() => load(next))}>下一页</Button></Group>
    <Dialog opened={chosen !== undefined} title="确认恢复服务器内容" onClose={() => setChosen(undefined)}>
      <Text>恢复 {chosen?.name}。服务器会校验权限和当前修订，并保留同名目标；只有committed才显示已恢复。</Text>
      <Group><Button disabled={busy || !authenticated || !chosen} onClick={() => {
        if (!chosen) return;
        const value = chosen; setChosen(undefined);
        run(() => restore({ input: { bindingId: binding, entryId: value.entry_id!, baseRevision: value.revision!, versionId: source === "history" ? value.version_id : undefined, operationId: crypto.randomUUID().replaceAll("-", "") }, state: "unknown" }));
      }}>确认恢复</Button></Group>
    </Dialog>
  </Stack>;
}
