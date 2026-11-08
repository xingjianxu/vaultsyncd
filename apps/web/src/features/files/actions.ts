import { ApiError, beginUpload, downloadVersionContent, finalizeUpload, getOperation, getSubtreeToken, getUpload, putUploadPart, submitOperation,
  type OperationRequest, type OperationResult, type SnapshotEntry, type UploadSession } from "@vaultsyncd/api-client";
import { sha256 as incrementalSha256 } from "@noble/hashes/sha2.js";
import { SignedOut, type BrowserAuth } from "../../services/auth";
import { readDirectorySnapshot, type DirectorySnapshot } from "./browser";

/** 固定版本下载 API 当前完整缓冲；复制保持显式上限，上传走增量 Blob 分片。 */
export const MAX_FILE_ACTION_BYTES = 64 * 1024 * 1024;
export const MAX_UPLOAD_BYTES = 64 * 1024 * 1024 * 1024;
const BUFFER_BYTES = 1024 * 1024;
export type FileActionState = "ready" | "running" | "pending" | "failed" | "completed";
export interface FileActionJob {
  readonly label: string;
  readonly state: FileActionState;
  readonly progress: { completed: number; total: number; current?: string };
  readonly summary: string;
  /** 没有未知或 pending 的命名空间操作时才可停止后续；已 committed 内容不回滚。 */
  readonly canDiscard: boolean;
  /** 保留此对象后继续；pending 不表示成功。刷新后通过恢复日志查询原请求。 */
  run(): Promise<void>;
}
export interface FileClipboard {
  readonly mode: "copy" | "cut";
  readonly root: string;
  readonly serverEpoch: string;
  readonly entry: Readonly<SnapshotEntry>;
  /** 父先于子，名称、revision 和 VersionId 均为复制时的值。 */
  readonly entries: readonly Readonly<SnapshotEntry>[];
}
const identity = () => crypto.randomUUID().replaceAll("-", "");
const sha256 = async (bytes: Uint8Array) => Array.from(new Uint8Array(await crypto.subtle.digest("SHA-256", bytes as Uint8Array<ArrayBuffer>)), byte => byte.toString(16).padStart(2, "0")).join("");
const message = (error: unknown) => error instanceof Error ? error.message : String(error);

function selectedTree(snapshot: DirectorySnapshot, entry: SnapshotEntry): SnapshotEntry[] {
  if (!entry.parent_id || !snapshot.entries.has(entry.entry_id)) throw new Error("不能操作 Root 根目录或已消失的条目。");
  const entries = [{ ...snapshot.entries.get(entry.entry_id)! }];
  for (let index = 0; index < entries.length; index++) {
    for (const child of snapshot.children.get(entries[index]!.entry_id) ?? []) entries.push({ ...child });
  }
  return entries;
}
function checkRoot(root: string, snapshot: DirectorySnapshot) {
  if (snapshot.metadata.root_id !== root) throw new Error("Root 与快照不匹配，请刷新。");
}
function checkTarget(root: string, snapshot: DirectorySnapshot, parent: SnapshotEntry, names: readonly string[]) {
  checkRoot(root, snapshot);
  if (snapshot.entries.get(parent.entry_id)?.kind !== "directory") throw new Error("目标目录不存在，请刷新。");
  const occupied = new Set((snapshot.children.get(parent.entry_id) ?? []).map(entry => entry.name));
  for (const name of names) {
    if (!name || name === "." || name === ".." || /[\\/\0]/.test(name)) throw new Error("文件名无效。");
    if (occupied.has(name)) throw new Error(`目标已存在同名条目：${name}。不会覆盖，请改名或选择其他目录。`);
    occupied.add(name);
  }
}
function checkSize(size: number | bigint, uploading = false) {
  if (size > (uploading ? MAX_UPLOAD_BYTES : MAX_FILE_ACTION_BYTES) || size < 0 || (typeof size === "number" && !Number.isSafeInteger(size))) throw new Error(uploading ? "上传单文件超过服务端 64 GiB 上限。" : "Web 复制单文件上限为 64 MiB，请使用同步客户端复制大文件。");
}

