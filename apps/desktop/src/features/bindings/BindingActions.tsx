import { useState } from "react";
import { Button, Dialog } from "@vaultsyncd/ui";
import type { BindingInfo, BindingStatus } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";
export function BindingActions({agent,binding,status,run,busy,changed}:{agent:AgentClient;binding:BindingInfo;status?:BindingStatus;run(action:()=>Promise<void>):void;busy:boolean;changed():Promise<void>}) {
  const [mode,setMode]=useState<"full"|"on_demand">(binding.mode==="full"?"full":"on_demand");
  const [destination,setDestination]=useState("");const [confirm,setConfirm]=useState(false);const [message,setMessage]=useState("");
  return <section className="vs-card"><h2>绑定管理：{binding.local_path}</h2><p aria-live="polite">{message}</p>
    <label>目标模式<select name="target_mode" value={mode} onChange={e=>setMode(e.target.value==="full"?"full":"on_demand")}><option value="full">完整同步</option><option value="on_demand">按需下载</option></select></label>
    <p>切到完整同步会补齐内容；完成前仍显示进行中。切到按需不会自动释放已下载内容。</p>
    <Button disabled={busy||binding.detached} onClick={()=>run(async()=>{const result=await agent.setMode(binding.binding_id,mode);setMessage(result.mode_remaining?`模式补齐仍需核对 ${result.mode_remaining} 项。`:"模式目标已保存，请查看真实进度。");await changed();})}>保存模式</Button>
    <label>恢复导出到新目录<input name="recovery_destination" value={destination} onChange={e=>setDestination(e.target.value)} placeholder="现有父目录下的新绝对路径" /></label>
    <p>先导出未确认数据并核对恢复包；失败时源内容保留。</p>
    <Button disabled={busy||!destination} onClick={()=>run(async()=>{const result=await agent.exportRecovery(binding.binding_id,destination);if(result.exported_files===undefined)throw new Error("agent 未返回导出完成证明。");setMessage(`恢复包已核验 ${result.exported_files} 份内容。`);})}>导出恢复包</Button>
    <Button disabled={busy||binding.detached} onClick={()=>setConfirm(true)}>解除绑定（保留数据）</Button>
    {confirm&&<Dialog title="确认解除绑定" onClose={()=>setConfirm(false)}><p>停止管理此本地目录，保留本地内容、未确认数据和缓存；不会删除服务器文件。仅云端 {status?.metadata_only??"未知"} 项、部分内容 {status?.partial??"未知"} 项不能承诺离线可读，建议先导出恢复包。</p><Button onClick={()=>{setConfirm(false);run(async()=>{const result=await agent.detachBinding(binding.binding_id);if(!result.unconfirmed||!result.incomplete)throw new Error("agent 未返回解绑保留报告。");setMessage(`已停止管理，保留未确认 ${result.unconfirmed.length} 项；非完整内容 ${result.incomplete.length} 项仍不能保证离线读取。`);await changed();});}}>确认保留数据并解除</Button></Dialog>}
  </section>;
}
