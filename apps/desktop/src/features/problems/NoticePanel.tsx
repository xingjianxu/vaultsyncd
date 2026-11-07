import { useEffect,useState } from "react";
import { Button,Table } from "@vaultsyncd/ui";
import type { Notice } from "@vaultsyncd/local-ipc-types";
import type { AgentClient,NotificationSettings } from "../../services/agent";
export function NoticePanel({agent,notices,run,busy}:{agent:AgentClient;notices?:Notice[];run(action:()=>Promise<void>):void;busy:boolean}){
  const[value,setValue]=useState<NotificationSettings>();const[error,setError]=useState("");
  useEffect(()=>{let stopped=false;async function read(){try{const settings=await agent.notificationSettings();if(!stopped){setValue(settings);setError("");}}catch{if(!stopped)setError("通知设置不可用；问题仍由agent提供。");}}void read();const timer=window.setInterval(()=>void read(),2000);return()=>{stopped=true;window.clearInterval(timer);};},[agent]);
  return <section className="vs-card"><h2>系统通知与恢复状态</h2><p>开启系统通知会为本应用注册开始菜单快捷方式；关闭不会清除问题/待办。同原因同Root每个活动代际只领取一次，清除通知中心也不会清理问题、待办或冲突副本。</p>
    <Button disabled={busy||!value} onClick={()=>run(async()=>{setValue(await agent.setNotifications(!value!.enabled));})}>{value?.enabled?"关闭系统通知":"开启系统通知"}</Button>
    <p role="status">{value?`系统通知${value.enabled?"已开启":"已关闭"}；本次界面提交给Windows ${value.delivered} 条，失败或策略禁用 ${value.blocked} 条。`:"尚未读取通知设置。"}</p>
    {(error||value?.last_error)&&<p role="alert">{error||value?.last_error}</p>}
    <Table<Notice> caption="Root问题生命周期" rows={notices??[]} rowKey={n=>n.key} emptyMessage={notices?"未记录需处理的Root问题。":"尚未取得问题生命周期。"} columns={[
      {key:"root",title:"Root",render:n=>n.root_id??"全局"},
      {key:"cause",title:"原因",render:n=>n.cause==="CONFLICT_COPY"?"冲突副本待检查":n.cause},
      {key:"state",title:"状态",render:n=>n.state==="resolved"?"已恢复":`仍需处理 · 活动代际 ${n.generation}`},
      {key:"delivery",title:"系统投递",render:n=>n.claimed?"本代际已领取投递（不代表用户已读）":"待前台领取"},
    ]}/>
  </section>;
}