export interface RecoverableOperation { root: string; request: OperationRequest }
export interface RecoveredOperation extends RecoverableOperation { state: "missing" | OperationResult["state"]; result?: OperationResult }
export interface FileActionRecovery {
  list(): RecoverableOperation[];
  /** 只查询；404 保留为 missing，不能当成成功或自动重新删除。 */
  query(client: BrowserAuth): Promise<RecoveredOperation[]>;
  /** 必须由用户明确确认恢复原操作后调用；始终沿用原 request/device_id/op_id。 */
  resume(client: BrowserAuth, opId: string): Promise<RecoveredOperation>;
  save(record: RecoverableOperation): void;
  clear(opId: string): void;
}
/** 每个账号独立的 sessionStorage key；只保存命名空间请求，不保存凭据或文件字节。 */
export function createFileActionRecovery(accountScope: string, storage: Storage = sessionStorage): FileActionRecovery {
  if (!accountScope) throw new Error("文件操作恢复日志需要账号标识。");
  const key = `vaultsyncd.file-actions.${encodeURIComponent(accountScope)}`;
  const list = (): RecoverableOperation[] => {
    const records: unknown = JSON.parse(storage.getItem(key) ?? "[]");
    if (!Array.isArray(records) || records.some(record => !record || typeof record.root !== "string" || !record.request || typeof record.request.op_id !== "string" || typeof record.request.device_id !== "string" || typeof record.request.server_epoch !== "string" || !["create", "move", "delete"].includes(record.request.kind))) throw new Error("文件操作恢复日志无效，请保留日志并检查，不会自动发起新操作。");
    return records as RecoverableOperation[];
  };
  const write = (records: RecoverableOperation[]) => { if (records.length) storage.setItem(key, JSON.stringify(records)); else storage.removeItem(key); };
  const clear = (opId: string) => write(list().filter(record => record.request.op_id !== opId));
  const save = (record: RecoverableOperation) => {
    const records = list();
    const existing = records.find(item => item.request.op_id === record.request.op_id);
    if (existing && JSON.stringify(existing) !== JSON.stringify(record)) throw new Error("恢复日志的 OpId 对应不同请求，已停止。");
    if (!existing) {
      if (records.length) throw new Error("请先核对未确认的文件操作，不能发起新任务。");
      write([...records, record]);
    }
  };
  const queryOne = async (client: BrowserAuth, record: RecoverableOperation, replay: boolean): Promise<RecoveredOperation> => {
    let result: OperationResult;
    try { result = await client.request(options => getOperation(options, record.root, record.request.op_id)); }
    catch (error) {
      if (!(error instanceof ApiError) || error.status !== 404) throw error;
      if (!replay) return { ...record, state: "missing" };
      // 本次重放被拒绝不代表更早的未知 POST 已取消，只有稳定操作结果才可清日志。
      result = await client.request(options => submitOperation(options, record.root, record.request));
    }
    if (result.op_id !== record.request.op_id) throw new Error("恢复查询返回了不同的操作身份。");
    if (result.state !== "pending") clear(record.request.op_id);
    return { ...record, state: result.state, result };
  };
  return {
    list, save, clear,
    async query(client) {
      const results: RecoveredOperation[] = [];
      for (const record of list()) results.push(await queryOne(client, record, false));
      return results;
    },
    async resume(client, opId) {
      const record = list().find(item => item.request.op_id === opId);
      if (!record) throw new Error("没有此未确认的文件操作。");
      return queryOne(client, record, true);
    },
  };
}
function definitelyRejected(error: unknown) {
  return error instanceof SignedOut || (error instanceof ApiError && error.status >= 400 && error.status < 500 && error.status !== 408);
}
class OperationScope {
  readonly uncertain = new Set<string>();
  constructor(readonly recovery?: FileActionRecovery) {
    if (recovery?.list().length) throw new Error("请先核对未确认的文件操作，不能发起新任务。");
  }
  begin(root: string, request: OperationRequest) {
    this.recovery?.save({ root, request }); // 存储失败时禁止 submit。
    this.uncertain.add(request.op_id);
  }
  settle(opId: string) { this.uncertain.delete(opId); this.recovery?.clear(opId); }
}

