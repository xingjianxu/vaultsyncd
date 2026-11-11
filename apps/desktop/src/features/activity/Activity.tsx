import { useCallback, useEffect, useRef, useState } from "react";
import { Alert, Card, Group, NativeSelect, SimpleGrid, Stack, Text, Title } from "@mantine/core";
import { Button, Dialog, StatusBadge, Table } from "@vaultsyncd/ui";
import type { ActivityRecord, BindingInfo, ConnectionInfo } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";

type Direction = "" | "upload" | "download";

export function activityTime(value: string): string {
  if (!/^(0|[1-9][0-9]*)$/.test(value)) return `UTC秒 ${value}`;
  const seconds = Number(value);
  const date = new Date(seconds * 1000);
  return Number.isSafeInteger(seconds) && Number.isFinite(date.getTime())
    ? date.toLocaleString("zh-CN", { hour12: false }) : `UTC秒 ${value}`;
}
export function activityResult(row: ActivityRecord): string {
  if (row.status === "failed") return "失败";
  if (row.action === "sync_commit" && row.status === "committed") return "同步已提交";
  if (row.action === "upload_finalize" && row.status === "ready") return "上传内容已校验";
  if (row.action === "download" && row.status === "served") return "已提供下载内容";
  return `${row.action} · ${row.status}`;
}
export function activityFile(row: ActivityRecord): string {
  return row.path ?? (row.action === "upload_finalize" ? "待提交的上传内容" : row.entry_id ?? "文件路径未知");
}

