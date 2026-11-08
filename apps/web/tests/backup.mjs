import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";
const server=await createServer({root:fileURLToPath(new URL("../../..",import.meta.url)),configFile:false,server:{middlewareMode:true,hmr:false},ssr:{noExternal:["@noble/hashes"]}});
try {
 const { verifyArchive,indexManifest }=await server.ssrLoadModule("/apps/web/src/features/backup/archive.ts");
 const digest=bytes=>createHash("sha256").update(bytes).digest("hex");
 const id="ab".repeat(16),root="cd".repeat(16);
 const large=Buffer.alloc(2*1024*1024,0x51),hash=digest(large);
 const manifest={format_version:1,state:"prepared",scope:"confirmed_logical_state",sessions:"invalidated",external_uncollected_writes:"excluded",backup_id:id,server_id:"ef".repeat(16),schema_version:13,roots:[{root_id:root,seq:"1"}],database_contents:Object.fromEntries(["users","roots","entries","versions","trash_entries","tombstones","changes","operations"].map(key=>[key,"0"])),files:[{path:"database.sqlite",size:"0",sha256:digest(Buffer.alloc(0))},{path:"config.json",size:"2",sha256:digest(Buffer.from("{}"))}],objects:[{root_id:root,sha256:hash,size:String(large.length),storage_path:"objects/"+hash.slice(0,2)+"/"+hash,archive_path:"objects/"+root+"/"+hash}]};
 const bytes=Buffer.from(JSON.stringify(manifest)),expected=digest(bytes);
 const u32=n=>{const b=Buffer.alloc(4);b.writeUInt32BE(n);return b;},u64=n=>{const b=Buffer.alloc(8);b.writeBigUInt64BE(BigInt(n));return b;};
 const parts=[Buffer.from("VSBK001\n"),u64(bytes.length),bytes];
 for(const [name,content]of [["config.json",Buffer.from("{}")],["database.sqlite",Buffer.alloc(0)],["objects/"+root+"/"+hash,large]]){
   const path=Buffer.from(name);parts.push(u32(path.length),path,u64(content.length),content);
 }
 parts.push(u32(0));const archive=Buffer.concat(parts);
 const stream=(data,step=719)=>new ReadableStream({start(controller){for(let at=0;at<data.length;at+=step)controller.enqueue(new Uint8Array(data.subarray(at,at+step)));controller.close();}});
 let received=0,maxChunk=0;
 assert.equal((await verifyArchive(stream(archive),expected,id,async bytes=>{received+=bytes.length;maxChunk=Math.max(maxChunk,bytes.length);})).items.length,3);
 assert.equal(received,archive.length);assert.ok(maxChunk<=65536);
 await assert.rejects(verifyArchive(stream(archive.subarray(0,-4)),expected,id),/不完整/);
 const bad=Buffer.from(archive);bad[bad.length-100]^=1;
 await assert.rejects(verifyArchive(stream(bad),expected,id),/摘要/);
 await assert.rejects(verifyArchive(stream(Buffer.concat([archive,Buffer.from("x")])),expected,id),/额外/);
 await assert.rejects(verifyArchive(stream(archive),"0".repeat(64),id),/清单摘要/);
 const bom=parts.slice();const pathWithBOM=Buffer.from("\ufeffconfig.json");bom[3]=u32(pathWithBOM.length);bom[4]=pathWithBOM;
 await assert.rejects(verifyArchive(stream(Buffer.concat(bom)),expected,id),/越界/);
 const malicious=structuredClone(manifest);malicious.files[0].path="../live.sqlite";
 const encoded=Buffer.from(JSON.stringify(malicious));
 assert.throws(()=>indexManifest(encoded,digest(encoded),id),/未授权/);
 // S136/schema15: 新Root-qualified登记路径与legacy归档均可验证，归档成员路径保持不变。
 const scoped=structuredClone(manifest);scoped.schema_version=15;
 scoped.objects[0].storage_path="roots/"+root+"/"+scoped.objects[0].storage_path;
 const scopedBytes=Buffer.from(JSON.stringify(scoped)),scopedParts=parts.slice();
 scopedParts[1]=u64(scopedBytes.length);scopedParts[2]=scopedBytes;
 assert.equal((await verifyArchive(stream(Buffer.concat(scopedParts)),digest(scopedBytes),id)).items.length,3);
 const another="12".repeat(16);
 scoped.roots.push({root_id:another,seq:"1"});
 scoped.objects.push({...scoped.objects[0],root_id:another,archive_path:"objects/"+another+"/"+hash,storage_path:"roots/"+another+"/objects/"+hash.slice(0,2)+"/"+hash});
 const inspect=value=>{const encoded=Buffer.from(JSON.stringify(value));return ()=>indexManifest(encoded,digest(encoded),id);};
 assert.equal(inspect(scoped)().items.length,4,"两Root同摘要不能被当作同一个物理对象");
 for(const invalid of ["roots/"+another+"/objects/"+hash.slice(0,2)+"/"+hash,"roots/"+root+"/../objects/"+hash.slice(0,2)+"/"+hash,"roots/"+root+"//objects/"+hash.slice(0,2)+"/"+hash]){
   const changed=structuredClone(scoped);changed.objects[0].storage_path=invalid;assert.throws(inspect(changed),/对象路径/);
 }
 const duplicate=structuredClone(scoped);for(const item of duplicate.objects)item.storage_path="objects/"+hash.slice(0,2)+"/"+hash;
 assert.throws(inspect(duplicate),/重复/,"legacy路径不能让不同Root共享同一物理登记");
 const { exportBackupSnapshot }=await server.ssrLoadModule("/packages/api-client/src/index.ts");
 const raw=new Response(stream(archive));raw.arrayBuffer=()=>{throw new Error("must not materialize archive");};
 const result=await exportBackupSnapshot({base_url:"https://vault.invalid",request_id:()=>"stream-test",fetch:async()=>raw},id);
 assert.equal(result,raw);
 console.log("backup: 任意分块/有界写入、坏hash、截断缺尾、额外字节、清单路径越界、生成客户端不缓冲归档通过");
}finally{await server.close();}
