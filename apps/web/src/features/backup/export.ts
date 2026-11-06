import { exportBackupSnapshot, type BackupSnapshotStatus } from "@vaultsyncd/api-client";
import type { BrowserAuth } from "../../services/auth";
import { verifyArchive, type ArchiveIndex } from "./archive";

export type ExportRecord = { id: string; hash: string; file: string; state: "incomplete" | "exported_unverified" | "verified"; reason: string };
export class LocalExport {
  readonly key: string;
  constructor(user: string) { this.key="vaultsyncd.backup-export."+user; }
  read(): ExportRecord | null { const raw=sessionStorage.getItem(this.key);return raw ? JSON.parse(raw) as ExportRecord : null; }
  save(record: ExportRecord) { sessionStorage.setItem(this.key,JSON.stringify(record)); }
  async directory() { if(typeof navigator.storage.getDirectory!=="function")throw new Error("浏览器不支持安全流式暂存，请使用管理CLI导出。");return (await navigator.storage.getDirectory()).getDirectoryHandle("vaultsyncd-backups",{create:true}); }
  async file(record: ExportRecord): Promise<File> {return (await (await this.directory()).getFileHandle(record.file)).getFile();}
  async download(client: BrowserAuth, status: BackupSnapshotStatus, signal: AbortSignal, progress: (bytes: string)=>void): Promise<ExportRecord> {
    if(status.phase!=="ready" || !status.manifest_hash)throw new Error("源快照尚未就绪");
    const record: ExportRecord={id:status.backup_id,hash:status.manifest_hash,file:status.backup_id+"-"+crypto.randomUUID()+".vsbk",state:"incomplete",reason:"下载进行中；还不是有效备份"};
    this.save(record);
    const handle=await (await this.directory()).getFileHandle(record.file,{create:true});
    const writer=await handle.createWritable();let bytes=0n;
    try {
      const response=await client.request(options=>exportBackupSnapshot(options,status.backup_id,{signal}));
      if(!response.body)throw new Error("缺少归档流");
      await verifyArchive(response.body,record.hash,record.id,async chunk=>{
        if(signal.aborted)throw new Error("下载已取消");
        await writer.write(new Uint8Array(chunk));bytes+=BigInt(chunk.length);progress(bytes.toString());
      });
      await writer.close();
      record.state="exported_unverified";record.reason="已写入浏览器暂存；必须重读校验，另存副本还需单独核验";
      this.save(record);return record;
    } catch(error) {
      await writer.close().catch(()=>{});record.reason=signal.aborted?"下载已取消；部分暂存保留，不能作为有效备份":String(error);
      this.save(record);throw error;
    }
  }
  async verify(record: ExportRecord, selected?: File): Promise<ArchiveIndex> {
    // Withdraw old verification before every re-read.
    this.save({...record,state:"incomplete",reason:"正在重读校验"});
    try {
      const result=await verifyArchive((selected ?? await this.file(record)).stream(),record.hash,record.id);
      this.save({...record,state:"verified",reason:selected?"所选归档完整性已验证":"浏览器暂存完整性已验证；不代表另存副本已验证"});return result;
    } catch(error){this.save({...record,state:"incomplete",reason:String(error)});throw error;}
  }
}