class ActionJob implements FileActionJob {
  state: FileActionState = "ready";
  progress: { completed: number; total: number; current?: string };
  private error?: string;
  private active?: Promise<void>;
  constructor(readonly label: string, private readonly steps: { name: string; run: () => Promise<boolean> }[], private readonly scope: OperationScope) {
    this.progress = { completed: 0, total: steps.length };
  }
  get canDiscard() { return this.state !== "running" && this.scope.uncertain.size === 0; }
  get summary() {
    const count = `${this.progress.completed}/${this.progress.total} 项已提交`;
    if (this.state === "completed") return `${this.label}完成（${count}）。`;
    if (this.state === "pending") return `${count}；服务器仍在处理，请保留此任务并点击继续查询，不要重复发起。`;
    if (this.state === "failed") return `${count}；${this.error} 已提交的内容会保留，不自动回滚；请保留任务以核对或继续。`;
    return `${this.label}：${count}。`;
  }
  run(): Promise<void> {
    if (this.active) return this.active;
    if (this.state === "completed") return Promise.resolve();
    this.active = this.execute().finally(() => { this.active = undefined; });
    return this.active;
  }
  private async execute() {
    this.state = "running";
    this.error = undefined;
    try {
      while (this.progress.completed < this.steps.length) {
        const step = this.steps[this.progress.completed]!;
        this.progress.current = step.name;
        if (!await step.run()) { this.state = "pending"; return; }
        this.progress.completed++;
      }
      this.state = "completed";
      this.progress.current = undefined;
    } catch (error) {
      this.state = "failed";
      this.error = message(error);
      throw error;
    }
  }
}

/** 请求一经生成就不变；响应丢失先查 OpId，404 才重放同一请求。 */
function operation(client: BrowserAuth, root: string, scope: OperationScope, makeRequest: () => OperationRequest, beforeSubmit?: () => void) {
  let request: OperationRequest | undefined;
  let attempted = false;
  let result: OperationResult | undefined;
  let rejection: unknown;
  return async (): Promise<OperationResult | undefined> => {
    if (rejection) throw rejection;
    request ??= makeRequest();
    if (!result || result.state === "pending") {
      if (attempted) {
        try { result = await client.request(options => getOperation(options, root, request!.op_id)); }
        catch (error) {
          if (!(error instanceof ApiError) || error.status !== 404) throw error;
          result = undefined;
          // 404 只是查询瞬间尚未登记；原 POST 仍可能在途，不能清除追踪。
        }
      }
      if (!result) {
        beforeSubmit?.();
        const previouslyUncertain = scope.uncertain.has(request.op_id);
        scope.begin(root, request);
        attempted = true; // 网络异常也必须进入查询路径。
        try { result = await client.request(options => submitOperation(options, root, request!)); }
        catch (error) {
          if (definitelyRejected(error) && !previouslyUncertain) { rejection = error; scope.settle(request.op_id); }
          throw error;
        }
      }
    }
    if (result.op_id !== request.op_id) throw new Error("操作响应身份不匹配，请停止并核对。");
    if (result.state === "pending") return undefined;
    scope.settle(request.op_id);
    if (result.state === "rejected") throw new Error(result.error_message ?? `操作被拒绝：${result.error_code ?? "未知原因"}`);
    return result;
  };
}

