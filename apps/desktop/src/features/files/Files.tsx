import { useCallback, useEffect, useRef, useState } from "react";
import { Button, Dialog, Table } from "@vaultsyncd/ui";
import type { BindingInfo, FileStatus } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";
const labels: Record<string,string> = { unavailable:"不可用", failed:"失败",conflict:"冲突",local_changes:"待上传本地修改",syncing:"正在同步",downloaded:"已下载",cloud_only:"仅云端" };
export function Files({ agent, bindings = [], run, busy, changed }: { agent:AgentClient;bindings?:BindingInfo[];run(action:()=>Promise<void>):void;busy:boolean;changed():Promise<void> }) {
  const [binding,select] = useState(""); const [rows,setRows] = useState<FileStatus[]>();
  const [cursor,setCursor] = useState<string>(); const [next,setNext] = useState<string>();
  const [message,setMessage] = useState("请选择绑定后读取实际状态。"); const [fresh,setFresh] = useState(false);
  const [selected,setSelected] = useState<FileStatus>(); const [search,setSearch] = useState("");
  const sequence = useRef(0); const reading = useRef(false); const operations = useRef(new Map<string,string>());
  const current = bindings.find(b=>b.binding_id===binding);
  const load = useCallback(async () => {
    if (!binding || reading.current) return; reading.current=true; const version=sequence.current;
    try { const result=await agent.listFiles(binding,cursor); if (!result.files) throw new Error("agent 未返回文件页。"); if(version===sequence.current){setRows(result.files);setNext(result.next_cursor);setFresh(true);setMessage("当前页实际状态；执行操作时后端仍会重新核对。");} }
    catch(error){if(version===sequence.current){setFresh(false);setMessage(error instanceof Error?error.message:"文件状态读取失败。");}} finally{reading.current=false;}
  },[agent,binding,cursor]);
  useEffect(()=>{sequence.current++;setRows(undefined);setFresh(false);void load();},[load]);
  useEffect(()=>{if(busy)return;const timer=window.setInterval(()=>void load(),2000);return()=>window.clearInterval(timer);},[load,busy]);
  async function action(row:FileStatus,kind:"download"|"release") {
    if(!row.entry_id)throw new Error("文件尚未确认身份。");const key=`${binding}:${row.entry_id}:${kind}`;
    let op=operations.current.get(key);if(!op){op=crypto.randomUUID().replaceAll("-","");operations.current.set(key,op);}
    await (kind==="download"?agent.downloadEntry(binding,row.entry_id,op):agent.releaseEntry(binding,row.entry_id,op));
    operations.current.delete(key);await load();await changed();
  }
  return <><h1>文件状态与操作</h1><label>绑定<select name="files_binding" value={binding} onChange={e=>{select(e.target.value);setCursor(undefined);}}><option value="">请选择绑定</option>{bindings.map(b=><option key={b.binding_id} value={b.binding_id}>{b.local_path}{b.detached?"（已解除）":""}</option>)}</select></label>
    <label>搜索当前页<input name="file_search" value={search} onChange={e=>setSearch(e.target.value)} /></label>
    <p role="status">{message}</p><Button disabled={busy||!binding} onClick={()=>run(load)}>刷新文件状态</Button>
    <Table<FileStatus> caption="文件当前页" rows={(rows??[]).filter(r=>r.path.toLowerCase().includes(search.toLowerCase()))} rowKey={r=>r.path} emptyMessage={rows?"当前页没有匹配文件。":"尚未读取文件。"} columns={[
      {key:"path",title:"路径",render:r=>r.path},
      {key:"state",title:"状态",render:r=><span>{labels[r.status]??"未知状态"}{r.pending?" · 待确认/未完成":""}{r.busy?" · 正在使用或保护未知":""}{r.synchronized&&!r.pending&&!r.dirty?" · 基线已确认":""}</span>},
      {key:"actions",title:"操作",render:r=><><Button disabled={busy||!fresh||!r.entry_id||r.dirty||current?.paused||current?.detached} onClick={()=>run(()=>action(r,"download"))}>下载</Button><Button disabled={busy||!fresh||!r.release_allowed||current?.detached} onClick={()=>setSelected(r)}>释放空间</Button><p>{r.release_reason}</p></>},
    ]}/>
    <Button disabled={busy||!cursor} onClick={()=>setCursor(undefined)}>回到第一页</Button><Button disabled={busy||!next} onClick={()=>setCursor(next)}>下一页</Button>
    {selected&&<Dialog title="确认释放本地内容" onClose={()=>setSelected(undefined)}><p>{selected.path} 的本地内容将移除，远端数据保留；离线将不可读取。未确认或忙文件不会释放。</p><Button onClick={()=>{const row=selected;setSelected(undefined);run(()=>action(row,"release"));}}>确认释放</Button></Dialog>}
  </>;
}
