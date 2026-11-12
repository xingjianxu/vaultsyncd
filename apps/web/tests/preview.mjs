import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

const server = await createServer({ root: fileURLToPath(new URL("../../..", import.meta.url)), configFile: false,
  server: { middlewareMode: true, hmr: false }, ssr: { noExternal: ["@vaultsyncd/api-client"] },
});
try {
  const { previewKind, downloadPreview, boundedCanvas, rasterDimensions, MAX_PREVIEW_BYTES, MAX_CANVAS_PIXELS } = await server.ssrLoadModule("/apps/web/src/features/files/preview.ts");
  const data = Buffer.from("immutable content");
  const entry = { kind: "file", entry_id: "entry", name: "文档.PDF", revision: "3", version_id: "old-version", size: String(data.length), hash: createHash("sha256").update(data).digest("hex") };
  for (const name of ["a.pdf", "a.PNG", "a.jpg", "a.jpeg", "a.gif", "a.webp", "a.bmp", "a.avif"]) assert.ok(previewKind({ ...entry, name }));
  for (const name of ["a.svg", "a.html", "a.htm", "a.js", "a.pdf.html", "a.txt", "no-extension", "__proto__", "constructor"])
    assert.equal(previewKind({ ...entry, name }), null);
  assert.equal(previewKind({ ...entry, kind: "directory" }), null);
  const requests = [];
  const { BrowserAuth } = await server.ssrLoadModule("/apps/web/src/services/auth.ts");
  const client = new BrowserAuth({ base_url: "https://test.invalid", request_id: () => "preview-test", fetch: async (url, init) => {
    requests.push({ url, init }); assert.equal(init.credentials, "same-origin");
    return new Response(data, { status: 206, headers: { "content-range": `bytes 0-${data.length - 1}/${data.length}` } });
  } });
  for (const invalid of [{ size: undefined }, { size: "" }, { size: "-1" }, { size: "1.5" }, { size: "01" }, { size: "9".repeat(100) },
    { size: String(MAX_PREVIEW_BYTES + 1) }, { version_id: undefined }, { hash: undefined }, { hash: "bad" }, { name: "script.svg" }]) {
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
  console.log("在线预览：白名单、50MiB 网络前门禁、固定版本校验、栅格签名/尺寸、画布预算、取消/迟到结果与资源释放均通过");
} finally { await server.close(); }
