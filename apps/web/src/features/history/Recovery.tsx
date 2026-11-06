import { useEffect, useState } from "react";
import { listRoots, listRecoveryEntries, listConflictCopies, listEntryHistory, listTrash, type RecoveryItem, type RecoveryPage, type RootSummary } from "@vaultsyncd/api-client";
import { Button, EmptyState, StatusBadge, Table } from "@vaultsyncd/ui";
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
  useEffect(() => {
    setRoots([]); setRoot(""); setData(null); setRecord(null);
    if (!user || !client) return;
    let active = true;
    client.request(listRoots).then(value => { if(active) { setRoots(value.roots); setRoot(value.roots[0]?.root_id ?? ""); } }).catch(cause => { if(active) { if(cause instanceof SignedOut) auth.reportSignedOut(); setError(String(cause)); } });
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
  return <><section className="vs-card"><h2>文件恢复</h2><label>恢复 Root<select disabled={busy} aria-label="恢复 Root" value={root} onChange={event => {setRoot(event.target.value);setAfter(undefined);}}>
    {roots.map(item => <option key={item.root_id} value={item.root_id}>{item.root_id}</option>)}</select></label>
    <p>恢复保留当前目标；同名冲突时另名恢复。到期时间是最早回收时间，最终以服务端检查为准。</p>
    {["entries","conflicts","trash"].map((value,i) => <Button key={value} disabled={busy} onClick={() => tab(value)}>{["文件与历史","冲突副本","回收站"][i]}</Button>)}{" "}
    <Button disabled={busy} onClick={() => {setAfter(undefined);setRevision(value => value+1);}}>刷新恢复清单</Button>
    {mode === "conflicts" && notice && <p>冲突副本保留了另一份修改。<Button onClick={() => setNotice(false)}>关闭提示</Button></p>}
    {error && <p role="alert">{error}</p>}{busy && <p role="status">正在核对…</p>}
    {!roots.length && <p>尚无授权 Root。</p>}
  </section>
  {record && <section className="vs-card"><h2>恢复操作</h2><p>操作编号：<code>{record.request.op_id}</code></p>
    <StatusBadge tone={record.result?.state === "committed" ? "success" : "warning"}>{record.result?.state === "committed" ? "恢复已提交" : record.result?.state === "rejected" ? "恢复被拒绝" : record.result?.state === "pending" ? "恢复 pending，尚未提交" : "结果未知，尚未确认恢复"}</StatusBadge>
    {record.result?.error_message && <p>{record.result.error_message}</p>}
    {record.result?.state === "committed" && <p>实际恢复位置：{record.path ?? "尚未读取，请核对结果"} · 条目 {record.result.entry_id}</p>}
    <Button disabled={busy} onClick={() => void restore()}>核对并重试原操作</Button>
  </section>}
  {data && <><Table caption={mode === "trash" ? "回收站" : mode === "conflicts" ? "冲突副本" : mode === "history" ? "历史版本" : "文件与历史"} rows={data.items} rowKey={item => item.id} emptyMessage="本页没有项目；这不表示客户端已同步。" columns={[
    { key:"name",title:"路径 / 身份",render:item=><><code>{item.name}</code><p>{item.entry_id}</p>{item.conflict_source && <p>冲突源：{item.conflict_source}</p>}</>},
    { key:"version",title:"版本 / 到期时间",render:item=><>{item.version_id ?? "目录"}<p>{expiry(item)}</p>{expired(item) && <StatusBadge tone="warning">保留期已到</StatusBadge>}</>},
    { key:"action",title:"操作",render:item=> mode === "entries" || mode === "conflicts" ? <Button disabled={busy || !item.version_id} onClick={() => {setEntry(item.entry_id);tab("history");}}>查看历史</Button>
      : <Button disabled={busy || !!unfinished || expired(item)} onClick={() => void restore(item)}>恢复此项</Button>},
  ]}/>{data.next_cursor && <Button disabled={busy} onClick={() => setAfter(data.next_cursor)}>下一页</Button>}</>}
  </>;
}
