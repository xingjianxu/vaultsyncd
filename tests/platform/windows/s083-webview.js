// Real Windows notification-center verification. Cause fixtures are generated offline by core ports.
window.addEventListener("DOMContentLoaded",async()=>{
  const report={ok:false};const fixture=window.__S083_FIXTURE__;const invoke=window.__TAURI_INTERNALS__.invoke;
  const delay=()=>new Promise(r=>setTimeout(r,150));
  async function wait(label,fn){const end=Date.now()+120000;while(Date.now()<end){try{if(await fn())return;}catch(e){report.lastError=String(e);if(String(e).includes("Windows notification failure"))throw e;}await delay();}throw new Error(`timeout ${label}`);}
  async function click(label){await wait(label,()=>[...document.querySelectorAll("button")].some(b=>b.textContent===label&&!b.disabled));[...document.querySelectorAll("button")].find(b=>b.textContent===label&&!b.disabled).click();await delay();}
  const settings=()=>invoke("agent_notification_settings");const status=()=>invoke("agent_get_status");const history=()=>invoke("acceptance_toast_history",{clear:false});
  try{
    await wait("agent",async()=>Array.isArray((await status()).management?.notices));if(fixture.phase!=="history"){location.hash="/problems";
    await wait("problem page",()=>document.body.innerText.includes("系统通知与恢复状态"));
    if(!(await settings()).enabled)await click("开启系统通知");}
    if(fixture.phase==="restart_check"){
      for(let i=0;i<15;i++)await status();
      report.guiRestartNoNewDelivery=(await settings()).delivered===0;report.actualCenterStillPresent=(await history()).length===1;
      report.ok=report.guiRestartNoNewDelivery&&report.actualCenterStillPresent;
    }else if(fixture.phase==="notifications"){
      await wait("real Windows center",async()=>{const s=await settings();if(s.blocked)throw new Error(`Windows notification failure ${s.last_error}`);return(await history()).length===3;});
      report.centerTags=await history();report.initialDelivered=(await settings()).delivered;
      await wait("cause projection",()=>document.body.innerText.includes("MOUNT_FAILED")&&document.body.innerText.includes("INSUFFICIENT_STORAGE")&&document.body.innerText.includes("冲突副本待检查"));report.causesVisible=true;
      for(let i=0;i<20;i++)await status();report.noPollStorm=(await settings()).delivered===report.initialDelivered;
      await invoke("acceptance_fixture_phase",{action:"duplicate"});for(let i=0;i<15;i++)await status();report.duplicateAndAgentRestartNoStorm=(await settings()).delivered===report.initialDelivered;
      await invoke("acceptance_toast_history",{clear:true});for(let i=0;i<15;i++)await status();report.closeKeepsProblems=(await history()).length===0&&(await status()).management.notices.filter(n=>n.state==="active").length===3;
      await click("关闭系统通知");await wait("disabled",async()=>!(await settings()).enabled);report.disabledKeepsProblems=(await status()).management.notices.filter(n=>n.state==="active").length===3;
      await invoke("acceptance_fixture_phase",{action:"resolve"});await wait("resolved",async()=>(await status()).management.notices.filter(n=>n.state==="resolved").length===2);await wait("recovery page",()=>document.body.innerText.includes("已恢复"));report.recoveredVisible=true;
      await click("开启系统通知");for(let i=0;i<10;i++)await status();report.recoveryNoNewPopup=(await history()).length===0;
      await invoke("acceptance_fixture_phase",{action:"offline"});for(let i=0;i<10;i++)await status();report.ordinaryOfflineSilent=(await history()).length===0&&(await status()).management.notices.length===3;
      await invoke("acceptance_fixture_phase",{action:"recur"});await wait("recurrence",async()=>(await history()).length===1);report.recurrenceOnce=(await settings()).delivered===report.initialDelivered+1;
      report.finalCenterTags=await history();report.coreFixtureNotVolumeFault=true;
      report.ok=report.initialDelivered===3&&["causesVisible","noPollStorm","duplicateAndAgentRestartNoStorm","closeKeepsProblems","disabledKeepsProblems","recoveredVisible","recoveryNoNewPopup","ordinaryOfflineSilent","recurrenceOnce"].every(k=>report[k]===true);
    }else if(fixture.phase==="history"){
      function field(name,value){const el=document.querySelector(`[name="${name}"]`);if(!el)throw new Error(`field ${name}`);const p=el instanceof HTMLSelectElement?HTMLSelectElement.prototype:HTMLInputElement.prototype;Object.getOwnPropertyDescriptor(p,"value").set.call(el,value);el.dispatchEvent(new Event(el instanceof HTMLSelectElement?"change":"input",{bubbles:true}));}
      const text=()=>document.body.innerText;const confirmed=()=>text().includes("操作已由 agent 确认。");
      location.hash="/connections";await wait("connection form",()=>document.querySelector('[name="server_id"]')?.value?.length===32);
      field("server_id",fixture.serverId);field("server_url",fixture.url);field("ca_file",fixture.caFile);await click("保存连接");await wait("saved",confirmed);
      field("username",fixture.login);field("password",fixture.password);await click("登录");await wait("logged in",()=>text().includes("已登录（持有凭据"));
      location.hash="/bindings";await click("读取授权 Root");await wait("root",()=>document.querySelector(`option[value="${fixture.root}"]`));await window.__TAURI_INTERNALS__.invoke("agent_create_binding",{input:{bindingId:crypto.randomUUID().replaceAll("-",""),rootId:fixture.root,localPath:fixture.localPath,mode:"full"}});
      await wait("initial synchronized",async()=>{const s=await status();const b=s.management?.bindings?.find(x=>x.local_path===fixture.localPath);if(!b)return false;report.bindingId=b.binding_id;return s.bindings.find(x=>x.binding_id===b.binding_id)?.synchronized;});
      location.hash="/problems";await wait("real conflict notice",()=>text().includes("冲突副本待检查"));report.realConflictVisible=true;
      location.hash="/history";await wait("history page",()=>document.querySelector('[name="history_binding"]'));field("history_binding",report.bindingId);await click("读取文件当前页");await wait("history entry",()=>document.querySelector(`option[value="${fixture.head.entry_id}"]`));field("history_entry",fixture.head.entry_id);await click("读取恢复清单");
      await wait("old version and expiry",()=>text().includes(fixture.original.version_id)&&text().includes("UTC秒"));
      const oldRow=[...document.querySelectorAll("tbody tr")].find(r=>r.textContent.includes(fixture.original.version_id));oldRow.querySelector("button").click();await click("确认恢复");await wait("history committed",()=>text().includes("恢复已提交，实际位置："));
      const historyJob=JSON.parse(sessionStorage.getItem(`vaultsyncd.desktop.restore.${report.bindingId}`));report.historyPath=historyJob.path;report.historyOperation=historyJob.input.operationId;report.historyCommitted=historyJob.state==="committed";
      await click("核对原恢复操作");await wait("same original operation",()=>JSON.parse(sessionStorage.getItem(`vaultsyncd.desktop.restore.${report.bindingId}`)).input.operationId===report.historyOperation&&confirmed());
      const repeated=await invoke("agent_restore_entry",{input:historyJob.input});report.sameOperationSamePath=repeated.operation_id===report.historyOperation&&repeated.restored_path===report.historyPath;
      try{await invoke("agent_list_history",{bindingId:report.bindingId,entryId:fixture.foreign.entry_id});}catch{report.foreignRootHistoryRejected=true;}
      try{await invoke("agent_restore_entry",{input:{bindingId:report.bindingId,entryId:fixture.foreign.entry_id,baseRevision:fixture.foreign.revision,versionId:fixture.foreign.version_id,operationId:crypto.randomUUID().replaceAll("-","")}});}catch{report.foreignRootRestoreRejected=true;}
      field("recovery_source","trash");await click("读取恢复清单");await wait("deleted item",()=>text().includes("trash.txt"));const trashRow=[...document.querySelectorAll("tbody tr")].find(r=>r.querySelector("td")?.textContent==="trash.txt");trashRow.querySelector("button").click();await click("确认恢复");await wait("trash committed",()=>{const j=JSON.parse(sessionStorage.getItem(`vaultsyncd.desktop.restore.${report.bindingId}`));return j.input.operationId!==report.historyOperation&&j.state==="committed"&&j.path;});const trashJob=JSON.parse(sessionStorage.getItem(`vaultsyncd.desktop.restore.${report.bindingId}`));report.trashPath=trashJob.path;report.trashOperation=trashJob.input.operationId;report.trashNamePreserved=trashJob.path!=="trash.txt";
      await click("核对原恢复操作");await wait("trash retry",confirmed);await wait("agent converged",async()=>(await status()).bindings.find(x=>x.binding_id===report.bindingId)?.synchronized);
      report.ok=["realConflictVisible","historyCommitted","sameOperationSamePath","foreignRootHistoryRejected","foreignRootRestoreRejected","trashNamePreserved"].every(k=>report[k]===true);
    }else{throw new Error("unknown S083 acceptance phase");}
  }catch(e){report.error=String(e);report.settings=await settings().catch(e=>String(e));report.page=document.body.innerText;}
  await invoke("acceptance_report",{report});
});
