import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

const server = await createServer({ root: fileURLToPath(new URL("../../..", import.meta.url)), configFile: false,
  server: { middlewareMode: true, hmr: false }, ssr: { noExternal: ["@vaultsyncd/api-client"] },
});
try {
  const { previewKind, officeFileType, downloadPreview, boundedCanvas, rasterDimensions, decodePreviewText, MAX_TEXT_PREVIEW_BYTES, MAX_PREVIEW_BYTES, MAX_CANVAS_PIXELS } = await server.ssrLoadModule("/apps/web/src/features/files/preview.ts");
  const { remarkPreviewBudget, MAX_MARKDOWN_NODES, MAX_MARKDOWN_CODE_BLOCKS, MAX_MARKDOWN_DEPTH, MARKDOWN_BUDGET_MESSAGE } =
    await server.ssrLoadModule("/apps/web/src/features/files/TextPreview.tsx");
  const transform = remarkPreviewBudget();
  const normal = { type: "root", children: [{ type: "code", lang: "js", value: "const x = 1;" }] };
  transform(normal); assert.equal(normal.children[0].type, "code");
  for (const tree of [
    { type: "root", children: Array.from({ length: MAX_MARKDOWN_CODE_BLOCKS + 1 }, () => ({ type: "code", lang: "js", value: "x" })) },
    { type: "root", children: Array.from({ length: MAX_MARKDOWN_NODES }, () => ({ type: "text", value: "x" })) },
  ]) {
    transform(tree); assert.equal(tree.children.length, 1);
    assert.equal(tree.children[0].children[0].value, MARKDOWN_BUDGET_MESSAGE);
  }
  const deep = { type: "root", children: [] };
  let branch = deep;
  for (let depth = 0; depth <= MAX_MARKDOWN_DEPTH; depth++) {
    const child = { type: "blockquote", children: [] }; branch.children.push(child); branch = child;
  }
  transform(deep); assert.equal(deep.children[0].children[0].value, MARKDOWN_BUDGET_MESSAGE);
  const data = Buffer.from("immutable content");
  const entry = { kind: "file", entry_id: "entry", name: "文档.PDF", revision: "3", version_id: "old-version", size: String(data.length), hash: createHash("sha256").update(data).digest("hex") };
  for (const name of ["a.pdf", "a.PNG", "a.jpg", "a.jpeg", "a.gif", "a.webp", "a.bmp", "a.avif"]) assert.ok(previewKind({ ...entry, name }));
  for (const extension of ["doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp"]) {
    assert.equal(previewKind({ ...entry, name: `中文 ${extension.toUpperCase()}.${extension.toUpperCase()}` }), "office");
    assert.equal(officeFileType(`文档.${extension.toUpperCase()}`), extension);
  }
  assert.equal(officeFileType("wrong.docx.exe"), null);
  for (const name of ["a.exe", "a.zip", "a.mp4", "a.woff2", "a.sqlite"])
    assert.equal(previewKind({ ...entry, name }), null);
  assert.equal(previewKind({ ...entry, kind: "directory" }), null);
  for (const name of ["a.svg", "a.html", "a.htm", "a.js", "a.tsx", "a.py", "a.rs", "a.txt", ".env", "Dockerfile", "a.custom", "__proto__", "constructor"])
    assert.equal(previewKind({ ...entry, name }), "text");
  assert.equal(previewKind({ ...entry, name: "README.MD" }), "markdown");
  assert.equal(await decodePreviewText(new Blob(["中文🙂\r\nconst n = 1;\t"])), "中文🙂\r\nconst n = 1;\t");
  assert.equal(await decodePreviewText(new Blob([])), "");
  assert.equal(await decodePreviewText(new Blob([Uint8Array.of(0xff, 0xfe, 0x2d, 0x4e)])), "中");
  assert.equal(await decodePreviewText(new Blob([Uint8Array.of(0xfe, 0xff, 0x4e, 0x2d)])), "中");
  for (const bytes of [Uint8Array.of(0xc3, 0x28), Uint8Array.of(0xff, 0xfe, 0x42), Uint8Array.of(0x61, 0, 0x62)])
    await assert.rejects(decodePreviewText(new Blob([bytes])));
  await assert.rejects(decodePreviewText(new Blob([new Uint8Array(MAX_TEXT_PREVIEW_BYTES + 1)])), /2 MiB/);
  const requests = [];
  const { BrowserAuth } = await server.ssrLoadModule("/apps/web/src/services/auth.ts");
  const client = new BrowserAuth({ base_url: "https://test.invalid", request_id: () => "preview-test", fetch: async (url, init) => {
    requests.push({ url, init }); assert.equal(init.credentials, "same-origin");
    return new Response(data, { status: 206, headers: { "content-range": `bytes 0-${data.length - 1}/${data.length}` } });
  } });
  for (const invalid of [{ size: undefined }, { size: "" }, { size: "-1" }, { size: "1.5" }, { size: "01" }, { size: "9".repeat(100) },
    { size: String(MAX_PREVIEW_BYTES + 1) }, { version_id: undefined }, { hash: undefined }, { hash: "bad" }, { name: "archive.zip" }, { name: "large.txt", size: String(MAX_TEXT_PREVIEW_BYTES + 1) }]) {
    await assert.rejects(downloadPreview(client, "vault", { ...entry, ...invalid }, new AbortController().signal, () => {}));
    assert.equal(requests.length, 0, "非法元数据/超限不得发网络请求");
  }
  const mutable = { ...entry };
  const pending = downloadPreview(client, "vault", mutable, new AbortController().signal, () => {});
  mutable.version_id = "new-head"; mutable.name = "changed";
  const result = await pending;
  assert.equal(result.name, entry.name); assert.equal(await result.blob.text(), data.toString());
  assert.match(requests[0].url, /\/versions\/old-version\/content$/);
  const canceled = new AbortController(); canceled.abort();
  await assert.rejects(downloadPreview(client, "vault", entry, canceled.signal, () => {}), { name: "AbortError" });
  assert.equal(requests.length, 1);
  for (const size of [[600, 800, 2], [1e10, 1e10, 2], [1, 1e10, 1]]) {
    const bound = boundedCanvas(...size);
    assert.ok(bound.width * bound.height <= MAX_CANVAS_PIXELS && Math.max(bound.width, bound.height) <= 8192);
  }
  for (const size of [[0, 1], [Infinity, 2], [-1, 1], [1, NaN]]) assert.throws(() => boundedCanvas(...size));
  const png = (width, height) => { const bytes = Buffer.alloc(24); bytes.set(Buffer.from("89504e470d0a1a0a0000000d49484452", "hex")); bytes.writeUInt32BE(width, 16); bytes.writeUInt32BE(height, 20); return bytes; };
  assert.deepEqual(rasterDimensions(png(2, 3), "png"), { width: 2, height: 3 });
  assert.throws(() => rasterDimensions(png(8193, 2), "png"), /超限/);
  assert.throws(() => rasterDimensions(png(8192, 8192), "png"), /超限/);
  for (const kind of ["png", "jpeg", "gif", "webp", "bmp", "avif"]) {
    assert.throws(() => rasterDimensions(Buffer.from('<svg onload="alert(1)"></svg>'), kind), /无效/);
    for (let length = 0; length < 24; length++) assert.throws(() => rasterDimensions(png(2, 3).subarray(0, length), kind));
  }
  const gif = Buffer.from("47494638396102000300", "hex");
  assert.deepEqual(rasterDimensions(gif, "gif"), { width: 2, height: 3 });
  const bmp = Buffer.alloc(54); bmp.write("BM"); bmp.writeUInt32LE(40, 14); bmp.writeInt32LE(2, 18); bmp.writeInt32LE(-3, 22);
  assert.deepEqual(rasterDimensions(bmp, "bmp"), { width: 2, height: 3 });
  const jpeg = Buffer.from("ffd8ffe000040000ffc000070800030002ffd9", "hex");
  assert.deepEqual(rasterDimensions(jpeg, "jpeg"), { width: 2, height: 3 });
  const webp = Buffer.alloc(30); webp.write("RIFF"); webp.write("WEBPVP8X", 8); webp[24] = 1; webp[27] = 2;
  assert.deepEqual(rasterDimensions(webp, "webp"), { width: 2, height: 3 });
  const box = (type, bytes) => { const header = Buffer.alloc(8); header.writeUInt32BE(8 + bytes.length); header.write(type, 4); return Buffer.concat([header, bytes]); };
  const ispe = Buffer.alloc(12); ispe.writeUInt32BE(2, 4); ispe.writeUInt32BE(3, 8);
  const avif = Buffer.concat([box("ftyp", Buffer.from("avif0000")), box("meta", Buffer.concat([Buffer.alloc(4), box("iprp", box("ipco", box("ispe", ispe)))]))]);
  assert.deepEqual(rasterDimensions(avif, "avif"), { width: 2, height: 3 });
  avif.writeUInt32BE(9999999, avif.length - 8); assert.throws(() => rasterDimensions(avif, "avif"), /超限/);

  const { downloadOfficePreview } = await server.ssrLoadModule("/apps/web/src/features/files/office-preview.ts");
  const pdf = Buffer.from("%PDF-1.7\nOffice converted immutable preview\n%%EOF");
  const pdfHash = createHash("sha256").update(pdf).digest("hex");
  const officeEntry = { ...entry, name: "中文.DOCX" };
  const officeRequests = [];
  const previewHeaders = {
    "content-type": "application/pdf", "content-length": String(pdf.length), "x-content-sha256": pdfHash,
    "x-preview-source-sha256": entry.hash, "x-preview-source-version": entry.version_id,
  };
  const makeClient = fetch => new BrowserAuth({ base_url: "https://test.invalid", request_id: () => "office-test", fetch });
  const officeClient = makeClient(async (url, init) => {
    officeRequests.push({ url, init });
    assert.equal(init.credentials, "same-origin");
    assert.equal(init.method, "POST");
    assert.equal(new Headers(init.headers).get("accept"), "application/pdf");
    return new Response(pdf, { headers: previewHeaders });
  });
  for (const invalid of [{ name: "wrong.docx.exe" }, { kind: "directory" }, { hash: "bad" }, { hash: undefined },
    { version_id: undefined }, { size: "01" }, { size: "-1" }, { size: "1.5" }, { size: String(MAX_PREVIEW_BYTES + 1) }]) {
    await assert.rejects(downloadOfficePreview(officeClient, "vault", { ...officeEntry, ...invalid }, new AbortController().signal));
    assert.equal(officeRequests.length, 0, "Office 非法元数据或输入超限在网络前拒绝");
  }
  const officeMutable = { ...officeEntry };
  const officePending = downloadOfficePreview(officeClient, "vault", officeMutable, new AbortController().signal);
  officeMutable.name = "changed.xlsx"; officeMutable.hash = "b".repeat(64); officeMutable.version_id = "new-head";
  const officeResult = await officePending;
  assert.equal(officeResult.type, "application/pdf");
  assert.equal(await officeResult.text(), pdf.toString());
  assert.match(officeRequests[0].url, /\/versions\/old-version\/office-preview$/);
  assert.deepEqual(JSON.parse(officeRequests[0].init.body), { file_type: "docx", hash: entry.hash, size: entry.size });
  await assert.rejects(downloadOfficePreview(officeClient, "vault", officeEntry, canceled.signal), { name: "AbortError" });
  assert.equal(officeRequests.length, 1);

  for (const [changes, message] of [
    [{ "content-type": "text/html" }, /不是 PDF/], [{ "content-type": null }, /不是 PDF/],
    [{ "x-preview-source-sha256": "c".repeat(64) }, /源版本或摘要不匹配/],
    [{ "x-preview-source-version": "new-head" }, /源版本或摘要不匹配/],
    [{ "content-length": null }, /大小无效/], [{ "content-length": "0" }, /大小无效/],
    [{ "content-length": "01" }, /大小无效/], [{ "content-length": String(MAX_PREVIEW_BYTES + 1) }, /超过 50 MiB/],
    [{ "x-content-sha256": null }, /缺少有效/], [{ "x-content-sha256": "not-a-hash" }, /缺少有效/],
  ]) {
    let bodyCanceled = false;
    const headers = new Headers(previewHeaders);
    for (const [key, value] of Object.entries(changes)) value === null ? headers.delete(key) : headers.set(key, value);
    const badClient = makeClient(async () => new Response(new ReadableStream({ cancel() { bodyCanceled = true; } }), { headers }));
    await assert.rejects(downloadOfficePreview(badClient, "vault", officeEntry, new AbortController().signal), message);
    assert.equal(bodyCanceled, true, "错误响应头须立即取消内容流");
  }
  for (const [bytes, headers, message] of [
    [pdf.subarray(1), previewHeaders, /长度不匹配/],
    [Buffer.concat([pdf, Buffer.from("extra")]), previewHeaders, /超出声明长度/],
    [Buffer.from(pdf.toString().replace("Office", "office")), previewHeaders, /SHA-256/],
    [Buffer.from("<!doctype html>"), { ...previewHeaders, "content-length": "15", "x-content-sha256": createHash("sha256").update("<!doctype html>").digest("hex") }, /不是有效的 PDF/],
  ]) {
    const badClient = makeClient(async () => new Response(bytes, { headers }));
    await assert.rejects(downloadOfficePreview(badClient, "vault", officeEntry, new AbortController().signal), message);
  }
  const noBody = makeClient(async () => new Response(null, { headers: previewHeaders }));
  await assert.rejects(downloadOfficePreview(noBody, "vault", officeEntry, new AbortController().signal), /缺少内容流/);
  const partial = makeClient(async () => new Response(pdf, { status: 206, headers: previewHeaders }));
  await assert.rejects(downloadOfficePreview(partial, "vault", officeEntry, new AbortController().signal), /不是 PDF/);
  const unavailable = makeClient(async () => Response.json({ error: { code: "SERVICE_NOT_READY", message: "尚未配置 Office 预览，请下载原文件。", request_id: "office-test", retryable: true } }, { status: 503 }));
  await assert.rejects(downloadOfficePreview(unavailable, "vault", officeEntry, new AbortController().signal), /尚未配置 Office/);

  // fetch 不响应取消时，迟到响应仍必须丢弃并关闭其内容流。
  let releaseLate, lateCanceled = false;
  const lateResponse = new Promise(resolve => { releaseLate = resolve; });
  const lateAbort = new AbortController();
  const lateClient = makeClient(async () => lateResponse);
  const lateOffice = downloadOfficePreview(lateClient, "vault", officeEntry, lateAbort.signal);
  const lateRejected = assert.rejects(lateOffice, { name: "AbortError" });
  lateAbort.abort();
  releaseLate(new Response(new ReadableStream({ cancel() { lateCanceled = true; } }), { headers: previewHeaders }));
  await lateRejected; assert.equal(lateCanceled, true);

  // 读取中的取消必须主动关闭 reader；不能等待永不完成的伪造内容流。
  const readingAbort = new AbortController();
  let readingCanceled = false, readingStarted;
  const started = new Promise(resolve => { readingStarted = resolve; });
  const blocked = makeClient(async () => new Response(new ReadableStream({
    cancel() { readingCanceled = true; },
  }), { headers: previewHeaders }));
  const readingOffice = downloadOfficePreview(blocked, "vault", officeEntry, readingAbort.signal, () => readingStarted());
  const readingRejected = assert.rejects(readingOffice, { name: "AbortError" });
  await started; readingAbort.abort(); await readingRejected; assert.equal(readingCanceled, true);

  let releaseSession;
  const sessionResponse = new Promise(resolve => { releaseSession = resolve; });
  const changing = makeClient(async url => url.endsWith("/logout")
    ? Response.json({ session_id: "session", revoked_at: "1000" }) : sessionResponse);
  const oldOffice = downloadOfficePreview(changing, "vault", officeEntry, new AbortController().signal);
  const oldRejected = assert.rejects(oldOffice, /会话已变更/);
  await changing.logout(); releaseSession(new Response(pdf, { headers: previewHeaders })); await oldRejected;

  const { createPreviewRenderer } = await server.ssrLoadModule("/apps/web/src/features/files/preview-renderer.ts");
  let bitmapClosed = 0;
  globalThis.createImageBitmap = async () => ({ width: 2, height: 3, close: () => { bitmapClosed++; } });
  const raster = await createPreviewRenderer(new Blob([png(2, 3)]), "png", new AbortController().signal);
  let drawn = false;
  await raster.render({ getContext: () => ({ drawImage: () => { drawn = true; } }) }, new AbortController().signal);
  assert.ok(drawn); await raster.destroy(); assert.equal(bitmapClosed, 1);
  const decoding = new AbortController();
  globalThis.createImageBitmap = async () => { decoding.abort(); return { width: 2, height: 3, close: () => { bitmapClosed++; } }; };
  await assert.rejects(createPreviewRenderer(new Blob([png(2, 3)]), "png", decoding.signal), { name: "AbortError" });
  assert.equal(bitmapClosed, 2, "迟到的原生解码结果必须关闭");
  console.log("在线预览：Office 类型/固定版本转换、PDF 有界流/源身份/摘要/取消校验、文本分类/严格编码/2MiB 与图片PDF50MiB 网络前门禁、固定版本校验、栅格签名/尺寸、画布预算、取消/迟到结果与资源释放均通过");
} finally { await server.close(); }
