// Executes only in the real native-acceptance WebView; production has no fixture/control command.
window.addEventListener("DOMContentLoaded", async () => {
  const report={ok:false}; const fixture=window.__S082_FIXTURE__; const invoke=window.__TAURI_INTERNALS__.invoke;
  const delay=()=>new Promise(r=>setTimeout(r,150));
  async function wait(label,predicate,timeout=120000){const end=Date.now()+timeout;while(Date.now()<end){try{if(await predicate())return;}catch(error){report.lastPollError=JSON.stringify(error);}await delay();}throw new Error(`timeout ${label}`);}
  function field(name,value){const el=document.querySelector(`[name="${name}"]`);if(!el)throw new Error(`field ${name}`);const p=el instanceof HTMLSelectElement?HTMLSelectElement.prototype:el instanceof HTMLTextAreaElement?HTMLTextAreaElement.prototype:HTMLInputElement.prototype;Object.getOwnPropertyDescriptor(p,"value").set.call(el,value);el.dispatchEvent(new Event(el instanceof HTMLSelectElement?"change":"input",{bubbles:true}));}
  async function click(label,scope=document){await wait(label,()=>[...scope.querySelectorAll("button")].some(b=>b.textContent===label&&!b.disabled));[...scope.querySelectorAll("button")].find(b=>b.textContent===label&&!b.disabled).click();await delay();}
  const text=()=>document.body.innerText;const confirmed=()=>text().includes("操作已由 agent 确认。");
  const row=path=>[...document.querySelectorAll("tbody tr")].find(r=>r.querySelector("td")?.textContent===path);
  async function listing(){return (await invoke("agent_list_files",{bindingId:report.bindingId})).files;}
  async function page(path){location.hash=path;await delay();}
  async function filePage(){await page("/files");await wait("files page",()=>document.querySelector('[name="files_binding"]'));field("files_binding",report.bindingId);await wait("listed seed",()=>row("server-seed.txt"));}
  async function controlPage(){await page("/bindings");await wait("binding page",()=>text().includes("管理与恢复"));await click("管理与恢复");}
  try{
    report.maliciousWebViewDenied=true;
    for(const [command,args] of [["arbitrary_command",{}],["plugin:shell|execute",{program:"cmd.exe",args:["/c","exit"]}],["plugin:fs|read_text_file",{path:"C:\\Windows\\win.ini"}],["plugin:window|set_title",{label:"main",title:"unauthorized"}],["agent_release_entry",{bindingId:"../outside",entryId:"bad",operationId:"bad"}]]){
      let denied=false;try{await invoke(command,args);}catch{denied=true;}if(!denied)throw new Error("malicious command accepted: "+command);
    }
    await wait("connection",()=>document.querySelector('[name="server_id"]')?.value?.length===32);
    field("server_id",fixture.serverId);field("server_url",fixture.url);field("ca_file",fixture.caFile);await click("保存连接");await wait("connection saved",confirmed);
    field("username",fixture.login);field("password",fixture.password);await click("登录");await wait("login",()=>text().includes("已登录（持有凭据"));
    await page("/bindings");await click("读取授权 Root");await wait("root",()=>document.querySelector(`option[value="${fixture.root}"]`));field("root_id",fixture.root);field("local_path",fixture.localPath);field("mode","on_demand");await click("创建绑定并开始合并");
    await wait("merge",async()=>{const s=await invoke("agent_get_status");const b=s.management?.bindings?.find(b=>b.local_path===fixture.localPath);if(!b)return false;report.bindingId=b.binding_id;return s.bindings.find(x=>x.binding_id===b.binding_id)?.synchronized;});
    await filePage();await click("下载",row("server-seed.txt"));await wait("downloaded",async()=>(await listing()).find(f=>f.path==="server-seed.txt")?.residency==="materialized");report.download=true;
    // Rendered clean state is deliberately stale by confirmation time.
    await click("刷新文件状态");await click("释放空间",row("server-seed.txt"));await wait("race dialog",()=>document.querySelector("dialog"));
    await invoke("acceptance_file_control",{action:"hold"});await click("确认释放",document.querySelector("dialog"));
    await wait("release rechecked",()=>!text().includes("操作正在执行，等待 agent 确认。"));
    if(confirmed()||(await listing()).find(f=>f.path==="server-seed.txt")?.residency!=="materialized")throw new Error("stale release confirmation removed busy content");
    report.releaseConfirmationRace=true;await invoke("acceptance_file_control",{action:"unhold"});
    await invoke("acceptance_file_control",{action:"hold"});await wait("busy",async()=>(await listing()).find(f=>f.path==="server-seed.txt")?.busy);
    await click("刷新文件状态");await wait("busy UI",()=>row("server-seed.txt")?.textContent.includes("正在使用")&&[...row("server-seed.txt").querySelectorAll("button")].find(b=>b.textContent==="释放空间")?.disabled);report.busyUi=true;
    try{await invoke("agent_release_entry",{bindingId:report.bindingId,entryId:(await listing()).find(f=>f.path==="server-seed.txt").entry_id,operationId:crypto.randomUUID().replaceAll("-","")});}catch{report.busyBackendRejects=true;}
    await invoke("acceptance_file_control",{action:"unhold"});await wait("idle",async()=>(await listing()).find(f=>f.path==="server-seed.txt")?.release_allowed);
    await click("刷新文件状态");await click("释放空间",row("server-seed.txt"));await wait("release impact",()=>document.querySelector("dialog")?.textContent.includes("离线将不可读取"));await click("确认释放",document.querySelector("dialog"));await wait("released",async()=>(await listing()).find(f=>f.path==="server-seed.txt")?.residency==="metadata_only");report.release=true;
    await click("刷新文件状态");await click("下载",row("server-seed.txt"));await wait("redownload",async()=>(await listing()).find(f=>f.path==="server-seed.txt")?.residency==="materialized");
    await controlPage();await click("读取排除规则");await wait("rules editable",()=>!document.querySelector('[name="exclusion_rules"]')?.disabled);
    field("exclusion_rules","server-seed.txt\n");await click("保存排除规则");await wait("rules durable",async()=>(await invoke("agent_get_rules",{bindingId:report.bindingId})).rules==="server-seed.txt\n");
    let excludedRejected=false;try{await invoke("agent_release_entry",{bindingId:report.bindingId,entryId:(await listing()).find(f=>f.path==="server-seed.txt").entry_id,operationId:crypto.randomUUID().replaceAll("-","")});}catch{excludedRejected=true;}
    if(!excludedRejected)throw new Error("excluded entry release accepted");
    field("exclusion_rules","");await click("保存排除规则");await wait("clear reconciled",async()=>(await listing()).find(f=>f.path==="server-seed.txt")?.release_allowed);report.rulesGui=true;
    await controlPage();field("target_mode","full");await click("保存模式");await wait("full target",async()=>(await invoke("agent_get_status")).management.bindings.find(b=>b.binding_id===report.bindingId)?.mode==="full");
    await filePage();await wait("full release disabled",()=>row("server-seed.txt")?.textContent.includes("完整同步模式"));report.fullModeKeepsContent=true;
    await controlPage();field("target_mode","on_demand");await click("保存模式");await wait("on demand",async()=>(await invoke("agent_get_status")).management.bindings.find(b=>b.binding_id===report.bindingId)?.mode==="on_demand");await click("暂停");await wait("paused",()=>text().includes("已暂停"));
    await invoke("acceptance_file_control",{action:"dirty"});await filePage();await click("刷新文件状态");await wait("dirty UI",()=>row("local-only.txt")?.textContent.includes("未确认")&&[...row("local-only.txt").querySelectorAll("button")].find(b=>b.textContent==="释放空间")?.disabled);report.dirtyUi=true;
    const dirty=(await listing()).find(f=>f.path==="local-only.txt");try{await invoke("agent_release_entry",{bindingId:report.bindingId,entryId:dirty.entry_id,operationId:crypto.randomUUID().replaceAll("-","")});}catch{report.dirtyBackendRejects=true;}
    const pending=(await listing()).find(f=>f.path==="pending-only.txt");report.pendingNotGreen=pending?.pending===true&&pending.synchronized===false;
    await controlPage();field("recovery_destination",fixture.exportPath);await click("导出恢复包");await wait("export proof",()=>text().includes("恢复包已核验"));report.export=true;
    await click("解除绑定（保留数据）");await wait("detach impact",()=>document.querySelector("dialog")?.textContent.includes("不会删除服务器文件"));await invoke("acceptance_file_control",{action:"late-dirty"});await click("确认保留数据并解除",document.querySelector("dialog"));await wait("detached",async()=>(await invoke("agent_get_status")).management.bindings.find(b=>b.binding_id===report.bindingId)?.detached);report.detach=true;
    await page("/problems");await click("读取问题");await wait("problems read",confirmed);report.problems=true;
    report.ok=["maliciousWebViewDenied","releaseConfirmationRace","rulesGui","download","busyUi","busyBackendRejects","release","fullModeKeepsContent","dirtyUi","dirtyBackendRejects","pendingNotGreen","export","detach","problems"].every(k=>report[k]===true);
  }catch(error){report.error=typeof error==="object"?JSON.stringify(error):String(error);report.page=text();}
  await invoke("acceptance_report",{report});
});
