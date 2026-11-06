import { ApiError, createSnapshot, getOperation, listRecoveryEntries, submitOperation, type OperationRequest, type OperationResult, type RecoveryItem } from "@vaultsyncd/api-client";
import type { BrowserAuth } from "../../services/auth";

export type RestoreRecord = { root: string; request: OperationRequest; result?: OperationResult; path?: string };
const identity = () => crypto.randomUUID().replaceAll("-", "");
export class RestoreSession {
  readonly key: string;
  constructor(readonly user: string, readonly root: string) { this.key = `vaultsyncd.restore.${user}.${root}`; }
  read(): RestoreRecord | null {
    const raw = sessionStorage.getItem(this.key);
    return raw ? JSON.parse(raw) as RestoreRecord : null;
  }
  save(record: RestoreRecord) { sessionStorage.setItem(this.key, JSON.stringify(record)); }
  async begin(client: BrowserAuth, item: RecoveryItem, historical: boolean): Promise<RestoreRecord> {
    const previous = this.read();
    if (previous && (!previous.result || previous.result.state === "pending")) throw new Error("请先核对上次恢复结果。");
    const snapshot = await client.request(options => createSnapshot(options, this.root));
    const record: RestoreRecord = { root: this.root, request: {
      kind: "restore", op_id: identity(), device_id: identity(), server_epoch: snapshot.server_epoch,
      entry_id: item.entry_id, base_revision: item.revision, ...(historical ? { version_id: item.version_id } : {}),
    } };
    this.save(record); // 先保留原请求；提交失败也不可换 OpId。
    return this.resume(client, record);
  }
  async resume(client: BrowserAuth, record = this.read()): Promise<RestoreRecord> {
    if (!record || record.root !== this.root) throw new Error("没有可重试的恢复操作。");
    const request = record.request;
    let result: OperationResult;
    try { result = await client.request(options => getOperation(options, this.root, request.op_id)); }
    catch (cause) {
      if (!(cause instanceof ApiError) || cause.status !== 404) throw cause;
      result = await client.request(options => submitOperation(options, this.root, request));
    }
    record = { ...record, result }; this.save(record);
    if (result.state === "committed") {
      let after: string | undefined;
      do {
        const page = await client.request(options => listRecoveryEntries(options, this.root, { after }));
        const item = page.items.find(item => item.entry_id === result.entry_id);
        if (item) { record.path = item.name; break; }
        after = page.next_cursor;
      } while (after);
      this.save(record);
    }
    return record;
  }
}
export function expiry(item: RecoveryItem): string {
  return item.expires_at ? new Date(Number(item.expires_at) * 1000).toISOString() : "当前活跃版本，无年龄到期";
}
export function expired(item: RecoveryItem): boolean { return !!item.expires_at && BigInt(item.expires_at) * 1000n <= BigInt(Date.now()); }
