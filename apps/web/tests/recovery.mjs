import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";
const values = new Map();
globalThis.sessionStorage = { getItem: key => values.get(key) ?? null, setItem: (key,value) => values.set(key,value) };
const server = await createServer({root:fileURLToPath(new URL("../../..", import.meta.url)),configFile:false,server:{middlewareMode:true,hmr:false},ssr:{noExternal:["@vaultsyncd/api-client"]}});
try {
 const { RestoreSession, expiry, expired } = await server.ssrLoadModule("/apps/web/src/features/history/recovery.ts");
 const { BrowserAuth } = await server.ssrLoadModule("/apps/web/src/services/auth.ts");
 let reads=0, posts=0, original;
 const client = new BrowserAuth({base_url:"https://test.invalid",request_id:()=>"restore-test",fetch:async(url,init)=>{
   if(url.endsWith("/snapshots"))return Response.json({snapshot_id:"s",root_id:"root",server_epoch:"1",high_water_seq:"0",entry_count:"1",expires_at:"2000000000"});
   if(url.endsWith("/operations")){posts++;original=JSON.parse(init.body);throw new Error("response lost");}
   if(url.includes("/operations/")){
     reads++;
     if(reads===1)return Response.json({error:{code:"INVALID_REQUEST",message:"not found",request_id:"r",retryable:false}},{status:404});
     return Response.json(reads===2?{op_id:original.op_id,state:"pending"}:{op_id:original.op_id,state:"committed",entry_id:"entry"});
   }
   if(url.endsWith("/recovery"))return Response.json({items:[{id:"entry",entry_id:"entry",name:"actual-restored.txt",revision:"2"}]});
   throw new Error(url);
 }});
 const session = new RestoreSession("owner","root");
 await assert.rejects(session.begin(client,{entry_id:"entry",revision:"1"},false),/response lost/);
 const saved=session.read();
 assert.equal(saved.request.op_id,original.op_id);
 const pending=await new RestoreSession("owner","root").resume(client);
 assert.equal(pending.result.state,"pending"); assert.equal(pending.result.entry_id,undefined);
 await assert.rejects(session.begin(client,{entry_id:"entry",revision:"1"},false),/核对/);
 const committed=await session.resume(client);
 assert.equal(committed.path,"actual-restored.txt"); assert.equal(posts,1);
 assert.equal(committed.request.op_id,original.op_id);
 assert.equal(expired({}),false);assert.equal(expired({expires_at:"1"}),true);
 assert.match(expiry({}),/无年龄到期/);
 console.log("恢复会话：丢响应保留原Op、重建会话、pending独立、未知时不重复恢复、实际路径、到期通过");
} finally {await server.close();}
