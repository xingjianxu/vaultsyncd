import { useEffect, useRef, useState } from "react";
import { backupSnapshotManifest, backupSnapshotStatus, listBackupSnapshots, prepareBackupSnapshot, releaseBackupSnapshot, verifyBackupSnapshot, type BackupSnapshotStatus, type BackupSnapshotList } from "@vaultsyncd/api-client";
import { Button } from "@heroui/react/button";
import { Card } from "@heroui/react/card";
import { Input } from "@heroui/react/input";
import { Label } from "@heroui/react/label";
import { Dialog, EmptyState, StatusBadge, Table } from "../../components/ui";
import { useAuth } from "../auth/Auth";
import { SignedOut } from "../../services/auth";
import { LocalExport, type ExportRecord } from "./export";
import { indexManifest, type ArchiveIndex } from "./archive";

const phaseLabels = {preparing:"准备中",ready:"已就绪",failed:"准备失败",releasing:"释放中",released:"已释放",expired:"已到期"};
export function BackupPanel() {
 const auth=useAuth(),client=auth.client,user=auth.account?.user_id;
 const [data,setData]=useState<BackupSnapshotList|null>(null),[page,setPage]=useState("0"),[selected,setSelected]=useState<BackupSnapshotStatus|null>(null);
 const [busy,setBusy]=useState(false),[error,setError]=useState(""),[revision,setRevision]=useState(0);
 const [record,setRecord]=useState<ExportRecord|null>(null),[index,setIndex]=useState<ArchiveIndex|null>(null),[progress,setProgress]=useState("0"),[url,setUrl]=useState(""),[receiptUrl,setReceiptUrl]=useState(""),[confirmRelease,setConfirmRelease]=useState(false);
 const cancel=useRef<AbortController|null>(null);
 const prepareKey="vaultsyncd.backup-prepare."+user;
 function failed(cause: unknown){if(cause instanceof SignedOut)auth.reportSignedOut();setError(cause instanceof Error?cause.message:String(cause));}
 useEffect(()=>{setData(null);setSelected(null);
   const local=user?new LocalExport(user):null;let restored=local?.read()??null;
   if(restored?.state==="verified"){restored={...restored,state:"exported_unverified",reason:"页面重新打开，请重读暂存或重新选择另存归档核对；不沿用历史校验状态"};local?.save(restored);}
   setRecord(restored);setIndex(null);setUrl("");
   return ()=>{cancel.current?.abort();cancel.current=null;};},[user]);
 useEffect(()=>()=>{if(url)URL.revokeObjectURL(url);},[url]);
 useEffect(()=>()=>{if(receiptUrl)URL.revokeObjectURL(receiptUrl);},[receiptUrl]);
 useEffect(()=>{
   if(!client||!user)return;
   let active=true,running=false;const controller=new AbortController();
   async function refresh(){if(running)return;running=true;try{
     const list=await client!.request(options=>listBackupSnapshots(options,{page},{signal:controller.signal}));
     if(!active)return;setData(list);
     if(selected){const status=await client!.request(options=>backupSnapshotStatus(options,selected.backup_id,{signal:controller.signal}));if(active){setSelected(status);if(!status.job_pending&&status.phase==="ready")sessionStorage.removeItem(prepareKey);}}
   }catch(cause){if(active)failed(cause);}finally{running=false;}}
   void refresh();const timer=setInterval(()=>void refresh(),2000);
   return ()=>{active=false;controller.abort();clearInterval(timer);};
 },[client,user,page,revision,selected?.backup_id]);
 async function action(kind: "prepare"|"verify"|"release") {
   if(!client||!user||busy)return;
   setBusy(true);setError("");
   try{
     if(kind==="prepare"){
       const id=sessionStorage.getItem(prepareKey)??crypto.randomUUID().replaceAll("-","");
       sessionStorage.setItem(prepareKey,id);
       setSelected(await client.request(options=>prepareBackupSnapshot(options,{backup_id:id})));
     }else if(selected)setSelected(await client.request(options=>(kind==="verify"?verifyBackupSnapshot:releaseBackupSnapshot)(options,selected.backup_id)));
     setRevision(value=>value+1);
   }catch(cause){failed(cause);}finally{setBusy(false);}
 }
 async function manifest(){
   if(!client||!selected?.manifest_hash)return;setBusy(true);setError("");
   try{
     const response=await client.request(options=>backupSnapshotManifest(options,selected.backup_id));
     if(!response.body)throw new Error("缺少清单流");
     const reader=response.body.getReader(),chunks:Uint8Array[]=[];let length=0;
     try{while(true){const next=await reader.read();if(next.done)break;length+=next.value.length;if(length>256*1024*1024)throw new Error("清单超过有界上限");chunks.push(next.value);}}
     finally{await reader.cancel().catch(()=>{});}
     const bytes=new Uint8Array(length);let at=0;for(const chunk of chunks){bytes.set(chunk,at);at+=chunk.length;}
     setIndex(indexManifest(bytes,selected.manifest_hash,selected.backup_id));
   }catch(cause){failed(cause);}finally{setBusy(false);}
 }
 async function download(){
   if(!client||!user||!selected||busy)return;const local=new LocalExport(user),controller=new AbortController();
   cancel.current=controller;setBusy(true);setError("");setUrl("");setReceiptUrl("");setProgress("0");
   try{const running=local.download(client,selected,controller.signal,setProgress);setRecord(local.read());await running;}
   catch(cause){failed(cause);}finally{setRecord(local.read());setBusy(false);cancel.current=null;}
 }
 async function verify(file?:File){
   if(!user||!record||busy)return;const local=new LocalExport(user);setBusy(true);setError("");setUrl("");setReceiptUrl("");
   try{
     const result=await local.verify(record,file);setIndex(result);
     if(!file)setUrl(URL.createObjectURL(await local.file(record)));
     // Receipt remains exported_unverified until the separately saved archive is selected and reread.
     const receipt={format_version:1,state:file?"verified":"exported_unverified",backup_id:record.id,manifest_hash:record.hash,server_id:result.server,checked_files:String(result.items.length),reason:file?"selected archive verified":"browser staging verified; saved archive must be rechecked"};
     setReceiptUrl(URL.createObjectURL(new Blob([JSON.stringify(receipt,null,2)],{type:"application/json"})));
   }catch(cause){failed(cause);}finally{setRecord(local.read());setBusy(false);}
 }
 if(!user)return <EmptyState title="请先登录">备份包含账号验证材料，仅 owner 可准备、导出或释放。</EmptyState>;
 return <div className="space-y-6">
   <Card className="p-6 sm:p-8">
     <Card.Header><Card.Title render={props => <h2 {...props} />}>一致备份</Card.Title>
       <Card.Description>只包含已确认逻辑状态及保留的历史/回收站，不包含尚未采集的外部修改。同盘快照不是独立备份。</Card.Description>
     </Card.Header>
     <Card.Content className="mt-5 space-y-5">
       <div className="flex flex-wrap gap-3">
         <Button isDisabled={busy} onPress={()=>void action("prepare")}>准备一致备份（未知时重试同ID）</Button>
         <Button variant="secondary" isDisabled={busy} onPress={()=>setRevision(v=>v+1)}>刷新备份作业</Button>
       </div>
       {error&&<p role="alert" className="rounded-xl bg-danger-soft p-4 text-sm text-danger">{error}</p>}
       {!data&&!error&&<p role="status" className="text-sm text-muted">正在读取备份作业，尚不能判断是否存在有效备份。</p>}
       {data&&<Table caption="备份作业" rows={data.items} rowKey={s=>s.backup_id} emptyMessage="尚无备份；不能据此推断数据已安全导出。" columns={[
         {key:"id",title:"备份身份",render:s=><code className="text-xs break-all">{s.backup_id}</code>},
         {key:"state",title:"准备/作业",render:s=><div className="space-y-2"><StatusBadge tone={s.phase==="failed"?"danger":s.phase==="ready"?"info":"neutral"}>{phaseLabels[s.phase]}</StatusBadge><p className="text-sm">{s.job_pending?"作业未完成":"作业已结束"}</p><p className="text-sm text-muted">{s.detail}</p></div>},
         {key:"source",title:"源完整性",render:s=><span className="text-sm">{s.integrity==="verified"?"源已校验（不代表已下载）":s.integrity==="incomplete"?"源不完整":"源未校验"}</span>},
         {key:"action",title:"操作",render:s=><Button variant="secondary" isDisabled={busy} onPress={()=>setSelected(s)}>选择备份</Button>},
       ]}/>}
       {data?.next_page&&<Button variant="secondary" isDisabled={busy} onPress={()=>setPage(data.next_page!)}>下一页备份</Button>}
     </Card.Content>
   </Card>
   {selected&&<Card className="p-6 sm:p-8">
     <Card.Header><Card.Title render={props => <h2 {...props} />}>固定备份源</Card.Title>
       <Card.Description>核对服务端源后，将归档导出到独立位置。</Card.Description></Card.Header>
     <Card.Content className="mt-5 space-y-4">
       <p className="text-sm break-all">{selected.backup_id} · {phaseLabels[selected.phase]} · {selected.job_pending?"作业未完成，不能当作有效包":"作业已结束"}</p>
       <p className="rounded-xl bg-surface-secondary p-4 text-sm">源完整性：{selected.integrity}。这不是浏览器下载完整性。</p>
       <p className="text-xs text-muted break-all">租约到期：{new Date(Number(selected.expires_at)*1000).toISOString()} · 清单摘要 {selected.manifest_hash??"尚未生成"}</p>
       <div className="flex flex-wrap gap-3">
         <Button variant="secondary" isDisabled={busy||selected.phase!=="ready"||selected.job_pending} onPress={()=>void action("verify")}>校验服务端源</Button>
         <Button variant="secondary" isDisabled={busy||selected.phase!=="ready"} onPress={()=>void manifest()}>查看固定清单</Button>
         <Button isDisabled={busy||selected.phase!=="ready"||selected.job_pending} onPress={()=>void download()}>导出或重试缺失下载</Button>
         <Button variant="danger-soft" isDisabled={busy||selected.phase==="released"||selected.job_pending} onPress={()=>setConfirmRelease(true)}>释放服务端快照</Button>
         {!selected.job_pending&&["failed","released","expired"].includes(selected.phase)&&<Button variant="secondary" isDisabled={busy} onPress={()=>{sessionStorage.removeItem(prepareKey);void action("prepare");}}>使用新ID重新准备</Button>}
       </div>
     </Card.Content>
   </Card>}
   {index&&<Card className="p-6 sm:p-8">
     <Card.Header><Card.Title render={props => <h2 {...props} />}>固定清单</Card.Title></Card.Header>
     <Card.Content className="mt-4 space-y-4 text-sm">
       <p className="break-all">Server {index.server} · {index.items.length} 个文件/对象 · SHA-256 {index.hash}</p>
       <ul className="max-h-64 space-y-2 overflow-auto rounded-xl bg-surface-secondary p-4">{index.roots.slice(0,100).map(root=><li className="font-mono text-xs break-all" key={root.id}>Root {root.id} · seq {root.seq}</li>)}</ul>
       <p className="text-muted">清单项目按固定路径校验，下载归档不执行其中的内容。</p>
     </Card.Content>
   </Card>}
   {record&&<Card className="p-6 sm:p-8">
     <Card.Header><Card.Title render={props => <h2 {...props} />}>本机导出完整性</Card.Title>
       <Card.Description>浏览器暂存与独立备份位置不同。</Card.Description></Card.Header>
     <Card.Content className="mt-5 space-y-4">
       <p className="text-sm break-all">备份 {record.id} · 本次接收 {progress} 字节</p>
       <StatusBadge tone={record.state==="verified"?"success":"warning"}>{record.state==="verified"?"归档完整性已验证":record.state==="exported_unverified"?"已下载，尚未重读校验":"未完成，不能作为有效备份"}</StatusBadge>
       <p className="text-sm text-muted">{record.reason}</p>
       <p className="rounded-xl bg-warning-soft p-4 text-sm text-warning">归档含账号验证材料，请保存到仅管理员可读的位置；浏览器不能保证保存文件的权限。</p>
       <div className="flex flex-wrap gap-3">
         {busy&&cancel.current&&<Button variant="danger-soft" onPress={()=>cancel.current?.abort()}>取消下载并保留未完成状态</Button>}
         <Button variant="secondary" isDisabled={busy} onPress={()=>void verify()}>重读校验浏览器暂存</Button>
       </div>
       <div className="flex flex-wrap gap-4 text-sm">
         {url&&<a className="text-accent underline underline-offset-4" href={url} download={`backup-${record.id}.vsbk`}>另存已校验归档</a>}
         {receiptUrl&&<a className="text-accent underline underline-offset-4" href={receiptUrl} download={`backup-${record.id}.vsbk.verification.json`}>下载完整性收据</a>}
       </div>
       <div className="space-y-2 rounded-xl border border-border p-4">
         <Label htmlFor="backup-archive">选择另存归档再次验证</Label>
         <Input id="backup-archive" className="w-full" type="file" accept=".vsbk" disabled={busy} onChange={event=>{const file=event.target.files?.[0];if(file)void verify(file);event.target.value="";}} />
       </div>
     </Card.Content>
   </Card>}
   <Card variant="secondary" className="p-6 sm:p-8">
     <Card.Header><Card.Title render={props => <h2 {...props} />}>离线恢复</Card.Title></Card.Header>
     <Card.Content className="mt-3 space-y-3 text-sm text-muted">
       <p>先保管归档与收据、停止服务，使用 CLI 的 restore validate 流程核对归档、空状态目录和 Root 映射，再按离线恢复文档操作。页面不对在线数据库执行 restore。</p>
       <p>完整性校验不等于已完成恢复演练。导出到独立故障域并验证后再释放源快照。</p>
     </Card.Content>
   </Card>
   {confirmRelease&&<Dialog title="释放此备份源？" onClose={()=>setConfirmRelease(false)}>
     <p>只删除此ID的服务端准备快照并释放租约；不会删除共享对象或浏览器归档。先确认另存副本与收据完整。</p>
     <Button variant="danger" onPress={()=>{setConfirmRelease(false);void action("release");}}>确认释放此快照</Button>
   </Dialog>}
 </div>;
}
