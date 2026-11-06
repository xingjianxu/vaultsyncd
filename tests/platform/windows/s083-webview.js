// Real Windows notification-center verification. Cause fixtures are generated offline by core ports.
window.addEventListener("DOMContentLoaded",async()=>{
  const report={ok:false};const fixture=window.__S083_FIXTURE__;const invoke=window.__TAURI_INTERNALS__.invoke;
  const delay=()=>new Promise(r=>setTimeout(r,150));
  async function wait(label,fn){const end=Date.now()+120000;while(Date.now()<end){try{if(await fn())return;}catch(e){report.lastError=JSON.stringify(e);}await delay();}throw new Error(`timeout ${label}`);}
  async function click(label){await wait(label,()=>[...document.querySelectorAll("button")].some(b=>b.textContent===label&&!b.disabled));[...document.querySelectorAll("button")].find(b=>b.textContent===label&&!b.disabled).click();await delay();}
  const settings=()=>invoke("agent_notification_settings");const status=()=>invoke("agent_get_status");const history=()=>invoke("acceptance_toast_history",{clear:false});
  try{
    await wait("agent",async()=>Array.isArray((await status()).management?.notices));location.hash="/problems";
    await wait("problem page",()=>document.body.innerText.includes("系统通知与恢复状态"));
    if(!(await settings()).enabled)await click("开启系统通知");
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
    }else{throw new Error("unknown S083 acceptance phase");}
  }catch(e){report.error=String(e);report.settings=await settings().catch(e=>String(e));report.page=document.body.innerText;}
  await invoke("acceptance_report",{report});
});
