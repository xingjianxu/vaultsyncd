import { useEffect, useState } from "react";
import { listRoots, listRecoveryEntries, listConflictCopies, listEntryHistory, listTrash, type RecoveryItem, type RecoveryPage, type RootSummary } from "@vaultsyncd/api-client";
import { Card } from "@heroui/react/card";
import { ButtonGroup } from "@heroui/react/button-group";
import { Label } from "@heroui/react/label";
import { ListBox } from "@heroui/react/list-box";
import { Select } from "@heroui/react/select";
import { Button, EmptyState, StatusBadge, Table } from "../../components/ui";
import { useAuth } from "../auth/Auth";
import { SignedOut } from "../../services/auth";
import { expiry, expired, RestoreSession, type RestoreRecord } from "./recovery";

export function RecoveryPanel() {
  const auth = useAuth(); const client = auth.client; const user = auth.account?.user_id;
  const [roots, setRoots] = useState<RootSummary[]>([]);
  const [root, setRoot] = useState(""); const [mode, setMode] = useState("entries"); const [entry, setEntry] = useState("");
  const [data, setData] = useState<RecoveryPage | null>(null); const [after, setAfter] = useState<string>();
  const [revision, setRevision] = useState(0); const [busy, setBusy] = useState(false); const [error, setError] = useState("");
  const [record, setRecord] = useState<RestoreRecord | null>(null); const [notice, setNotice] = useState(true);
  const [rootsLoading, setRootsLoading] = useState(false);
  useEffect(() => {
    setRoots([]); setRoot(""); setData(null); setRecord(null);
    if (!user || !client) return;
    let active = true;
    setRootsLoading(true);
    client.request(listRoots).then(value => { if(active) { setRoots(value.roots); setRoot(value.roots[0]?.root_id ?? ""); } }).catch(cause => { if(active) { if(cause instanceof SignedOut) auth.reportSignedOut(); setError(String(cause)); } }).finally(() => { if (active) setRootsLoading(false); });
    return () => { active = false; };
  }, [client, user]);
  useEffect(() => {
    setData(null); setError("");
    if (!client || !user || !root) return;
    let active = true; const abort = new AbortController();
    setRecord(new RestoreSession(user, root).read()); setBusy(true);
    const load = mode === "history" ? (options: Parameters<typeof listRoots>[0]) => listEntryHistory(options, root, entry, { after }, { signal: abort.signal })
      : (options: Parameters<typeof listRoots>[0]) => (mode === "trash" ? listTrash : mode === "conflicts" ? listConflictCopies : listRecoveryEntries)(options, root, { after }, { signal: abort.signal });
    client.request(load).then(value => { if(active) setData(value); }).catch(cause => {
      if (!active) return;
      if (cause instanceof SignedOut) auth.reportSignedOut();
      setError(cause instanceof Error ? cause.message : "读取失败");
    }).finally(() => { if(active) setBusy(false); });
    return () => { active = false; abort.abort(); };
  }, [client, user, root, mode, entry, after, revision]);
  async function restore(item?: RecoveryItem) {
    if(!client || !user || busy) return;
    setBusy(true); setError("");
    const session = new RestoreSession(user, root);
    try { setRecord(await (item ? session.begin(client, item, mode === "history") : session.resume(client))); }
    catch(cause) {
      setRecord(session.read());
      if(cause instanceof SignedOut) auth.reportSignedOut();
      setError(`${cause instanceof Error ? cause.message : "请求失败"}；保留原操作，可核对并重试。`);
    } finally { setBusy(false); }
  }
  function tab(next: string) { setMode(next); setAfter(undefined); setNotice(true); setRevision(value => value+1); }
  if(!user) return <EmptyState title="请先登录">登录后查看授权 Root 的历史、冲突副本和回收站。</EmptyState>;
  const unfinished = record && (!record.result || record.result.state === "pending");
  const title = mode === "trash" ? "回收站" : mode === "conflicts" ? "冲突副本" : mode === "history" ? "历史版本" : "文件与历史";
  return <div className="space-y-4">
  <Card className="border border-border p-4 sm:p-5">
    <Card.Header className="gap-2">
      <div className="text-xs font-semibold tracking-widest text-muted">RECOVERY</div>
      <Card.Title render={props => <h2 {...props} />}>文件恢复</Card.Title>
      <Card.Description>恢复保留当前目标；同名冲突时另名恢复。到期时间是最早回收时间，最终以服务端检查为准。</Card.Description>
    </Card.Header>
    <Card.Content className="mt-3 space-y-3">
      <div className="flex flex-col gap-4 sm:flex-row sm:items-end sm:justify-between">
        <Select className="w-full sm:max-w-sm" aria-label="恢复 Root" placeholder="选择授权目录" isDisabled={busy || rootsLoading || !roots.length}
          value={root || null} onChange={value => { setRoot(value == null ? "" : String(value)); setAfter(undefined); }}>
          <Label>恢复 Root</Label><Select.Trigger><Select.Value /><Select.Indicator /></Select.Trigger>
          <Select.Popover><ListBox>{roots.map(item => <ListBox.Item key={item.root_id} id={item.root_id} textValue={item.name || item.root_id}>
            <div className="min-w-0"><p className="truncate">{item.name || item.root_id}</p>{item.name && <p className="font-mono text-xs text-muted">{item.root_id}</p>}</div><ListBox.ItemIndicator />
          </ListBox.Item>)}</ListBox></Select.Popover>
        </Select>
        <Button size="sm" disabled={busy || rootsLoading || !root} onClick={() => {setAfter(undefined);setRevision(value => value+1);}}>刷新恢复清单</Button>
      </div>
      <ButtonGroup size="sm" className="self-start" aria-label="恢复清单类别">
        {["entries","conflicts","trash"].map((value,i) => <Button size="sm" key={value} disabled={busy} aria-pressed={mode === value || (value === "entries" && mode === "history")}
          variant={mode === value || (value === "entries" && mode === "history") ? "primary" : "secondary"}
          onClick={() => tab(value)}>{["文件与历史","冲突副本","回收站"][i]}</Button>)}
      </ButtonGroup>
      {mode === "conflicts" && notice && <div className="flex flex-wrap items-center justify-between gap-3 rounded-md bg-surface-secondary p-4 text-sm"><p>冲突副本保留了另一份修改。</p><Button size="sm" onClick={() => setNotice(false)}>关闭提示</Button></div>}
      {error && <p role="alert" className="rounded-md bg-danger/10 p-4 text-sm text-danger">{error}</p>}
      {(busy || rootsLoading) && <p role="status" className="text-sm text-muted">正在核对…</p>}
      {!rootsLoading && !roots.length && !error && <p className="text-sm text-muted">尚无授权 Root。</p>}
    </Card.Content>
  </Card>
  {record && <Card className="border border-border p-4 sm:p-5"><Card.Header><Card.Title render={props => <h2 {...props} />}>恢复操作</Card.Title><Card.Description>核对服务端结果后确认完成。未确认时保留原操作继续重试。</Card.Description></Card.Header><Card.Content className="mt-4 space-y-4"><p className="break-all text-sm text-muted">操作编号：<code>{record.request.op_id}</code></p>
    <StatusBadge tone={record.result?.state === "committed" ? "success" : "warning"}>{record.result?.state === "committed" ? "恢复已提交" : record.result?.state === "rejected" ? "恢复被拒绝" : record.result?.state === "pending" ? "恢复 pending，尚未提交" : "结果未知，尚未确认恢复"}</StatusBadge>
    {record.result?.error_message && <p>{record.result.error_message}</p>}
    {record.result?.state === "committed" && <p>实际恢复位置：{record.path ?? "尚未读取，请核对结果"} · 条目 {record.result.entry_id}</p>}
    <Button size="sm" disabled={busy} onClick={() => void restore()}>核对并重试原操作</Button>
  </Card.Content></Card>}
  {data && <Card className="min-w-0 border border-border p-4 sm:p-5"><Card.Header className="mb-4"><Card.Title render={props => <h2 {...props} />}>{title}</Card.Title><Card.Description>{mode === "history" ? "按版本核对内容，恢复操作保留当前目标。" : "选择文件查看历史，或核对待恢复的项目。"}</Card.Description></Card.Header><Card.Content className="min-w-0"><Table caption={title} rows={data.items} rowKey={item => item.id} emptyMessage="本页没有项目；这不表示客户端已同步。" columns={[
    { key:"name",title:"路径 / 身份",render:item=><div className="space-y-2"><code className="break-all font-medium">{item.name}</code><p className="font-mono text-xs text-muted">{item.entry_id}</p>{item.conflict_source && <p className="break-all text-xs text-muted">冲突源：{item.conflict_source}</p>}</div>},
    { key:"version",title:"版本 / 到期时间",render:item=><div className="space-y-2"><span className="break-all font-mono text-xs">{item.version_id ?? "目录"}</span><p className="text-xs text-muted">{expiry(item)}</p>{expired(item) && <StatusBadge tone="warning">保留期已到</StatusBadge>}</div>},
    { key:"action",title:"操作",render:item=> mode === "entries" || mode === "conflicts" ? <Button size="sm" disabled={busy || !item.version_id} onClick={() => {setEntry(item.entry_id);tab("history");}}>查看历史</Button>
      : <Button size="sm" disabled={busy || !!unfinished || expired(item)} onClick={() => void restore(item)}>恢复此项</Button>},
  ]}/>{data.next_cursor && <div className="mt-4 flex justify-end"><Button size="sm" disabled={busy} onClick={() => setAfter(data.next_cursor)}>下一页</Button></div>}</Card.Content></Card>}
  </div>;
}