async function blobHash(blob: Blob): Promise<string> {
  const digest = incrementalSha256.create();
  for (let offset = 0; offset < blob.size; offset += BUFFER_BYTES) digest.update(new Uint8Array(await blob.slice(offset, offset + BUFFER_BYTES).arrayBuffer()));
  return Array.from(digest.digest(), byte => byte.toString(16).padStart(2, "0")).join("");
}
function uploader(client: BrowserAuth, root: string, load: () => Promise<Blob>, expected?: { hash: string; size: string }) {
  let session: UploadSession | undefined;
  let finalized = false;
  let hash: string | undefined;
  return async () => {
    if (finalized) return session!.upload_id;
    const blob = await load();
    checkSize(blob.size, true);
    // 上传 Blob 不变，可复用摘要；远端复制每次下载都重新核验。
    if (!hash || expected) hash = await blobHash(blob);
    if (expected && (hash !== expected.hash || String(blob.size) !== expected.size)) throw new Error("固定版本的大小或 SHA-256 校验失败，未创建目标文件。");
    if (!session) session = await client.request(options => beginUpload(options, root, { hash: hash!, total_size: String(blob.size), chunk_size: String(BUFFER_BYTES) }));
    const status = await client.request(options => getUpload(options, root, session!.upload_id));
    session = status.upload;
    if (session.root_id !== root || session.hash !== hash || session.total_size !== String(blob.size)) throw new Error("上传会话与内容不匹配。");
    if (session.state === "aborted") throw new Error("上传会话已中止，请保留任务并核对服务器。");
    const chunk = Number(session.chunk_size);
    const parts = Math.ceil(blob.size / chunk);
    if (!Number.isSafeInteger(chunk) || chunk <= 0 || chunk > MAX_FILE_ACTION_BYTES || String(parts) !== session.part_count) throw new Error("上传分块参数无效。");
    if (session.state !== "finalized") {
      const received = new Set(status.received.map(part => part.index));
      for (let index = 0; index < parts; index++) {
        if (received.has(String(index))) continue;
        const part = new Uint8Array(await blob.slice(index * chunk, Math.min((index + 1) * chunk, blob.size)).arrayBuffer());
        const partHash = await sha256(part);
        await client.request(options => putUploadPart(options, root, session!.upload_id, String(index), partHash, part));
      }
      const content = await client.request(options => finalizeUpload(options, root, session!.upload_id));
      if (content.hash !== hash || content.size !== String(blob.size) || content.upload_id !== session.upload_id) throw new Error("上传完成校验结果不匹配。");
    }
    finalized = true;
    return session.upload_id;
  };
}

/** 限制实际响应流，而不只相信清单或 Content-Length，之后仍检查整文件摘要。 */
async function fixedContent(client: BrowserAuth, root: string, entry: Readonly<SnapshotEntry>): Promise<Uint8Array> {
  const size = BigInt(entry.size!);
  checkSize(size);
  return client.request(options => downloadVersionContent({ ...options, fetch: async (url, init) => {
    const response = await options.fetch(url, init);
    if (!response.ok) return response;
    if (response.status !== 200) throw new Error("固定版本未返回完整内容。");
    if (!response.body) throw new Error("固定版本响应缺少内容流。");
    const reader = response.body.getReader();
    let count = 0;
    return new Response(new ReadableStream<Uint8Array>({
      async pull(controller) {
        try {
          const { done, value } = await reader.read();
          if (done) {
            if (BigInt(count) !== size) throw new Error("固定版本长度不匹配。");
            controller.close(); return;
          }
          count += value.byteLength;
          if (BigInt(count) > size || count > MAX_FILE_ACTION_BYTES) throw new Error("固定版本超出声明大小或 64 MiB 上限。");
          controller.enqueue(value);
        } catch (error) { await reader.cancel().catch(() => undefined); controller.error(error); }
      },
      cancel(reason) { return reader.cancel(reason); },
    }), { status: response.status, headers: response.headers });
  } }, root, entry.entry_id, entry.version_id!));
}

export function createClipboard(mode: "copy" | "cut", root: string, snapshot: DirectorySnapshot, entry: SnapshotEntry): FileClipboard {
  checkRoot(root, snapshot);
  const entries = selectedTree(snapshot, entry).map(item => Object.freeze(item));
  return Object.freeze({ mode, root, serverEpoch: snapshot.metadata.server_epoch, entry: entries[0]!, entries: Object.freeze(entries) });
}

