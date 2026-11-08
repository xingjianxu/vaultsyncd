import { createSnapshot, listSnapshotEntries, type Snapshot, type SnapshotEntry } from "@vaultsyncd/api-client";
import type { BrowserAuth } from "../../services/auth";

export interface DirectorySnapshot {
  metadata: Snapshot;
  root: SnapshotEntry;
  entries: Map<string, SnapshotEntry>;
  children: Map<string, SnapshotEntry[]>;
}
const MAX_ENTRIES = 100_000;

/** 只有全部分页及父目录树核验完成后才发布；中途错误不得表现成空目录。 */
export async function readDirectorySnapshot(client: BrowserAuth, rootId: string, signal: AbortSignal): Promise<DirectorySnapshot> {
  const metadata = await client.request(options => createSnapshot(options, rootId, { signal }));
  signal.throwIfAborted();
  if (metadata.root_id !== rootId) throw new Error("快照 Root 不匹配，请刷新重试。");
  const expected = BigInt(metadata.entry_count);
  if (expected > BigInt(MAX_ENTRIES)) throw new Error("此 Root 超过文件浏览器当前 100,000 项上限，请使用客户端浏览。");
  const entries = new Map<string, SnapshotEntry>();
  let cursor: string | undefined;
  do {
    const page = await client.request(options => listSnapshotEntries(options, rootId, metadata.snapshot_id, { cursor, limit: "1000" }, { signal }));
    signal.throwIfAborted();
    if (page.snapshot_id !== metadata.snapshot_id || page.high_water_seq !== metadata.high_water_seq) throw new Error("快照分页不一致，请刷新重试。");
    for (const entry of page.entries) {
      if (entries.has(entry.entry_id)) throw new Error("快照包含重复条目，请刷新重试。");
      entries.set(entry.entry_id, entry);
    }
    if (entries.size > MAX_ENTRIES || BigInt(entries.size) > expected) throw new Error("快照条目数不一致，请刷新重试。");
    if (page.next_cursor !== undefined && BigInt(page.next_cursor) <= BigInt(cursor ?? "0")) throw new Error("快照分页没有前进，请刷新重试。");
    cursor = page.next_cursor;
  } while (cursor !== undefined);
  if (BigInt(entries.size) !== expected) throw new Error("快照未完整读取，请刷新重试。");
  const roots = [...entries.values()].filter(entry => entry.parent_id === undefined);
  const root = roots[0];
  if (roots.length !== 1 || !root || root.kind !== "directory" || root.name !== "") throw new Error("快照根目录无效，请刷新重试。");
  const children = new Map<string, SnapshotEntry[]>();
  for (const entry of entries.values()) {
    if (entry.parent_id === undefined) continue;
    if (entries.get(entry.parent_id)?.kind !== "directory") throw new Error("快照父目录缺失，请刷新重试。");
    const siblings = children.get(entry.parent_id) ?? [];
    siblings.push(entry); children.set(entry.parent_id, siblings);
  }
  // 线性检查可达性，也能发现无根的目录环。
  const reachable = new Set<string>();
  const pending = [root];
  while (pending.length) {
    const entry = pending.pop()!;
    if (reachable.has(entry.entry_id)) throw new Error("快照目录存在环，请刷新重试。");
    reachable.add(entry.entry_id);
    for (const child of children.get(entry.entry_id) ?? []) pending.push(child);
  }
  if (reachable.size !== entries.size) throw new Error("快照目录不可达，请刷新重试。");
  const collator = new Intl.Collator("zh-CN", { numeric: true });
  for (const siblings of children.values()) siblings.sort((a, b) =>
    Number(b.kind === "directory") - Number(a.kind === "directory") || collator.compare(a.name, b.name) || a.entry_id.localeCompare(b.entry_id));
  return { metadata, root, entries, children };
}

export function ancestry(snapshot: DirectorySnapshot, entry: SnapshotEntry): SnapshotEntry[] {
  const path = [entry];
  while (entry.parent_id !== undefined) {
    entry = snapshot.entries.get(entry.parent_id)!;
    path.push(entry);
  }
  return path.reverse();
}

export function fileSize(entry: SnapshotEntry): string {
  if (entry.kind === "directory") return "—";
  return entry.size === undefined ? "未知" : `${BigInt(entry.size).toLocaleString("zh-CN")} 字节`;
}
