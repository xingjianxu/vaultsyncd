import assert from "node:assert/strict";
import { File } from "node:buffer";
import { createHash, webcrypto } from "node:crypto";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

globalThis.crypto ??= webcrypto;
const server = await createServer({ root: fileURLToPath(new URL("../../..", import.meta.url)), configFile: false,
  server: { middlewareMode: true, hmr: false }, ssr: { noExternal: ["@vaultsyncd/api-client"] } });
try {
  const { createClipboard, createUploadJob, createPasteJob, createRenameJob, prepareDelete, createFileActionRecovery, MAX_FILE_ACTION_BYTES, MAX_UPLOAD_BYTES } = await server.ssrLoadModule("/apps/web/src/features/files/actions.ts");
  const { BrowserAuth, SignedOut } = await server.ssrLoadModule("/apps/web/src/services/auth.ts");
  const hash = bytes => createHash("sha256").update(bytes).digest("hex");
  const directory = (entry_id, parent_id, name) => ({ entry_id, parent_id, name, kind: "directory", revision: "1" });
  const file = (entry_id, parent_id, name, bytes = "abcdef") => ({ entry_id, parent_id, name, kind: "file", revision: "4", version_id: `v-${entry_id}`, size: String(Buffer.byteLength(bytes)), hash: hash(bytes) });
  function snapshot(vault = "vault", items = [directory("r", undefined, ""), directory("d", "r", "folder"), file("f", "d", "a.txt"), directory("empty", "d", "empty")]) {
    const entries = new Map(items.map(entry => [entry.entry_id, entry]));
    const children = new Map();
    for (const entry of items) if (entry.parent_id) {
      const siblings = children.get(entry.parent_id) ?? []; siblings.push(entry); children.set(entry.parent_id, siblings);
    }
    return { metadata: { vault_id: vault, snapshot_id: "snapshot", server_epoch: "1", high_water_seq: "9", entry_count: String(items.length), expires_at: String(Math.floor(Date.now() / 1000) + 3600) }, vault: items[0], entries, children };
  }
  const error = (status, code, message = code) => Response.json({ error: { code, message, retryable: false, request_id: "test" } }, { status });
  function transport(source = snapshot(), config = {}) {
    const uploads = new Map(), operations = new Map(), submissions = [], queries = [], calls = [], downloads = [];
    let uploadCount = 0;
    const client = new BrowserAuth({ base_url: "https://test.invalid", request_id: () => "actions-test", fetch: async (url, init) => {
      assert.equal(init.credentials, "same-origin");
      const path = new URL(url).pathname.split("/").filter(Boolean), vault = path[2];
      calls.push([init.method, path.join("/")]);
      if (config.intercept) { const result = await config.intercept({ path, init, uploads, operations, submissions }); if (result) return result; }
      if (path[3] === "snapshots") {
        const current = config.current ?? source;
        if (init.method === "POST") return Response.json(current.metadata);
        return Response.json({ snapshot_id: current.metadata.snapshot_id, high_water_seq: current.metadata.high_water_seq, entries: [...current.entries.values()] });
      }
      if (path[3] === "entries" && path[5] === "subtree") return Response.json({ vault_id: vault, entry_id: path[4], entries: String(config.subtreeCount ?? 3), token: "frozen-token" });
      if (path[3] === "entries" && path[5] === "versions") {
        downloads.push({ vault, entry: path[4], version: path[6] });
        if (config.versionGone) return error(410, "VERSION_GONE");
        return new Response(config.download ?? "abcdef");
      }
      if (path[3] === "uploads") {
        if (path.length === 4) {
          const body = JSON.parse(init.body), id = `upload-${++uploadCount}`;
          const upload = { ...body, upload_id: id, vault_id: vault, chunk_size: "2", part_count: String(Math.ceil(Number(body.total_size) / 2)), expires_at: "9999999999", state: body.total_size === "0" ? "complete" : "open" };
          uploads.set(id, { upload, parts: new Map(), puts: [] });
          return Response.json(upload, { status: 201 });
        }
        const record = uploads.get(path[4]); assert.ok(record);
        if (path[5] === "parts") {
          const bytes = Buffer.from(init.body), digest = new Headers(init.headers).get("x-part-sha256");
          assert.equal(digest, hash(bytes));
          record.puts.push(path[6]); record.parts.set(path[6], bytes);
          if (record.parts.size === Number(record.upload.part_count)) record.upload.state = "complete";
          if (config.losePart && !config.partLost) { config.partLost = true; throw new Error("lost part response"); }
          return Response.json({ upload_id: path[4], index: path[6], hash: digest, length: String(bytes.length), state: record.upload.state, already_present: false });
        }
        if (path[5] === "finalize") {
          const bytes = Buffer.concat([...record.parts.values()]);
          assert.equal(hash(bytes), record.upload.hash); assert.equal(String(bytes.length), record.upload.total_size);
          record.upload.state = "finalized";
          if (config.loseFinalize && !config.finalizeLost) { config.finalizeLost = true; throw new Error("lost finalize response"); }
          return Response.json({ upload_id: path[4], vault_id: vault, hash: record.upload.hash, size: record.upload.total_size, part_count: record.upload.part_count, replayed: false, reuse: "freshly_published", storage_path: "opaque" });
        }
        return Response.json({ upload: record.upload, received: [...record.parts].map(([index, bytes]) => ({ index, hash: hash(bytes), length: String(bytes.length) })), missing: [] });
      }
      if (path[3] === "operations") {
        if (init.method === "GET") { queries.push(path[4]); return operations.has(path[4]) ? Response.json(operations.get(path[4])) : error(404, "INVALID_REQUEST"); }
        const request = JSON.parse(init.body); submissions.push({ vault, request });
        if (request.upload_id) assert.equal(uploads.get(request.upload_id)?.upload.state, "finalized", "I05: create 必须在 finalize 后");
        if (config.beforeSubmit) await config.beforeSubmit(request, submissions.length);
        const result = config.result?.(request, submissions.length) ?? { state: "committed", op_id: request.op_id, entry_id: request.entry_id ?? `created-${submissions.length}`, revision: "5", vault_seq: "10" };
        operations.set(request.op_id, result);
        if (config.loseOperation && !config.operationLost) { config.operationLost = true; if (config.notRecorded) operations.delete(request.op_id); throw new Error("lost operation response"); }
        return Response.json(result);
      }
      throw new Error(`unexpected ${init.method} ${url}`);
    } });
    return { client, uploads, operations, submissions, queries, calls, downloads };
  }

  // I05/I06: 多文件、分块摘要、空文件 0 块也 finalize，提交前不可见；重复 run 不重复创建。
  {
    const target = snapshot("target", [directory("tr", undefined, "")]);
    const api = transport();
    const job = createUploadJob(api.client, "target", target, target.vault, [new File(["abcdef"], "a.txt"), new File([], "empty.txt")]);
    assert.equal(job.state, "ready");
    await Promise.all([job.run(), job.run()]);
    assert.equal(job.state, "completed"); assert.equal(job.progress.completed, 2);
    assert.equal(api.uploads.size, 2); assert.equal(api.submissions.length, 2);
    assert.equal(api.uploads.get("upload-1").puts.length, 3); assert.equal(api.uploads.get("upload-2").puts.length, 0);
    for (const { request } of api.submissions) {
      assert.equal(request.kind, "create"); assert.equal(request.expected_absent, true);
      assert.equal(api.uploads.get(request.upload_id).upload.state, "finalized");
      assert.match(request.device_id, /^[a-f0-9]{32}$/); assert.match(request.op_id, /^[a-f0-9]{32}$/);
    }
    await job.run(); assert.equal(api.submissions.length, 2);
  }
  // I06/I13: 丢操作响应先查询，只有明确 404 才同 OpId 同请求重放。
  for (const notRecorded of [false, true]) {
    const target = snapshot("target", [directory("tr", undefined, "")]);
    const api = transport(undefined, { loseOperation: true, notRecorded });
    const job = createUploadJob(api.client, "target", target, target.vault, [new File(["abcdef"], "a.txt")]);
    await assert.rejects(job.run(), /lost operation/); assert.equal(job.state, "failed"); assert.equal(job.canDiscard, false);
    await job.run(); assert.equal(job.state, "completed");
    assert.equal(api.queries.length, 1); assert.equal(api.uploads.size, 1);
    assert.equal(api.submissions.length, notRecorded ? 2 : 1);
    if (notRecorded) assert.deepEqual(api.submissions[0], api.submissions[1]);
  }
  // 响应丢失后从服务端收到块位图续传，finalize 丢响应不能引发第二次 create。
  for (const config of [{ losePart: true }, { loseFinalize: true }]) {
    const target = snapshot("target", [directory("tr", undefined, "")]); const api = transport(undefined, config);
    const job = createUploadJob(api.client, "target", target, target.vault, [new File(["abcdef"], "a.txt")]);
    await assert.rejects(job.run(), /lost (part|finalize)/); await job.run();
    assert.equal(job.state, "completed"); assert.equal(api.uploads.size, 1); assert.equal(api.submissions.length, 1);
    assert.deepEqual(api.uploads.get("upload-1").puts, ["0", "1", "2"]);
  }
  // I01: pending 不增加已提交数，不发后续 create；继续只查询，committed 后才创建子项。
  {
    const source = snapshot(), target = snapshot("target", [directory("tr", undefined, "")]);
    const api = transport(source, { result: (request, count) => count === 1 ? { state: "pending", op_id: request.op_id } : { state: "committed", op_id: request.op_id, entry_id: `new-${count}` } });
    const clipboard = createClipboard("copy", "vault", source, source.entries.get("d"));
    const job = createPasteJob(api.client, clipboard, "target", target, target.vault);
    await job.run(); assert.equal(job.state, "pending"); assert.equal(job.progress.completed, 0); assert.equal(job.canDiscard, false);
    assert.match(job.summary, /仍在处理/); assert.equal(api.submissions.length, 1); assert.equal(api.downloads.length, 0);
    await job.run(); assert.equal(job.state, "pending"); assert.equal(api.submissions.length, 1);
    const request = api.submissions[0].request;
    api.operations.set(request.op_id, { op_id: request.op_id, state: "committed", entry_id: "new-directory" });
    await job.run(); assert.equal(job.state, "completed"); assert.equal(api.submissions.length, 3);
    assert.equal(api.submissions[1].request.parent_id, "new-directory");
    assert.equal(api.submissions[2].request.parent_id, "new-directory");
    assert.equal(api.submissions[2].request.upload_id, undefined); // 保留空目录
    assert.equal(api.submissions[1].vault, "target"); assert.equal(api.downloads[0].vault, "vault");
  }
  // 多层递归复制必须使用每个已 committed 的新父目录身份，不能引用源父目录。
  {
    const source = snapshot("vault", [directory("r", undefined, ""), directory("d", "r", "folder"), directory("nested", "d", "nested"), file("f", "nested", "a.txt")]);
    const target = snapshot("target", [directory("tr", undefined, "")]), api = transport(source);
    const job = createPasteJob(api.client, createClipboard("copy", "vault", source, source.entries.get("d")), "target", target, target.vault);
    await job.run();
    assert.equal(job.state, "completed");
    assert.deepEqual(api.submissions.map(item => item.request.parent_id), ["tr", "created-1", "created-2"]);
  }
  // I12: 剪贴板冻结原 VersionId；源随后更新也不下载新 head；不存在时绝不回退。
  {
    const source = snapshot(), target = snapshot("target", [directory("tr", undefined, "")]);
    const clipboard = createClipboard("copy", "vault", source, source.entries.get("f"));
    source.entries.get("f").version_id = "new-version"; source.entries.get("f").name = "renamed";
    const api = transport(source), job = createPasteJob(api.client, clipboard, "target", target, target.vault);
    await job.run(); assert.equal(api.downloads[0].version, "v-f"); assert.equal(api.submissions[0].request.name, "a.txt");
    for (const config of [{ versionGone: true }, { download: "corrupt" }, { download: "abcdeg" }, { download: "abc" }]) {
      const failed = transport(source, config); const next = createPasteJob(failed.client, clipboard, "target", target, target.vault);
      await assert.rejects(next.run()); assert.equal(next.state, "failed"); assert.equal(failed.submissions.length, 0); assert.equal(failed.uploads.size, 0);
    }
  }
  // I03/I04: 同 Vault 移动保 EntryId + revision CAS；跨 Vault 剪切明确禁止；同名永不覆盖。
  {
    const source = snapshot(), api = transport(source);
    const clip = createClipboard("cut", "vault", source, source.entries.get("f"));
    const job = createPasteJob(api.client, clip, "vault", source, source.vault); await job.run();
    const request = api.submissions[0].request;
    assert.equal(request.kind, "move"); assert.equal(request.entry_id, "f"); assert.equal(request.base_revision, "4"); assert.equal(request.base_version_id, "v-f");
    const other = snapshot("target", [directory("tr", undefined, "")]);
    assert.throws(() => createPasteJob(api.client, clip, "target", other, other.vault), /跨 Vault 剪切/);
    assert.throws(() => createPasteJob(api.client, clip, "vault", source, source.entries.get("d")), /同名/);
    assert.throws(() => createUploadJob(api.client, "vault", source, source.entries.get("d"), [new File([], "a.txt")]), /同名/);
    assert.throws(() => createUploadJob(api.client, "vault", source, source.vault, [new File([], "x"), new File([], "x")]), /同名/);
    assert.throws(() => createPasteJob(api.client, createClipboard("cut", "vault", source, source.entries.get("d")), "vault", source, source.entries.get("empty")), /自身或后代/);
    assert.throws(() => createClipboard("copy", "vault", source, source.vault), /根目录/);
    assert.throws(() => createUploadJob(api.client, "vault", source, source.vault, [{ name: "large", size: MAX_UPLOAD_BYTES + 1 }]), /64 GiB/);
    assert.throws(() => createPasteJob(api.client, createClipboard("copy", "vault", source, source.entries.get("d")), "vault", source, source.entries.get("empty")), /自身或后代/);
  }
  // S137 I03/I04: 重命名冻结原父目录、身份和版本，以源 revision CAS，不读取或上传内容。
  for (const id of ["f", "d"]) {
    const source = snapshot(), selected = source.entries.get(id), api = transport(source);
    const original = { ...selected }, job = createRenameJob(api.client, "vault", source, selected, "新名称 % & 文档");
    selected.revision = "99"; selected.name = "并发变化"; selected.parent_id = "empty";
    await Promise.all([job.run(), job.run()]);
    assert.equal(job.state, "completed"); assert.equal(job.label, "重命名");
    const { request } = api.submissions[0];
    assert.equal(request.kind, "move"); assert.equal(request.entry_id, original.entry_id);
    assert.equal(request.parent_id, original.parent_id); assert.equal(request.base_revision, original.revision);
    assert.equal(request.base_version_id, original.version_id); assert.equal(request.name, "新名称 % & 文档");
    assert.equal(api.calls.length, 1); assert.equal(api.downloads.length, 0); assert.equal(api.uploads.size, 0);
    await job.run(); assert.equal(api.submissions.length, 1);
  }
  {
    const source = snapshot(), api = transport(source), selected = source.entries.get("f");
    for (const name of ["", ".", "..", "a/b", "a\\b", "bad\0name"]) assert.throws(() => createRenameJob(api.client, "vault", source, selected, name), /无效/);
    assert.throws(() => createRenameJob(api.client, "vault", source, selected, "a.txt"), /未改变/);
    assert.throws(() => createRenameJob(api.client, "vault", source, selected, "empty"), /同名/);
    assert.throws(() => createRenameJob(api.client, "vault", source, source.vault, "renamed"), /根目录/);
    assert.throws(() => createRenameJob(api.client, "other", source, selected, "renamed"), /不匹配/);
    assert.throws(() => createRenameJob(api.client, "vault", source, { ...selected, entry_id: "missing" }, "renamed"), /消失/);
    assert.equal(api.calls.length, 0);
    for (const code of ["BASE_MISMATCH", "INVALID_REQUEST", "INVALID_NAME"]) {
      const rejected = transport(source, { result: request => ({ state: "rejected", op_id: request.op_id, error_code: code }) });
      const job = createRenameJob(rejected.client, "vault", source, selected, "new.txt");
      await assert.rejects(job.run(), new RegExp(code)); assert.equal(job.canDiscard, true);
      await assert.rejects(job.run(), new RegExp(code)); assert.equal(rejected.submissions.length, 1);
    }
  }
  // S137 I01/I06/I13: pending 只查原 OpId；丢响应和404重放均保留同一个重命名请求与恢复记录。
  {
    const source = snapshot(), api = transport(source, { result: request => ({ state: "pending", op_id: request.op_id }) });
    const job = createRenameJob(api.client, "vault", source, source.entries.get("f"), "new.txt");
    await job.run(); assert.equal(job.state, "pending"); assert.equal(job.progress.completed, 0); assert.equal(job.canDiscard, false);
    await job.run(); assert.equal(api.submissions.length, 1);
    const original = api.submissions[0].request;
    api.operations.set(original.op_id, { state: "committed", op_id: original.op_id });
    await job.run(); assert.equal(job.state, "completed"); assert.equal(api.submissions.length, 1);
  }
  for (const notRecorded of [false, true]) {
    const source = snapshot(), values = new Map();
    const storage = { getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value), removeItem: key => values.delete(key) };
    const recovery = createFileActionRecovery("rename-account", storage), api = transport(source, { loseOperation: true, notRecorded });
    const job = createRenameJob(api.client, "vault", source, source.entries.get("f"), "new.txt", recovery);
    await assert.rejects(job.run(), /lost operation response/); assert.equal(job.canDiscard, false);
    const original = api.submissions[0].request;
    assert.deepEqual(recovery.list()[0].request, original);
    const query = await recovery.query(api.client); assert.equal(query[0].state, notRecorded ? "missing" : "committed");
    assert.equal(api.submissions.length, 1);
    if (notRecorded) { await recovery.resume(api.client, original.op_id); assert.deepEqual(api.submissions[1].request, original); }
    assert.equal(recovery.list().length, 0);
  }
  // 部分完成保留且显式报告；服务器同名/CAS 拒绝不会自动改名、覆盖、回滚或创建后续项。
  {
    const source = snapshot(), target = snapshot("target", [directory("tr", undefined, "")]);
    const api = transport(source, { result: (request, count) => count === 2 ? { state: "rejected", op_id: request.op_id, error_code: "BASE_MISMATCH", error_message: "目标并发变化" } : { state: "committed", op_id: request.op_id, entry_id: "new-dir" } });
    const job = createPasteJob(api.client, createClipboard("copy", "vault", source, source.entries.get("d")), "target", target, target.vault);
    await assert.rejects(job.run(), /目标并发变化/); assert.equal(job.progress.completed, 1); assert.match(job.summary, /1\/3.*保留/); assert.equal(job.canDiscard, true);
    await assert.rejects(job.run(), /目标并发变化/); assert.equal(api.submissions.length, 2);
    assert.ok(api.submissions.every(item => item.request.kind === "create" && item.request.expected_absent));
  }
  // I04: 目录 token 在确认之前冻结；确认后不刷新 token，过期/子树变化拒绝；回收站用 delete。
  {
    const source = snapshot(), api = transport(source);
    const prepared = await prepareDelete(api.client, "vault", source, source.entries.get("d"));
    assert.equal(api.submissions.length, 0); assert.equal(prepared.count, 3);
    const job = prepared.createJob(); assert.equal(job, prepared.createJob());
    await job.run();
    assert.equal(api.submissions[0].request.kind, "delete"); assert.equal(api.submissions[0].request.subtree_token, "frozen-token");
    assert.equal(api.submissions[0].request.base_revision, "1");
    assert.equal(api.calls.filter(([, path]) => path.endsWith("subtree")).length, 1);
    const originalNow = Date.now;
    try {
      const next = await prepareDelete(api.client, "vault", source, source.entries.get("f"));
      Date.now = () => next.expiresAt;
      await assert.rejects(next.createJob().run(), /确认已过期/);
      assert.equal(api.submissions.length, 1);
    } finally { Date.now = originalNow; }
    const current = snapshot(); current.entries.get("f").revision = "5";
    const changed = transport(source, { current });
    await assert.rejects(prepareDelete(changed.client, "vault", source, source.entries.get("d")), /子树已变化/);
    assert.equal(changed.submissions.length, 0);
    const changedAfter = transport(source, { result: request => ({ op_id: request.op_id, state: "rejected", error_code: "SUBTREE_CHANGED", error_message: "子树已变化" }) });
    const next = await prepareDelete(changedAfter.client, "vault", source, source.entries.get("d"));
    await assert.rejects(next.createJob().run(), /子树已变化/);
    assert.equal(changedAfter.calls.filter(([, path]) => path.endsWith("subtree")).length, 1);
  }
  // 删除请求丢响应后，即使确认窗口过期也允许查询原结果，不能重新扩大范围。
  {
    const source = snapshot(), api = transport(source, { loseOperation: true });
    const prepared = await prepareDelete(api.client, "vault", source, source.entries.get("f")); const job = prepared.createJob();
    await assert.rejects(job.run(), /lost operation/);
    const originalNow = Date.now;
    try { Date.now = () => prepared.expiresAt + 1; await job.run(); assert.equal(job.state, "completed"); }
    finally { Date.now = originalNow; }
    assert.equal(api.submissions.length, 1); assert.equal(api.queries.length, 1);
  }
  // 404不是取消确认：未知delete即使确认已到期，仍须保留原Op等待迟到提交。
  {
    const values = new Map(), storage = { getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value), removeItem: key => values.delete(key) };
    const source = snapshot(), recovery = createFileActionRecovery("account-expired", storage);
    const api = transport(source, { loseOperation: true, notRecorded: true });
    const prepared = await prepareDelete(api.client, "vault", source, source.entries.get("f"), recovery), job = prepared.createJob();
    await assert.rejects(job.run(), /lost operation/);
    const original = recovery.list()[0], originalNow = Date.now;
    try {
      Date.now = () => prepared.expiresAt + 1;
      await assert.rejects(job.run(), /已过期/);
      assert.equal(job.canDiscard, false); assert.deepEqual(recovery.list(), [original]);
      assert.equal(api.submissions.length, 1, "过期不重发，也不抹掉仍可能在途的原提交");
      api.operations.set(original.request.op_id, { state: "committed", op_id: original.request.op_id, entry_id: "f", revision: "5", vault_seq: "10" });
      await job.run(); assert.equal(job.state, "completed"); assert.equal(recovery.list().length, 0);
    } finally { Date.now = originalNow; }
  }
  // 恢复重放的认证门禁/4xx只证明本次未提交，不得抹掉更早的未知POST。
  for (const status of ["paused", 409]) {
    const values = new Map(), storage = { getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value), removeItem: key => values.delete(key) };
    const recovery = createFileActionRecovery(`account-${status}`, storage), source = snapshot();
    let replaying = false;
    const api = transport(source, { loseOperation: true, notRecorded: true, intercept: ({ path, init }) => {
      if (replaying && path[3] === "operations" && init.method === "POST") {
        if (status === "paused") throw new SignedOut();
        return error(status, "BASE_MISMATCH");
      }
    } });
    const job = createPasteJob(api.client, createClipboard("cut", "vault", source, source.entries.get("f")), "vault", source, source.vault, recovery);
    await assert.rejects(job.run()); const original = recovery.list()[0]; replaying = true;
    await assert.rejects(recovery.resume(api.client, original.request.op_id));
    assert.deepEqual(recovery.list(), [original]); assert.equal(job.canDiscard, false);
    await assert.rejects(job.run()); assert.equal(job.canDiscard, false); assert.deepEqual(recovery.list(), [original]);
  }
  // 上传超过复制上限仍按 1 MiB 分片增量摘要，禁止读取整文件；服务端64GiB边界保留。
  {
    const source = snapshot(), size = MAX_FILE_ACTION_BYTES + 1, slices = [];
    const virtualFile = { name: "large.bin", size,
      arrayBuffer() { throw new Error("must not buffer whole upload"); },
      slice(start, end) { const length = Math.min(size, end) - start; slices.push(length); return new Blob([new Uint8Array(length)]); } };
    let body;
    const api = transport(source, { intercept: ({ path, init }) => {
      if (path[3] === "uploads" && path.length === 4) { body = JSON.parse(init.body); return error(507, "INSUFFICIENT_STORAGE"); }
    } });
    const job = createUploadJob(api.client, "vault", source, source.vault, [virtualFile]);
    await assert.rejects(job.run(), /INSUFFICIENT_STORAGE/); assert.equal(job.canDiscard, true);
    assert.equal(body.total_size, String(size)); assert.ok(slices.length > 1); assert.ok(slices.every(length => length <= 1024 * 1024));
    const digest = createHash("sha256"); for (const length of slices) digest.update(Buffer.alloc(length));
    assert.equal(body.hash, digest.digest("hex"));
  }
  // 确定性4xx可停止；5xx/网络未知必须保留任务；上传会话失效尚未提交命名空间时可停止。
  for (const [status, canDiscard] of [[409, true], [422, true], [503, false]]) {
    const source = snapshot();
    const api = transport(source, { intercept: ({ path, init }) => path[3] === "operations" && init.method === "POST" ? error(status, "BASE_MISMATCH") : undefined });
    const job = createPasteJob(api.client, createClipboard("cut", "vault", source, source.entries.get("f")), "vault", source, source.vault);
    await assert.rejects(job.run()); assert.equal(job.canDiscard, canDiscard);
  }
  {
    const source = snapshot(), api = transport(source, { intercept: ({ path }) => path[3] === "uploads" && path.length === 5 ? error(410, "CURSOR_EXPIRED") : undefined });
    const job = createUploadJob(api.client, "vault", source, source.vault, [new File(["abc"], "upload.txt")]);
    await assert.rejects(job.run()); assert.equal(job.canDiscard, true); assert.equal(api.submissions.length, 0);
  }
  // I06/I13: submit前持久化最小请求；按账号隔离；刷新后只查询，显式继续才重放原请求。
  {
    const values = new Map();
    const storage = { getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value), removeItem: key => values.delete(key) };
    const recovery = createFileActionRecovery("account-A", storage), source = snapshot();
    const api = transport(source, { loseOperation: true, notRecorded: true, beforeSubmit: request => {
      assert.deepEqual(recovery.list()[0].request, JSON.parse(JSON.stringify(request)), "提交前必须保存原请求");
    } });
    const job = createPasteJob(api.client, createClipboard("cut", "vault", source, source.entries.get("f")), "vault", source, source.vault, recovery);
    await assert.rejects(job.run(), /lost operation/);
    assert.equal(job.canDiscard, false); assert.equal(recovery.list().length, 1);
    assert.deepEqual(createFileActionRecovery("account-B", storage).list(), []);
    assert.throws(() => createUploadJob(api.client, "vault", source, source.vault, [new File([], "new")], recovery), /未确认/);
    const reloaded = createFileActionRecovery("account-A", storage);
    const queried = await reloaded.query(api.client); assert.equal(queried[0].state, "missing");
    assert.equal(api.submissions.length, 1); assert.equal(reloaded.list().length, 1);
    const old = reloaded.list()[0];
    const resumed = await reloaded.resume(api.client, old.request.op_id);
    assert.equal(resumed.state, "committed"); assert.deepEqual(api.submissions[0], api.submissions[1]); assert.deepEqual(reloaded.list(), []);
    assert.equal(values.size, 0);
  }
  // 已提交但丢响应的删除，恢复查询不发第二个 delete；pending journal 保留，稳定拒绝后清除。
  for (const pending of [false, true]) {
    const values = new Map(), storage = { getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value), removeItem: key => values.delete(key) };
    const recovery = createFileActionRecovery("account", storage), source = snapshot();
    const api = transport(source, { loseOperation: true, result: pending ? request => ({ state: "pending", op_id: request.op_id }) : undefined });
    const prepared = await prepareDelete(api.client, "vault", source, source.entries.get("f"), recovery);
    await assert.rejects(prepared.createJob().run());
    const result = await recovery.query(api.client); assert.equal(result[0].state, pending ? "pending" : "committed");
    assert.equal(api.submissions.length, 1); assert.equal(recovery.list().length, pending ? 1 : 0);
    if (pending) {
      const record = recovery.list()[0]; api.operations.set(record.request.op_id, { state: "rejected", op_id: record.request.op_id, error_code: "BASE_MISMATCH" });
      assert.equal((await recovery.query(api.client))[0].state, "rejected"); assert.equal(recovery.list().length, 0);
    }
  }
  // sessionStorage 不可写时禁止提交，不能为了可用性丢掉恢复日志。
  {
    const storage = { getItem: () => null, setItem: () => { throw new Error("storage unavailable"); }, removeItem() {} };
    const recovery = createFileActionRecovery("account", storage), source = snapshot(), api = transport(source);
    const job = createPasteJob(api.client, createClipboard("cut", "vault", source, source.entries.get("f")), "vault", source, source.vault, recovery);
    await assert.rejects(job.run(), /storage unavailable/); assert.equal(api.submissions.length, 0); assert.equal(job.canDiscard, true);
  }
  console.log("文件操作：I01/I03/I04/I05/I06/I12/I13，多文件与空文件、增量SHA256有界上传、固定版本跨Vault复制、CAS/token、pending/丢响应续作、账号隔离恢复日志、确定性失败可停止均通过");
} finally { await server.close(); }