export function createUploadJob(client: BrowserAuth, root: string, snapshot: DirectorySnapshot, parent: SnapshotEntry, files: readonly File[], recovery?: FileActionRecovery): FileActionJob {
  const scope = new OperationScope(recovery);
  if (!files.length) throw new Error("请选择上传文件。");
  checkTarget(root, snapshot, parent, files.map(file => file.name));
  for (const file of files) checkSize(file.size, true);
  const epoch = snapshot.metadata.server_epoch;
  const parentId = parent.entry_id;
  const device = identity();
  return new ActionJob("上传", files.map(file => {
    const upload = uploader(client, root, async () => file);
    let uploadId: string;
    const commit = operation(client, root, scope, () => ({ kind: "create", op_id: identity(), device_id: device, server_epoch: epoch,
      parent_id: parentId, name: file.name, expected_absent: true, upload_id: uploadId }));
    return { name: file.name, run: async () => { uploadId ??= await upload(); return !!await commit(); } };
  }), scope);
}

export function createPasteJob(client: BrowserAuth, source: FileClipboard, targetRoot: string, targetSnapshot: DirectorySnapshot, parent: SnapshotEntry, recovery?: FileActionRecovery): FileActionJob {
  const scope = new OperationScope(recovery);
  if (source.mode === "cut" && source.root !== targetRoot) throw new Error("暂不支持跨 Root 剪切；请复制并核对目标后，再单独删除源内容。");
  checkTarget(targetRoot, targetSnapshot, parent, [source.entry.name]);
  if (source.root === targetRoot && source.entry.kind === "directory" && source.entries.some(entry => entry.entry_id === parent.entry_id)) throw new Error("不能将目录复制或移动到自身或后代。");
  if (source.mode === "cut" && source.serverEpoch !== targetSnapshot.metadata.server_epoch) throw new Error("源快照 epoch 已过期，请重新剪切。");
  const epoch = targetSnapshot.metadata.server_epoch;
  const device = identity();
  const parentId = parent.entry_id;
  if (source.mode === "cut") {
    const commit = operation(client, targetRoot, scope, () => ({ kind: "move", op_id: identity(), device_id: device, server_epoch: epoch,
      entry_id: source.entry.entry_id, base_revision: source.entry.revision, base_version_id: source.entry.version_id,
      parent_id: parentId, name: source.entry.name }));
    return new ActionJob("移动", [{ name: source.entry.name, run: async () => !!await commit() }], scope);
  }
  for (const entry of source.entries) if (entry.kind === "file") {
    if (!entry.version_id || !entry.hash || entry.size === undefined) throw new Error(`文件缺少固定版本或校验信息：${entry.name}`);
    checkSize(BigInt(entry.size));
  }
  const copied = new Map<string, string>();
  return new ActionJob("复制", source.entries.map(entry => {
    const upload = entry.kind === "file" ? uploader(client, targetRoot, async () => new Blob([await fixedContent(client, source.root, entry) as Uint8Array<ArrayBuffer>]), { hash: entry.hash!, size: entry.size! }) : undefined;
    let uploadId: string | undefined;
    const commit = operation(client, targetRoot, scope, () => {
      const destination = entry.entry_id === source.entry.entry_id ? parentId : copied.get(entry.parent_id!);
      if (!destination) throw new Error("目标父目录尚未提交，已停止复制。");
      return { kind: "create", op_id: identity(), device_id: device, server_epoch: epoch, parent_id: destination,
        name: entry.name, expected_absent: true, ...(uploadId ? { upload_id: uploadId } : {}) };
    });
    return { name: entry.name, run: async () => {
      if (upload && !uploadId) uploadId = await upload();
      const result = await commit();
      if (!result) return false;
      if (!result.entry_id) throw new Error("已提交结果缺少目标身份，请核对服务器，不要重复复制。");
      copied.set(entry.entry_id, result.entry_id); return true;
    } };
  }), scope);
}

