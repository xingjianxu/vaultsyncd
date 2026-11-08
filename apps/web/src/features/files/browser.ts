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

/** Hash 路由按路径段编码，避免名称中的 #、%、? 被解释成 URL 结构。 */
export function filesLocation(rootId: string, segments: readonly string[] = []): string {
  return `/files/${[rootId, ...segments].map(encodeURIComponent).join("/")}`;
}

export function parseFilesLocation(location: string): { rootId?: string; segments: string[]; error?: string } {
  if (location === "/files") return { segments: [] };
  try {
    const [rootId, ...segments] = location.slice("/files/".length).split("/").map(decodeURIComponent);
    if (!rootId || segments.some(part => !part || part === "." || part === ".." || part.includes("/") || part.includes("\0"))) throw new Error();
    return { rootId, segments };
  } catch { return { segments: [], error: "目录 URL 无效。" }; }
}

export function directoryAtPath(snapshot: DirectorySnapshot, segments: readonly string[]): SnapshotEntry | undefined {
  let entry = snapshot.root;
  for (const name of segments) {
    const child = snapshot.children.get(entry.entry_id)?.find(item => item.name === name && item.kind === "directory");
    if (!child) return undefined;
    entry = child;
  }
  return entry;
}

/** 1024 进制的人类可读大小；目录没有内容大小，未采集的大小明确显示“未知”。 */
export function fileSize(entry: SnapshotEntry): string {
  if (entry.kind === "directory") return "—";
  if (entry.size === undefined) return "未知";
  let value = Number(BigInt(entry.size));
  if (!Number.isFinite(value)) return "未知";
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const rounded = unit === 0 ? value : Math.round(value * 10) / 10;
  return `${rounded.toLocaleString("zh-CN")} ${units[unit]}`;
}

/** 属性面板里的精确字节数：人类可读值 + 原始字节，避免丢掉准确长度。 */
export function fileSizeDetail(entry: SnapshotEntry): string {
  if (entry.kind === "directory") return "—";
  if (entry.size === undefined) return "未知";
  return `${fileSize(entry)}（${BigInt(entry.size).toLocaleString("zh-CN")} 字节）`;
}

/** 服务端记录的最后修改时间按本地时区显示；升级前已存在的条目没有该值。 */
export function fileModified(entry: SnapshotEntry): string {
  if (entry.modified_at === undefined) return "—";
  const seconds = Number(entry.modified_at);
  if (!Number.isFinite(seconds)) return "—";
  const date = new Date(seconds * 1000);
  if (Number.isNaN(date.getTime())) return "—";
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}
