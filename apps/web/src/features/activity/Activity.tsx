import { TransferDirection, useLiveRefresh } from "@vaultsyncd/ui";
import { useCallback, useEffect, useState } from "react";
import { Alert, Badge, Button, Group, Loader, Modal, NativeSelect, ScrollArea, Stack, Table, Text } from "@mantine/core";
import { listActivity, listVaults, type ActivityPage, type ActivityRecord, type VaultSummary } from "@vaultsyncd/api-client";
import { useAuth } from "../auth/Auth";

function timestamp(value: string) {
  if (!/^(0|[1-9][0-9]*)$/.test(value)) return "未知";
  const date = new Date(Number(value) * 1000);
  return Number.isNaN(date.getTime()) ? "未知" : date.toLocaleString("zh-CN", { hour12: false });
}
function bytes(value?: string) {
  if (value === undefined || !/^(0|[1-9][0-9]*)$/.test(value)) return "未知";
  return `${BigInt(value).toLocaleString("zh-CN")} B`;
}
function outcome(record: ActivityRecord) {
  if (record.status === "failed") return "失败";
  if (record.action === "upload_finalize" && record.status === "ready") return "上传内容已校验";
  if (record.action === "sync_commit" && record.status === "committed") return "同步已提交";
  if (record.action === "download" && record.status === "served") return "已提供下载内容";
  return `${record.action} / ${record.status}`;
}
type LoadedPage = { key: string; value: ActivityPage };