/** 重命名冻结原父目录与 CAS 基线；继续操作沿用同一个 OpId。 */
export function createRenameJob(client: BrowserAuth, root: string, snapshot: DirectorySnapshot, entry: SnapshotEntry, name: string, recovery?: FileActionRecovery): FileActionJob {
  const scope = new OperationScope(recovery);
  checkRoot(root, snapshot);
  const current = snapshot.entries.get(entry.entry_id);
  if (!current?.parent_id) throw new Error("不能重命名 Root 根目录或已消失的条目。");
  const selected = { ...current };
  if (name === selected.name) throw new Error("名称未改变。");
  const parent = snapshot.entries.get(selected.parent_id!)!;
  checkTarget(root, snapshot, parent, [name]);
  const request: OperationRequest = { kind: "move", op_id: identity(), device_id: identity(), server_epoch: snapshot.metadata.server_epoch,
    entry_id: selected.entry_id, base_revision: selected.revision, base_version_id: selected.version_id, parent_id: selected.parent_id, name };
  const commit = operation(client, root, scope, () => request);
  return new ActionJob("重命名", [{ name: selected.name, run: async () => !!await commit() }], scope);
}

export interface PreparedDelete {
  readonly entry: Readonly<SnapshotEntry>;
  readonly count: number;
  readonly expiresAt: number;
  /** 必须在展示确认后调用；重复调用仍返回同一任务，禁止重新获取 token 偷换确认范围。 */
  createJob(): FileActionJob;
}
export async function prepareDelete(client: BrowserAuth, root: string, snapshot: DirectorySnapshot, entry: SnapshotEntry, recovery?: FileActionRecovery): Promise<PreparedDelete> {
  const scope = new OperationScope(recovery);
  checkRoot(root, snapshot);
  const frozen = selectedTree(snapshot, entry);
  const selected = Object.freeze(frozen[0]!);
  let token: string | undefined;
  if (selected.kind === "directory") {
    const proof = await client.request(options => getSubtreeToken(options, root, selected.entry_id));
    if (proof.root_id !== root || proof.entry_id !== selected.entry_id || proof.entries !== String(frozen.length)) throw new Error("目录子树已变化，请刷新后重新确认删除。");
    token = proof.token;
    // token 先于核验取得：核验后再变动只会导致 SUBTREE_CHANGED，不能扩大确认范围。
    const current = await readDirectorySnapshot(client, root, new AbortController().signal);
    const currentEntry = current.entries.get(selected.entry_id);
    if (!currentEntry || current.metadata.server_epoch !== snapshot.metadata.server_epoch) throw new Error("删除基线已过期，请刷新。");
    const latest = selectedTree(current, currentEntry);
    const byId = new Map(latest.map(item => [item.entry_id, item]));
    if (latest.length !== frozen.length || frozen.some(item => {
      const now = byId.get(item.entry_id);
      return !now || now.revision !== item.revision || now.parent_id !== item.parent_id || now.name !== item.name || now.kind !== item.kind || now.version_id !== item.version_id;
    })) throw new Error("目录子树已变化，请刷新后重新确认删除。");
  }
  const expiresAt = Math.min(Date.now() + 120_000, Number(snapshot.metadata.expires_at) * 1000);
  const valid = () => { if (Date.now() >= expiresAt) throw new Error("删除确认已过期，请刷新并重新确认；不会自动更新删除范围。"); };
  valid();
  const epoch = snapshot.metadata.server_epoch;
  const request: OperationRequest = { kind: "delete", op_id: identity(), device_id: identity(), server_epoch: epoch,
    entry_id: selected.entry_id, base_revision: selected.revision, base_version_id: selected.version_id, ...(token ? { subtree_token: token } : {}) };
  const commit = operation(client, root, scope, () => request, valid);
  const job = new ActionJob("移到回收站", [{ name: selected.name, run: async () => !!await commit() }], scope);
  return Object.freeze({ entry: selected, count: frozen.length, expiresAt, createJob: () => job });
}
