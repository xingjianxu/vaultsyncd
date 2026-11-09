import { sha256 } from "@noble/hashes/sha2.js";

const MAX_MANIFEST = 256 * 1024 * 1024;
const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
const hex = (bytes: Uint8Array) => Array.from(bytes, byte => byte.toString(16).padStart(2,"0")).join("");
type Item = { path: string; size: bigint; hash: string };
export type ArchiveIndex = { id: string; server: string; hash: string; items: Item[]; vaults: { id: string; seq: string }[] };
function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("清单对象无效");
  return value as Record<string,unknown>;
}
function text(value: Record<string,unknown>, field: string): string {
  if(typeof value[field] !== "string") throw new Error("清单字段缺失："+field);
  return value[field] as string;
}
function rows(value: Record<string,unknown>, field: string): Record<string,unknown>[] {
  if(!Array.isArray(value[field])) throw new Error("清单数组缺失："+field);
  return value[field].map(object);
}
function decimal(value: Record<string,unknown>, field: string): bigint {
  const raw=text(value,field);
  if(!/^(0|[1-9][0-9]*)$/.test(raw) || BigInt(raw)>9223372036854775807n) throw new Error("清单计数无效");
  return BigInt(raw);
}
function identity(raw: string, length: number) {
  if(!new RegExp("^[0-9a-f]{"+length+"}$").test(raw)) throw new Error("清单身份或摘要无效");
  return raw;
}
export function indexManifest(bytes: Uint8Array, expected: string, id: string): ArchiveIndex {
  if(!bytes.length || bytes.length>MAX_MANIFEST || hex(sha256(bytes))!==expected) throw new Error("清单摘要不符");
  const manifest=object(JSON.parse(decoder.decode(bytes)));
  if(manifest.format_version!==1 || manifest.state!=="prepared" || manifest.scope!=="confirmed_logical_state" ||
    manifest.sessions!=="invalidated" || manifest.external_uncollected_writes!=="excluded" || manifest.backup_id!==id ||
    !Number.isSafeInteger(manifest.schema_version) || Number(manifest.schema_version)<1) throw new Error("清单范围或身份不符");
  identity(id,32);
  const server=identity(text(manifest,"server_id"),32);
  const vaults=rows(manifest,"vaults").map(vault=>({id:identity(text(vault,"vault_id"),32),seq:decimal(vault,"seq").toString()}));
  const ids=new Set(vaults.map(vault=>vault.id));if(ids.size!==vaults.length) throw new Error("重复Vault");
  const counts=object(manifest.database_contents);
  for(const table of ["users","vaults","entries","versions","trash_entries","tombstones","changes","operations"]) decimal(counts,table);
  const items: Item[]=[];const paths=new Set<string>(),storagePaths=new Set<string>();
  function add(path: string, item: Record<string,unknown>) {
    if(paths.has(path)) throw new Error("重复归档路径");
    paths.add(path);items.push({path,size:decimal(item,"size"),hash:identity(text(item,"sha256"),64)});
  }
  for(const item of rows(manifest,"files")) {
    const path=text(item,"path");if(!["database.sqlite","config.json"].includes(path))throw new Error("未授权归档路径");
    add(path,item);
  }
  if(paths.size!==2)throw new Error("缺少数据库或配置");
  for(const item of rows(manifest,"objects")) {
    const vault=identity(text(item,"vault_id"),32),hash=identity(text(item,"sha256"),64);
    const path="objects/"+vault+"/"+hash;
    const canonical=vault+"/"+hash.slice(0,2)+"/"+hash;
    const storage=text(item,"storage_path");
    if(!ids.has(vault) || item.archive_path!==path || storage!==canonical || storagePaths.has(storage))throw new Error("对象路径越界或重复");
    storagePaths.add(storage);add(path,item);
  }
  items.sort((a,b)=>a.path<b.path?-1:a.path>b.path?1:0);
  return {id,server,hash:expected,items,vaults};
}

/** Each take is bounded; only the separately capped manifest is materialized. */
class Reader {
  private chunk: Uint8Array=new Uint8Array(0);private at=0;
  constructor(readonly source: ReadableStreamDefaultReader<Uint8Array>, readonly onBytes?: (bytes: Uint8Array)=>Promise<void>) {}
  async take(length: number): Promise<Uint8Array> {
    const bytes=new Uint8Array(length);let at=0;
    while(at<length) {
      if(this.at===this.chunk.length) {const next=await this.source.read();if(next.done)throw new Error("归档不完整：缺少数据或终止标记");this.chunk=next.value;this.at=0;}
      const count=Math.min(65536,length-at,this.chunk.length-this.at);
      const part=this.chunk.subarray(this.at,this.at+count);bytes.set(part,at);
      await this.onBytes?.(part);this.at+=count;at+=count;
    }return bytes;
  }
  async end() {
    if(this.at!==this.chunk.length || !(await this.source.read()).done)throw new Error("归档存在额外数据");
  }
}
/** VSBK001 framing and pinned Index rules follow api/backup-export.schema.json. */
export async function verifyArchive(stream: ReadableStream<Uint8Array>, expected: string, id: string, onBytes?: (bytes: Uint8Array)=>Promise<void>): Promise<ArchiveIndex> {
  const raw=stream.getReader(),reader=new Reader(raw,onBytes);
  try {
    if(decoder.decode(await reader.take(8))!=="VSBK001\n")throw new Error("归档格式无效");
    const length=new DataView((await reader.take(8)).buffer).getBigUint64(0);
    if(length===0n || length>BigInt(MAX_MANIFEST))throw new Error("清单超过有界上限");
    const index=indexManifest(await reader.take(Number(length)),expected,id);
    for(const item of index.items) {
      const nameLength=new DataView((await reader.take(4)).buffer).getUint32(0);
      if(!nameLength || nameLength>1024 || decoder.decode(await reader.take(nameLength))!==item.path)throw new Error("归档条目缺失、重复或越界");
      const size=new DataView((await reader.take(8)).buffer).getBigUint64(0);if(size!==item.size)throw new Error("归档长度不符");
      const hash=sha256.create();let remaining=size;
      while(remaining>0n) {const count=Number(remaining>65536n?65536n:remaining);hash.update(await reader.take(count));remaining-=BigInt(count);}
      if(hex(hash.digest())!==item.hash)throw new Error("归档内容摘要不符");
    }
    if(new DataView((await reader.take(4)).buffer).getUint32(0)!==0)throw new Error("归档缺少最终零标记");
    await reader.end();return index;
  } finally {await raw.cancel().catch(()=>{});raw.releaseLock();}
}