export function ActivityPanel() {
  const { client, account } = useAuth();
  const user = account?.user_id;
  const [vaults, setVaults] = useState<VaultSummary[]>([]);
  const [vault, setVault] = useState("");
  const [direction, setDirection] = useState("");
  const [before, setBefore] = useState<string>();
  const [revision, setRevision] = useState(0);
  const [vaultRevision, setVaultRevision] = useState(0);
  const [vaultsBusy, setVaultsBusy] = useState(false);
  const [vaultError, setVaultError] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [loaded, setLoaded] = useState<LoadedPage>();
  const [chosen, setChosen] = useState<ActivityRecord>();
  const key = JSON.stringify([user, vault, direction, before]);
  const page = loaded?.key === key ? loaded.value : undefined;

  useEffect(() => {
    setVaults([]); setVault(""); setLoaded(undefined); setChosen(undefined); setVaultError(""); setBefore(undefined);
    if (!client || !user) { setVaultsBusy(false); return; }
    let active = true;
    const controller = new AbortController();
    setVaultsBusy(true);
    client.request(options => listVaults(options, { signal: controller.signal })).then(result => {
      if (!active) return;
      setVaults(result.vaults); setVault(result.vaults[0]?.vault_id ?? "");
    }).catch(cause => {
      if (active) setVaultError(cause instanceof Error ? cause.message : "读取 Vault 失败");
    }).finally(() => { if (active) setVaultsBusy(false); });
    return () => { active = false; controller.abort(); };
  }, [client, user, vaultRevision]);

  useEffect(() => {
    setError(""); setChosen(undefined); setBusy(false);
  }, [client, key]);

  const load = useCallback(async (signal: AbortSignal, background: boolean) => {
    if (!client || !user || !vault) return;
    if (!background) setBusy(true);
    try {
      const result = await client.request(options => listActivity(options, vault, { before, direction: direction || undefined }, { signal, headers: { "x-vaultsyncd-client-properties": "1" } }));
      if (signal.aborted) return;
      setLoaded({ key, value: result }); setError("");
    } catch (cause) {
      if (!signal.aborted) setError(cause instanceof Error ? cause.message : "读取记录失败");
    } finally { if (!signal.aborted) setBusy(false); }
  }, [client, user, vault, direction, before, key, revision]);
  useLiveRefresh(load, !!client && !!user && !!vault);

  function refresh() { setBefore(undefined); setRevision(value => value + 1); }
  const details: Array<[string, string | undefined]> = chosen ? [
    ["记录", chosen.id], ["时间", timestamp(chosen.occurred_at)], ["客户端", chosen.client_hostname || "未知主机"], ["操作系统", chosen.client_os || "未知"], ["登录来源 IP", chosen.client_ip || "未知"],
    ["客户端类型", chosen.client_kind === "browser" ? "浏览器" : chosen.client_kind === "client" ? "客户端" : "未知"], ["连接身份", chosen.client_id], ["设备声明身份", chosen.device_id],
    ["文件", chosen.path], ["条目", chosen.entry_id], ["版本", chosen.version_id],
    ["文件大小", chosen.size === undefined ? undefined : bytes(chosen.size)],
    ["本次内容字节", chosen.bytes === undefined ? undefined : bytes(chosen.bytes)], ["范围起点", chosen.offset],
    ["操作", chosen.operation_id], ["上传会话", chosen.upload_id], ["结果", outcome(chosen)], ["错误码", chosen.error_code],
  ] : [];
  return <Stack>
    <Group align="flex-end" gap="sm">
      <NativeSelect label="Vault" value={vault} disabled={vaultsBusy || !vaults.length} onChange={event => { setVault(event.currentTarget.value); setBefore(undefined); }} style={{ flex: 1, minWidth: 180 }}>
        {!vaults.length && <option value="">{vaultsBusy ? "正在读取…" : "暂无授权 Vault"}</option>}
        {vaults.map(item => <option key={item.vault_id} value={item.vault_id}>{item.name || item.vault_id}</option>)}
      </NativeSelect>
      <NativeSelect label="传输方向" value={direction} onChange={event => { setDirection(event.currentTarget.value); setBefore(undefined); }}>
        <option value="">全部</option><option value="upload">上传</option><option value="download">下载</option>
      </NativeSelect>
      <Button disabled={busy || vaultsBusy || !vault} onClick={refresh}>刷新</Button>
    </Group>
    <Text size="sm" c="dimmed">服务端观察记录；已提供下载内容不代表客户端已接收或落盘。</Text>
    {(vaultError || error) && <Alert color="red" title="读取失败"><Stack gap="xs"><Text size="sm">{vaultError || error}</Text><Button variant="light" disabled={busy || vaultsBusy} onClick={() => vaultError ? setVaultRevision(value => value + 1) : setRevision(value => value + 1)}>重试</Button></Stack></Alert>}
    {(busy || vaultsBusy) && <Group role="status"><Loader size="sm" /><Text size="sm">正在读取记录…</Text></Group>}
    {page && <ScrollArea><Table miw={960} aria-label="同步与传输记录" striped highlightOnHover>
      <Table.Thead><Table.Tr>{["时间", "客户端", "文件", "方向", "结果", "文件大小", "详情"].map(title => <Table.Th key={title}>{title}</Table.Th>)}</Table.Tr></Table.Thead>
      <Table.Tbody>{page.items.map(row => <Table.Tr key={row.id}>
        <Table.Td><Text size="sm" style={{ whiteSpace: "nowrap" }}>{timestamp(row.occurred_at)}</Text></Table.Td>
        <Table.Td><Text size="sm" style={{ overflowWrap: "anywhere", maxWidth: 200 }}>{row.client_hostname || "未知主机"}</Text></Table.Td>
        <Table.Td><Text size="sm" style={{ overflowWrap: "anywhere", maxWidth: 320 }}>{row.path ?? (row.action === "upload_finalize" ? "上传内容（尚未关联文件）" : "路径未知")}</Text></Table.Td>
        <Table.Td>{<TransferDirection direction={row.direction} />}</Table.Td>
        <Table.Td><Badge variant="light" color={row.status === "failed" ? "red" : row.status === "committed" ? "green" : "blue"}>{outcome(row)}</Badge></Table.Td>
        <Table.Td><Text size="sm" style={{ whiteSpace: "nowrap" }}>{bytes(row.size)}</Text></Table.Td>
        <Table.Td><Button variant="subtle" size="compact-xs" aria-label={`查看记录 ${row.id} 详情`} onClick={() => setChosen(row)}>详情</Button></Table.Td>
      </Table.Tr>)}</Table.Tbody>
    </Table></ScrollArea>}
    {page?.items.length === 0 && !error && <Text>暂无同步与传输记录。</Text>}
    <Group justify="flex-end">
      <Button variant="default" disabled={busy || vaultsBusy || !before} onClick={() => setBefore(undefined)}>第一页</Button>
      <Button variant="default" disabled={busy || !page?.next_cursor} onClick={() => setBefore(page?.next_cursor)}>下一页</Button>
    </Group>
    <Modal opened={chosen !== undefined} onClose={() => setChosen(undefined)} title="记录详情" size="lg">
      <Stack gap="xs">{details.filter(([, value]) => value !== undefined).map(([label, value]) => <Group key={label} align="flex-start" wrap="nowrap"><Text size="sm" c="dimmed" w={100} style={{ flexShrink: 0 }}>{label}</Text><Text size="sm" style={{ overflowWrap: "anywhere" }}>{value}</Text></Group>)}</Stack>
    </Modal>
  </Stack>;
}
