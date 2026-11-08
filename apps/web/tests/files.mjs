import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

const server = await createServer({ root: fileURLToPath(new URL("../../..", import.meta.url)), configFile: false,
  server: { middlewareMode: true, hmr: false }, ssr: { noExternal: ["@vaultsyncd/api-client"] } });
try {
  const { readDirectorySnapshot, ancestry, fileSize, filesLocation, parseFilesLocation, directoryAtPath } = await server.ssrLoadModule("/apps/web/src/features/files/browser.ts");
  const { BrowserAuth } = await server.ssrLoadModule("/apps/web/src/services/auth.ts");
  const folder = (entry_id, parent_id, name) => ({ entry_id, parent_id, name, kind: "directory", revision: "1" });
  const entries = [folder("r", undefined, ""), folder("d", "r", "文档"), folder("empty", "r", "空目录"),
    { entry_id: "f", parent_id: "d", name: "<img src=x onerror=alert(1)>.txt", kind: "file", revision: "3", size: "9007199254740993", hash: "a".repeat(64), version_id: "v" }];
  const metadata = { root_id: "root", snapshot_id: "snapshot", server_epoch: "1", high_water_seq: "9", entry_count: "4", expires_at: "2000000000" };
  async function load({ items = entries, meta = metadata, pageChange, failSecond = false, signal = new AbortController().signal, onPage } = {}) {
    let pages = 0;
    const client = new BrowserAuth({ base_url: "https://test.invalid", request_id: () => "files-test", fetch: async (url, init) => {
      assert.equal(init.signal, signal);
      if (url.endsWith("/snapshots")) return Response.json(meta);
      const parsed = new URL(url);
      assert.equal(parsed.pathname, "/v1/roots/root/snapshots/snapshot/entries");
      assert.equal(parsed.searchParams.get("limit"), "1000");
      assert.equal(parsed.searchParams.get("cursor"), pages ? "2" : null);
      pages++;
      if (failSecond && pages === 2) throw new Error("offline midway");
      onPage?.(pages);
      const page = { snapshot_id: "snapshot", high_water_seq: "9", entries: items.slice((pages - 1) * 2, pages * 2),
        ...(pages === 1 && items.length > 2 ? { next_cursor: "2" } : {}) };
      return Response.json(pageChange ? pageChange(page, pages) : page);
    } });
    return readDirectorySnapshot(client, "root", signal);
  }
  const snapshot = await load();
  assert.equal(snapshot.entries.size, 4);
  const names = ["中文 # ? % & +", "%2F", "尾部 空格 "];
  const location = filesLocation("root", names);
  assert.deepEqual(parseFilesLocation(location), { rootId: "root", segments: names });
  assert.deepEqual(parseFilesLocation("/files"), { segments: [] });
  for (const bad of ["/files/", "/files/root/%zz", "/files/root/%2F", "/files/root/..", "/files/root//d", "/files/root/%00"]) assert.ok(parseFilesLocation(bad).error, bad);
  assert.equal(directoryAtPath(snapshot, []), snapshot.root);
  assert.equal(directoryAtPath(snapshot, ["文档"]).entry_id, "d");
  assert.equal(directoryAtPath(snapshot, ["空目录"]).entry_id, "empty");
  assert.equal(directoryAtPath(snapshot, ["missing"]), undefined);
  assert.equal(directoryAtPath(snapshot, ["文档", entries[3].name]), undefined);
  assert.equal(directoryAtPath(snapshot, [".."]), undefined);
  assert.equal(snapshot.children.get("empty"), undefined);
  assert.deepEqual(ancestry(snapshot, snapshot.entries.get("f")).map(item => item.name), ["", "文档", entries[3].name]);
  assert.equal(snapshot.children.get("r").length, 2);
  assert.equal(fileSize(entries[3]), "9,007,199,254,740,993 字节");
  assert.equal(fileSize({ kind: "file", size: "0" }), "0 字节");
  assert.equal(fileSize({ kind: "file" }), "未知");
  assert.equal(fileSize(entries[0]), "—");
  await assert.rejects(load({ failSecond: true }), /offline midway/);
  await assert.rejects(load({ pageChange: page => ({ ...page, snapshot_id: "other" }) }), /分页不一致/);
  await assert.rejects(load({ pageChange: page => ({ ...page, high_water_seq: "10" }) }), /分页不一致/);
  await assert.rejects(load({ pageChange: page => ({ ...page, next_cursor: "0" }) }), /没有前进/);
  await assert.rejects(load({ items: [...entries.slice(0, 3), entries[0]] }), /重复条目/);
  await assert.rejects(load({ meta: { ...metadata, entry_count: "5" } }), /未完整读取/);
  await assert.rejects(load({ meta: { ...metadata, root_id: "other" } }), /Root 不匹配/);
  await assert.rejects(load({ meta: { ...metadata, entry_count: "100001" } }), /上限/);
  await assert.rejects(load({ items: [folder("r", undefined, ""), folder("d", "missing", "d")], meta: { ...metadata, entry_count: "2" } }), /父目录缺失/);
  await assert.rejects(load({ items: [folder("r", undefined, ""), folder("a", "b", "a"), folder("b", "a", "b")], meta: { ...metadata, entry_count: "3" } }), /不可达/);
  const controller = new AbortController();
  await assert.rejects(load({ signal: controller.signal, onPage: () => controller.abort() }), { name: "AbortError" });
  console.log("文件浏览器：完整分页、父目录树、精确大小、特殊文件名、中途失败/混页/坏游标/坏树/取消均通过");
} finally { await server.close(); }
