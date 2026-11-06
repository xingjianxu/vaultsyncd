import { useState,useEffect } from "react";
import { Button, Table } from "@vaultsyncd/ui";
import type { Problem,Notice } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";
import { NoticePanel } from "./NoticePanel";
export function Problems({agent,run,busy,current,notices}:{agent:AgentClient;run(action:()=>Promise<void>):void;busy:boolean;current?:Problem[];notices?:Notice[]}) {
  const [items,setItems]=useState<Problem[]>();const [search,setSearch]=useState("");
  useEffect(()=>{if(current)setItems(current);},[current]);
  async function load(){const result=await agent.listProblems(undefined,search||undefined);if(!result.problems)throw new Error("agent 未返回问题清单。");setItems(result.problems);}
  return <><h1>问题与恢复</h1><label>搜索路径或原因<input value={search} onChange={e=>setSearch(e.target.value)} /></label><Button disabled={busy} onClick={()=>run(load)}>读取问题</Button>
    <NoticePanel agent={agent} notices={notices} run={run} busy={busy}/>
    <Table<Problem> caption="真实持久问题" rows={(items??[]).filter(p=>`${p.path} ${p.code}`.toLowerCase().includes(search.toLowerCase()))} rowKey={p=>`${p.binding_id}:${p.problem_id}`} emptyMessage={items?"没有匹配的持久问题。":"尚未读取问题。"} columns={[
      {key:"path",title:"路径",render:p=>p.path??p.entry_id??"未知路径"},
      {key:"reason",title:"原因",render:p=><>{p.code}：{p.reason}<p>本地内容：{p.local_data}</p><p>下一步：{p.next_step}</p></>},
      {key:"retry",title:"操作",render:p=><Button disabled={busy||!p.retryable} onClick={()=>run(async()=>{await agent.retryProblem(p.binding_id,p.problem_id);await load();})}>重试</Button>},
    ]}/></>;
}
