import { useEffect, useState } from "react";
import { listVaults, listRecoveryEntries, listConflictCopies, listEntryHistory, listTrash, type RecoveryItem, type RecoveryPage, type VaultSummary } from "@vaultsyncd/api-client";
import { Alert, Button, Group, Paper, Select, Stack, Text, Title } from "@mantine/core";
import { EmptyState, StatusBadge, Table } from "../../components/ui";
import { Feedback, showFeedback } from "../../components/Feedback";
import { useAuth } from "../auth/Auth";
import { SignedOut } from "../../services/auth";
import { expiry, expired, RestoreSession, type RestoreRecord } from "./recovery";

export function RecoveryPanel() {
  const auth = useAuth(); const client = auth.client; const user = auth.account?.user_id;
  const [vaults, setVaults] = useState<VaultSummary[]>([]);
  const [vault, setVault] = useState(""); const [mode, setMode] = useState("entries"); const [entry, setEntry] = useState("");
  const [data, setData] = useState<RecoveryPage | null>(null); const [after, setAfter] = useState<string>();
  const [revision, setRevision] = useState(0); const [busy, setBusy] = useState(false); const [error, setError] = useState("");
  const [record, setRecord] = useState<RestoreRecord | null>(null); const [notice, setNotice] = useState(true);
  const [vaultsLoading, setVaultsLoading] = useState(false);
  useEffect(() => {
    setVaults([]); setVault(""); setData(null); setRecord(null);
    if (!user || !client) return;
    let active = true;
    setVaultsLoading(true);
    client.request(listVaults).then(value => { if(active) { setVaults(value.vaults); setVault(value.vaults[0]?.vault_id ?? ""); } }).catch(cause => { if(active) { if(cause instanceof SignedOut) auth.reportSignedOut(); setError(String(cause)); } }).finally(() => { if (active) setVaultsLoading(false); });
    return () => { active = false; };
  }, [client, user]);
  useEffect(() => {
    setData(null); setError("");
    if (!client || !user || !vault) return;
    let active = true; const abort = new AbortController();
    setRecord(new RestoreSession(user, vault).read()); setBusy(true);
    const load = mode === "history" ? (options: Parameters<typeof listVaults>[0]) => listEntryHistory(options, vault, entry, { after }, { signal: abort.signal })
      : (options: Parameters<typeof listVaults>[0]) => (mode === "trash" ? listTrash : mode === "conflicts" ? listConflictCopies : listRecoveryEntries)(options, vault, { after }, { signal: abort.signal });
    client.request(load).then(value => { if(active) setData(value); }).catch(cause => {
      if (!active) return;
      if (cause instanceof SignedOut) auth.reportSignedOut();
      setError(cause instanceof Error ? cause.message : "读取失败");
    }).finally(() => { if(active) setBusy(false); });
    return () => { active = false; abort.abort(); };
  }, [client, user, vault, mode, entry, after, revision]);
  async function restore(item?: RecoveryItem) {
    if(!client || !user || busy) return;
    setBusy(true); setError("");
    const session = new RestoreSession(user, vault);
    try {
      const restored = await (item ? session.begin(client, item, mode === "history") : session.resume(client));
      setRecord(restored);
      if (restored?.result?.state === "committed") showFeedback(`恢复已提交，实际恢复位置：${restored.path ?? "请核对结果"}`);
      else if (restored?.result?.state === "rejected") showFeedback(restored.result.error_message ?? "恢复被拒绝", "red");
      else showFeedback("恢复尚未确认提交，请核对原操作。", "yellow");
    }
    catch(cause) {
      setRecord(session.read());
      if(cause instanceof SignedOut) auth.reportSignedOut();
      setError(`${cause instanceof Error ? cause.message : "请求失败"}；保留原操作，可核对并重试。`);
    } finally { setBusy(false); }
  }
  function tab(next: string) { setMode(next); setAfter(undefined); setNotice(true); setRevision(value => value+1); }
  if(!user) return <EmptyState title="请先登录">登录后查看授权 Vault 的历史、冲突副本和回收站。</EmptyState>;
  const unfinished = record && (!record.result || record.result.state === "pending");
  const title = mode === "trash" ? "回收站" : mode === "conflicts" ? "冲突副本" : mode === "history" ? "历史版本" : "文件与历史";
  return <Stack>
    <Paper withBorder p="lg">
      <Stack>
        <Title order={2} size="h4">文件恢复</Title>
        <Text size="sm" c="dimmed">恢复保留当前目标；同名冲突时另名恢复。到期时间是最早回收时间，最终以服务端检查为准。</Text>
        <Group align="flex-end" justify="space-between">
          <Select label="恢复 Vault" placeholder="选择授权目录" disabled={busy || vaultsLoading || !vaults.length}
            value={vault || null} allowDeselect={false} data={vaults.map(item => ({ value: item.vault_id, label: item.name || item.vault_id }))}
            onChange={value => { setVault(value == null ? "" : String(value)); setAfter(undefined); }} className="w-full sm:max-w-sm" />
          <Button variant="default" disabled={busy || vaultsLoading || !vault} onClick={() => { setAfter(undefined); setRevision(value => value + 1); }}>刷新恢复清单</Button>
        </Group>
        <Group aria-label="恢复清单类别" gap="xs">
          {["entries", "conflicts", "trash"].map((value, i) => <Button key={value} disabled={busy}
            aria-pressed={mode === value || (value === "entries" && mode === "history")}
            variant={mode === value || (value === "entries" && mode === "history") ? "filled" : "default"}
            onClick={() => tab(value)}>{["文件与历史", "冲突副本", "回收站"][i]}</Button>)}
        </Group>
        {mode === "conflicts" && notice && <Alert><Group justify="space-between"><Text size="sm">冲突副本保留了另一份修改。</Text><Button variant="subtle" onClick={() => setNotice(false)}>关闭提示</Button></Group></Alert>}
        <Feedback message={error} />
        {(busy || vaultsLoading) && <Text role="status" size="sm" c="dimmed">正在核对…</Text>}
        {!vaultsLoading && !vaults.length && !error && <Text size="sm" c="dimmed">尚无授权 Vault。</Text>}
      </Stack>
    </Paper>
    {record && <Paper withBorder p="lg">
      <Stack>
        <Title order={2} size="h4">恢复操作</Title>
        <Text size="sm" c="dimmed">核对服务端结果后确认完成。未确认时保留原操作继续重试。</Text>
        <Text size="sm" c="dimmed" className="break-all">操作编号：<code>{record.request.op_id}</code></Text>
        <StatusBadge tone={record.result?.state === "committed" ? "success" : "warning"}>{record.result?.state === "committed" ? "恢复已提交" : record.result?.state === "rejected" ? "恢复被拒绝" : record.result?.state === "pending" ? "恢复 pending，尚未提交" : "结果未知，尚未确认恢复"}</StatusBadge>
        {record.result?.error_message && <Text size="sm">{record.result.error_message}</Text>}
        {record.result?.state === "committed" && <Text size="sm">实际恢复位置：{record.path ?? "尚未读取，请核对结果"} · 条目 {record.result.entry_id}</Text>}
        <Group><Button variant="default" disabled={busy} onClick={() => void restore()}>核对并重试原操作</Button></Group>
      </Stack>
    </Paper>}
    {data && <Paper withBorder p="lg">
      <Stack>
        <Title order={2} size="h4">{title}</Title>
        <Text size="sm" c="dimmed">{mode === "history" ? "按版本核对内容，恢复操作保留当前目标。" : "选择文件查看历史，或核对待恢复的项目。"}</Text>
        <Table caption={title} rows={data.items} rowKey={item => item.id} emptyMessage="本页没有项目；这不表示客户端已同步。" columns={[
          { key: "name", title: "路径 / 身份", render: item => <Stack gap="xs"><Text component="code" fw={500} className="break-all">{item.name}</Text><Text size="xs" c="dimmed">{item.entry_id}</Text>{item.conflict_source && <Text size="xs" c="dimmed" className="break-all">冲突源：{item.conflict_source}</Text>}</Stack> },
          { key: "version", title: "版本 / 到期时间", render: item => <Stack gap="xs"><Text component="code" size="xs" className="break-all">{item.version_id ?? "目录"}</Text><Text size="xs" c="dimmed">{expiry(item)}</Text>{expired(item) && <StatusBadge tone="warning">保留期已到</StatusBadge>}</Stack> },
          { key: "action", title: "操作", render: item => mode === "entries" || mode === "conflicts" ? <Button variant="default" disabled={busy || !item.version_id} onClick={() => { setEntry(item.entry_id); tab("history"); }}>查看历史</Button>
            : <Button disabled={busy || !!unfinished || expired(item)} onClick={() => void restore(item)}>恢复此项</Button> },
        ]} />
        {data.next_cursor && <Group justify="flex-end"><Button variant="default" disabled={busy} onClick={() => setAfter(data.next_cursor)}>下一页</Button></Group>}
      </Stack>
    </Paper>}
  </Stack>;
}