export function Activity({ agent, bindings = [], connections = [] }: { agent: AgentClient; bindings?: BindingInfo[]; connections?: ConnectionInfo[] }) {
  const [binding, setBinding] = useState("");
  const [direction, setDirection] = useState<Direction>("");
  const [rows, setRows] = useState<ActivityRecord[]>();
  const [cursor, setCursor] = useState<string>();
  const [next, setNext] = useState<string>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [selected, setSelected] = useState<ActivityRecord>();
  const sequence = useRef(0);
  const current = bindings.find(item => item.binding_id === binding);
  const available = !!current;
  const server = connections.find(item => item.server_id === current?.server_id);
  const authenticated = server?.authenticated === true;
  const endpoint = server?.server_url;
  const load = useCallback(async (before?: string) => {
    const request = ++sequence.current;
    setRows(undefined); setNext(undefined); setError(""); setSelected(undefined);
    if (!binding || !available || !authenticated) { setLoading(false); setCursor(undefined); return; }
    setLoading(true);
    try {
      const result = await agent.listActivity(binding, before, direction || undefined);
      if (!result.activity) throw new Error("agent 未返回同步与传输记录。");
      if (request !== sequence.current) return;
      setRows(result.activity); setNext(result.next_cursor); setCursor(before);
    } catch (failure) {
      if (request === sequence.current) {
        setError(failure instanceof Error ? failure.message : "同步与传输记录读取失败。");
      }
    } finally { if (request === sequence.current) setLoading(false); }
  }, [agent, binding, direction, available, authenticated, endpoint]);
  useEffect(() => {
    void load();
    return () => { sequence.current++; };
  }, [load]);
  function chooseBinding(value: string) {
    if (value === binding) return;
    sequence.current++; setRows(undefined); setNext(undefined); setCursor(undefined); setSelected(undefined); setError(""); setBinding(value);
  }
  function chooseDirection(value: Direction) {
    if (value === direction) return;
    sequence.current++; setRows(undefined); setNext(undefined); setCursor(undefined); setSelected(undefined); setError(""); setDirection(value);
  }
  return <Stack gap="lg">
    <Title order={1} size="h2">同步与传输记录</Title>
    <Card withBorder><Stack>
      <SimpleGrid cols={{ base: 1, sm: 2 }}>
        <NativeSelect label="绑定" name="activity_binding" value={binding} onChange={event => chooseBinding(event.target.value)}>
          <option value="">请选择绑定</option>{bindings.map(item => <option key={item.binding_id} value={item.binding_id}>{item.local_path}{item.detached ? "（已解除）" : ""}</option>)}
        </NativeSelect>
        <NativeSelect label="方向" name="activity_direction" value={direction} onChange={event => chooseDirection(event.target.value as Direction)}>
          <option value="">全部</option><option value="upload">上传</option><option value="download">下载</option>
        </NativeSelect>
      </SimpleGrid>
      <Group justify="space-between">
        <Text size="sm" c="dimmed">服务端记录 · 所选 Vault 的所有客户端</Text>
        <Button variant="default" disabled={loading || !available || !authenticated} onClick={() => void load()}>刷新记录</Button>
      </Group>
      {current && <Text size="xs" c="dimmed">Vault {current.vault_id}</Text>}
    </Stack></Card>
    {server && !authenticated && <Alert color="yellow">请先登录此绑定的服务器。</Alert>}
    {error && <Alert color="red" role="alert">{error}</Alert>}
    {loading && <Text role="status">正在读取记录…</Text>}
    <Table<ActivityRecord> caption="同步与传输记录当前页" rows={rows ?? []} rowKey={row => row.id}
      emptyMessage={!available ? "请选择绑定。" : !authenticated ? server ? "尚未登录此绑定的服务器。" : "尚未读取记录。" : loading ? "正在读取记录…" : error ? "记录读取失败。" : rows ? "当前筛选没有记录。" : "请选择绑定。"} columns={[
        { key: "time", title: "时间", render: row => activityTime(row.occurred_at) },
        { key: "client", title: "客户端", render: row => row.client_hostname || "未知主机" },
        { key: "file", title: "文件", render: row => <Text size="sm" style={{ overflowWrap: "anywhere" }}>{activityFile(row)}</Text> },
        { key: "direction", title: "方向", render: row => row.direction === "upload" ? "上传" : row.direction === "download" ? "下载" : row.direction },
        { key: "result", title: "结果", render: row => <StatusBadge tone={row.status === "failed" ? "danger" : "neutral"}>{activityResult(row)}</StatusBadge> },
        { key: "size", title: "文件大小", render: row => row.size === undefined ? "—" : `${row.size} 字节` },
        { key: "details", title: "详情", render: row => <Button variant="default" size="xs" onClick={() => setSelected(row)}>查看详情</Button> },
      ]} />
    <Group justify="flex-end">
      <Button variant="default" disabled={loading || !cursor} onClick={() => void load()}>第一页</Button>
      <Button variant="default" disabled={loading || !next || !!error} onClick={() => void load(next)}>下一页</Button>
    </Group>
    <Dialog opened={!!selected} title="记录详情" onClose={() => setSelected(undefined)}>
      {selected && <Stack gap="sm">
        <Text>{activityFile(selected)}</Text>
        <Text>{activityResult(selected)}</Text>
        {selected.action === "download" && <Text size="sm">仅记录服务端提供内容；客户端接收、校验与应用结果未上报。</Text>}
        {selected.action === "upload_finalize" && <Text size="sm">内容已上传并校验，文件同步提交结果见后续记录。</Text>}
        <dl style={{ margin: 0, overflowWrap: "anywhere" }}>
          {([
            ["记录", selected.id], ["时间", activityTime(selected.occurred_at)], ["客户端", selected.client_hostname || "未知主机"], ["操作系统", selected.client_os || "未知"], ["登录来源 IP", selected.client_ip || "未知"],
            ["接入类型", selected.client_kind], ["连接身份", selected.client_id], ["设备", selected.device_id],
            ["条目", selected.entry_id], ["版本", selected.version_id], ["操作", selected.operation_id],
            ["上传会话", selected.upload_id], ["文件大小（字节）", selected.size], ["本次提供（字节）", selected.bytes],
            ["区间起点（字节）", selected.offset], ["错误码", selected.error_code],
          ] as [string, string | undefined][]).filter(([, value]) => value !== undefined).map(([label, value]) =>
            <div key={label}><dt><Text size="sm" c="dimmed">{label}</Text></dt><dd style={{ margin: "0 0 8px" }}>{value}</dd></div>)}
        </dl>
      </Stack>}
    </Dialog>
  </Stack>;
}
