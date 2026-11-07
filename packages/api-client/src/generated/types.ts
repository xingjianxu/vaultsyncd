// 本文件由 `cargo xtask generate` 从 `api/openapi.yaml` 生成，禁止手改。
//
// 契约：vaultsyncd remote API 0.1.0；生成器：vaultsyncd-xtask 0.2.0
// 服务端远端接口的首发草案（/v1）。JSON 使用 snake_case；所有清单分页有上限；未知枚举
// 不当作默认成功。健康探测与能力协商不需要身份认证，其余端点需要会话，按 design §15.2
// 在对应功能卡加入。
//
// 解码器只做 wire 形态校验：闭集枚举的未知取值、缺失的必填字段与非法十进制文本都会抛错，
// 调用方不会拿到“看起来可用”的默认值。

function expect_object(value: unknown, what: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`期望 ${what} 是对象`);
  }
  return value as Record<string, unknown>;
}

function expect_decimal(value: unknown, what: string): string {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/.test(value)) {
    throw new Error(`期望 ${what} 是十进制无符号整数文本`);
  }
  return value;
}

function expect_string(value: unknown, what: string): string {
  if (typeof value !== "string") {
    throw new Error(`期望 ${what} 是字符串`);
  }
  return value;
}

function expect_array(value: unknown, what: string): unknown[] {
  if (!Array.isArray(value)) {
    throw new Error(`期望 ${what} 是数组`);
  }
  return value;
}

function expect_boolean(value: unknown, what: string): boolean {
  if (typeof value !== "boolean") {
    throw new Error(`期望 ${what} 是布尔值`);
  }
  return value;
}

function expect_number(value: unknown, what: string): number {
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw new Error(`期望 ${what} 是整数`);
  }
  return value;
}

function expect_string_map(value: unknown, what: string): Record<string, string> {
  const object = expect_object(value, what);
  for (const [key, item] of Object.entries(object)) {
    if (typeof item !== "string") {
      throw new Error(`期望 ${what}.${key} 是字符串`);
    }
  }
  return object as Record<string, string>;
}

/** 账号操作完成且全部会话已撤销，必须重新登录。 */
export interface AccountChanged {
  /** 本次撤销的会话数 */
  sessions_revoked: string;
}

export function decodeAccountChanged(value: unknown): AccountChanged {
  const object = expect_object(value, "AccountChanged");
  return {
    sessions_revoked: expect_decimal(object.sessions_revoked, "AccountChanged.sessions_revoked"),
  };
}

/** 当前已认证账号，不含凭据。 */
export interface AccountInfo {
  /** 登录名 */
  login: string;
  /** 会话身份，不是凭据 */
  session_id: string;
  /** 账号身份 */
  user_id: string;
}

export function decodeAccountInfo(value: unknown): AccountInfo {
  const object = expect_object(value, "AccountInfo");
  return {
    login: expect_string(object.login, "AccountInfo.login"),
    session_id: expect_string(object.session_id, "AccountInfo.session_id"),
    user_id: expect_string(object.user_id, "AccountInfo.user_id"),
  };
}

/** 固定源内容的完整性，不能代表客户端目标已持久导出。 */
export type BackupIntegrity = "unverified" | "verified" | "incomplete";

export const BACKUP_INTEGRITY_VALUES = ["unverified", "verified", "incomplete"] as const;

export function decodeBackupIntegrity(value: unknown): BackupIntegrity {
  if (typeof value !== "string" || !(BACKUP_INTEGRITY_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 BackupIntegrity 取值：${JSON.stringify(value)}`);
  }
  return value as BackupIntegrity;
}

/** 备份管理的BackupPrepareRequest契约 */
export interface BackupPrepareRequest {
  /** 调用方稳定请求身份；重试复用。 */
  backup_id: string;
}

export function decodeBackupPrepareRequest(value: unknown): BackupPrepareRequest {
  const object = expect_object(value, "BackupPrepareRequest");
  return {
    backup_id: expect_string(object.backup_id, "BackupPrepareRequest.backup_id"),
  };
}

/** 备份管理的BackupSnapshotList契约 */
export interface BackupSnapshotList {
  /** 当前页备份状态 */
  items: Array<BackupSnapshotStatus>;
  /** 下一页号，缺省表示结束 */
  next_page?: string;
}

export function decodeBackupSnapshotList(value: unknown): BackupSnapshotList {
  const object = expect_object(value, "BackupSnapshotList");
  return {
    items: expect_array(object.items, "BackupSnapshotList.items").map((item) => decodeBackupSnapshotStatus(item)),
    next_page: object.next_page === undefined ? undefined : expect_decimal(object.next_page, "BackupSnapshotList.next_page"),
  };
}

/** 源快照准备/释放阶段；不会把pending当complete。 */
export type BackupSnapshotPhase = "preparing" | "ready" | "failed" | "releasing" | "released" | "expired";

export const BACKUP_SNAPSHOT_PHASE_VALUES = ["preparing", "ready", "failed", "releasing", "released", "expired"] as const;

export function decodeBackupSnapshotPhase(value: unknown): BackupSnapshotPhase {
  if (typeof value !== "string" || !(BACKUP_SNAPSHOT_PHASE_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 BackupSnapshotPhase 取值：${JSON.stringify(value)}`);
  }
  return value as BackupSnapshotPhase;
}

/** 备份管理的BackupSnapshotStatus契约 */
export interface BackupSnapshotStatus {
  /** 稳定备份身份，32位小写十六进制 */
  backup_id: string;
  /** 创建UTC Unix秒 */
  created_at: string;
  /** 当前原因或错误说明 */
  detail: string;
  /** 租约到期UTC Unix秒 */
  expires_at: string;
  /** 源快照完整性，独立于客户端导出 */
  integrity: BackupIntegrity;
  /** 持久作业是否尚未结束 */
  job_pending: boolean;
  /** 固定manifest SHA-256，准备成功后才提供 */
  manifest_hash?: string;
  /** 快照准备或释放阶段 */
  phase: BackupSnapshotPhase;
}

export function decodeBackupSnapshotStatus(value: unknown): BackupSnapshotStatus {
  const object = expect_object(value, "BackupSnapshotStatus");
  return {
    backup_id: expect_string(object.backup_id, "BackupSnapshotStatus.backup_id"),
    created_at: expect_decimal(object.created_at, "BackupSnapshotStatus.created_at"),
    detail: expect_string(object.detail, "BackupSnapshotStatus.detail"),
    expires_at: expect_decimal(object.expires_at, "BackupSnapshotStatus.expires_at"),
    integrity: decodeBackupIntegrity(object.integrity),
    job_pending: expect_boolean(object.job_pending, "BackupSnapshotStatus.job_pending"),
    manifest_hash: object.manifest_hash === undefined ? undefined : expect_string(object.manifest_hash, "BackupSnapshotStatus.manifest_hash"),
    phase: decodeBackupSnapshotPhase(object.phase),
  };
}

/** 浏览器只读会话元信息；凭据只在 HttpOnly cookie 中。 */
export interface BrowserSession {
  /** 访问会话到期 UTC Unix 秒 */
  access_expires_at: string;
  /** 会话身份，不是凭据 */
  session_id: string;
}

export function decodeBrowserSession(value: unknown): BrowserSession {
  const object = expect_object(value, "BrowserSession");
  return {
    access_expires_at: expect_decimal(object.access_expires_at, "BrowserSession.access_expires_at"),
    session_id: expect_string(object.session_id, "BrowserSession.session_id"),
  };
}

/** 协议版本、服务器身份、epoch、功能集合与限额（design §15.2）。 */
export interface Capabilities {
  /** 服务器 epoch 的十进制文本。恢复后更换，客户端据此丢弃旧 epoch 的响应。 */
  epoch: string;
  /**
   * 服务端支持的功能标记。客户端只使用自己认识的值，未识别的标记按“该功能不可用”
   * 处理；这里是可扩展标记集合，不是闭集枚举，因此不套用“未知枚举即失败”。
   */
  features: Array<string>;
  /** 客户端必须遵守的服务端限额。 */
  limits: CapabilityLimits;
  /** 协议主次版本。 */
  protocol: ProtocolVersion;
  /** 服务器身份，32 个小写十六进制字符（design §7.1）。 */
  server_id: string;
}

export function decodeCapabilities(value: unknown): Capabilities {
  const object = expect_object(value, "Capabilities");
  return {
    epoch: expect_decimal(object.epoch, "Capabilities.epoch"),
    features: expect_array(object.features, "Capabilities.features").map((item) => expect_string(item, "Capabilities.features 的元素")),
    limits: decodeCapabilityLimits(object.limits),
    protocol: decodeProtocolVersion(object.protocol),
    server_id: expect_string(object.server_id, "Capabilities.server_id"),
  };
}

/** 客户端必须遵守的服务端限额。 */
export interface CapabilityLimits {
  /** 清单分页的每页上限。（取值范围 1..=10000） */
  max_page_size: number;
  /** 单个上传块的最大字节数（十进制文本）。 */
  max_upload_part_bytes: string;
}

export function decodeCapabilityLimits(value: unknown): CapabilityLimits {
  const object = expect_object(value, "CapabilityLimits");
  return {
    max_page_size: expect_number(object.max_page_size, "CapabilityLimits.max_page_size"),
    max_upload_part_bytes: expect_decimal(object.max_upload_part_bytes, "CapabilityLimits.max_upload_part_bytes"),
  };
}

/** 一个事务批次（design §9.3）：RootSeq、TxId 与该事务的全部条目变化。 */
export interface ChangeBatch {
  /** 批次提交时间（UTC Unix 秒的十进制文本）。 */
  created_at: string;
  /** 该事务全部条目变化与冲突关联的 JSON 数组文本；按数据处理，客户端整批原子应用。 */
  events_json: string;
  /** 批次序号（该 Root 的第 seq 个事务批次，十进制文本）。 */
  seq: string;
  /** 产生该批次的服务端事务身份，32 位小写十六进制。 */
  tx_id: string;
}

export function decodeChangeBatch(value: unknown): ChangeBatch {
  const object = expect_object(value, "ChangeBatch");
  return {
    created_at: expect_decimal(object.created_at, "ChangeBatch.created_at"),
    events_json: expect_string(object.events_json, "ChangeBatch.events_json"),
    seq: expect_decimal(object.seq, "ChangeBatch.seq"),
    tx_id: expect_string(object.tx_id, "ChangeBatch.tx_id"),
  };
}

/** 一页有序事务批次。 */
export interface ChangeList {
  /** 批次，严格按 seq 升序，每个批次完整。 */
  batches: Array<ChangeBatch>;
  /** 读取时 `next_after` 之后是否还有批次。 */
  has_more: boolean;
  /** 读取时 Root 的最新 RootSeq（十进制文本）。 */
  head_seq: string;
  /** 下一次拉取应带的 `after`：本页最后一个批次的 seq；本页为空时等于请求的 `after`。 */
  next_after: string;
  /** Root 身份，32 位小写十六进制。 */
  root_id: string;
  /** Root 当前的服务器 epoch（十进制文本）。 */
  server_epoch: string;
}

export function decodeChangeList(value: unknown): ChangeList {
  const object = expect_object(value, "ChangeList");
  return {
    batches: expect_array(object.batches, "ChangeList.batches").map((item) => decodeChangeBatch(item)),
    has_more: expect_boolean(object.has_more, "ChangeList.has_more"),
    head_seq: expect_decimal(object.head_seq, "ChangeList.head_seq"),
    next_after: expect_decimal(object.next_after, "ChangeList.next_after"),
    root_id: expect_string(object.root_id, "ChangeList.root_id"),
    server_epoch: expect_decimal(object.server_epoch, "ChangeList.server_epoch"),
  };
}

/**
 * 过期基线的冲突策略闭集（design §15.3）。首发只有 `preserve_both`：服务器主条目与冲突副本
 * 都保留，绝不丢内容。
 */
export type ConflictPolicy = "preserve_both";

export const CONFLICT_POLICY_VALUES = ["preserve_both"] as const;

export function decodeConflictPolicy(value: unknown): ConflictPolicy {
  if (typeof value !== "string" || !(CONFLICT_POLICY_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 ConflictPolicy 取值：${JSON.stringify(value)}`);
  }
  return value as ConflictPolicy;
}

/** 提交成功但基线与服务器不一致的原因闭集（`preserve_both` 下产生冲突副本）。 */
export type ConflictReason = "stale_base";

export const CONFLICT_REASON_VALUES = ["stale_base"] as const;

export function decodeConflictReason(value: unknown): ConflictReason {
  if (typeof value !== "string" || !(CONFLICT_REASON_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 ConflictReason 取值：${JSON.stringify(value)}`);
  }
  return value as ConflictReason;
}

/** 条目种类闭集（design §7.2 `entries.kind`）。未知取值按协议错误处理。 */
export type EntryKind = "file" | "directory";

export const ENTRY_KIND_VALUES = ["file", "directory"] as const;

export function decodeEntryKind(value: unknown): EntryKind {
  if (typeof value !== "string" || !(ENTRY_KIND_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 EntryKind 取值：${JSON.stringify(value)}`);
  }
  return value as EntryKind;
}

/** 错误主体。 */
export interface ErrorBody {
  /** 闭集错误码；未知码不是可重试错误，按协议错误处理。 */
  code: ErrorCode;
  /** 结构化补充信息，例如 `{"reason": "mount_identity_changed"}`。 */
  details?: Record<string, string>;
  /** 面向用户的可理解原因，按数据处理，不当作指令。 */
  message: string;
  /** 服务端记录的请求标识，供日志关联。 */
  request_id: string;
  /** 同一请求稍后原样重试是否可能成功。 */
  retryable: boolean;
  /** 相关 Root 的身份（32 个小写十六进制字符）；与 Root 无关时省略。 */
  root_id?: string;
}

export function decodeErrorBody(value: unknown): ErrorBody {
  const object = expect_object(value, "ErrorBody");
  return {
    code: decodeErrorCode(object.code),
    details: object.details === undefined ? undefined : expect_string_map(object.details, "ErrorBody.details"),
    message: expect_string(object.message, "ErrorBody.message"),
    request_id: expect_string(object.request_id, "ErrorBody.request_id"),
    retryable: expect_boolean(object.retryable, "ErrorBody.retryable"),
    root_id: object.root_id === undefined ? undefined : expect_string(object.root_id, "ErrorBody.root_id"),
  };
}

/**
 * 远端错误码闭集（design §15.4）。未知错误码不能被当成可重试或成功；本机 IPC 专用的
 * 错误码（LOCAL_CHANGED、FILE_BUSY、MODE_REQUIRES_LOCAL_COPY、PLACEHOLDER_OFFLINE）
 * 属于 packages/local-ipc-types，不在远端契约内。
 *
 * SERVICE_NOT_READY 是“服务端这一版还没有装配该端点所需的执行链路”的显式拒绝（本卡起用于
 * 操作命令端点），与 NEEDS_RECOVERY 的“意图无法解释、需要人工介入”不是一回事；它待与
 * docs/design.md §15.4 的表格同步。
 */
export type ErrorCode = "AUTH_EXPIRED" | "ACCESS_DENIED" | "BASE_MISMATCH" | "SUBTREE_CHANGED" | "OP_ID_REUSED" | "RECONCILE_REQUIRED" | "CURSOR_EXPIRED" | "EPOCH_CHANGED" | "VERSION_GONE" | "INVALID_NAME" | "INVALID_REQUEST" | "UNSUPPORTED_ENTRY" | "HASH_MISMATCH" | "PART_CONFLICT" | "UNSUPPORTED_STORAGE_BACKEND" | "SERVER_PLACEHOLDER_UNSUPPORTED" | "RATE_LIMITED" | "ROOT_UNAVAILABLE" | "NEEDS_RECOVERY" | "INSUFFICIENT_STORAGE" | "SERVICE_NOT_READY";

export const ERROR_CODE_VALUES = ["AUTH_EXPIRED", "ACCESS_DENIED", "BASE_MISMATCH", "SUBTREE_CHANGED", "OP_ID_REUSED", "RECONCILE_REQUIRED", "CURSOR_EXPIRED", "EPOCH_CHANGED", "VERSION_GONE", "INVALID_NAME", "INVALID_REQUEST", "UNSUPPORTED_ENTRY", "HASH_MISMATCH", "PART_CONFLICT", "UNSUPPORTED_STORAGE_BACKEND", "SERVER_PLACEHOLDER_UNSUPPORTED", "RATE_LIMITED", "ROOT_UNAVAILABLE", "NEEDS_RECOVERY", "INSUFFICIENT_STORAGE", "SERVICE_NOT_READY"] as const;

export function decodeErrorCode(value: unknown): ErrorCode {
  if (typeof value !== "string" || !(ERROR_CODE_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 ErrorCode 取值：${JSON.stringify(value)}`);
  }
  return value as ErrorCode;
}

/** 错误信封（design §15.4）：所有非 2xx 响应都用它，客户端据此决定重试与暂停。 */
export interface ErrorEnvelope {
  /** 错误主体。 */
  error: ErrorBody;
}

export function decodeErrorEnvelope(value: unknown): ErrorEnvelope {
  const object = expect_object(value, "ErrorEnvelope");
  return {
    error: decodeErrorBody(object.error),
  };
}

/**
 * finalize 的结果：可提交内容引用（design §15.2 上传 finalize「只生成可提交内容引用，
 * 不修改条目 head」）。它还不是版本：条目 head 由后续操作按 CAS 提交。
 */
export interface FinalizedContent {
  /** 内容的 SHA-256，64 位小写十六进制；提交时用它引用这份内容。 */
  hash: string;
  /** 块数（十进制文本）。 */
  part_count: string;
  /** 是否为幂等重放（会话本来就已经 `finalized`）。 */
  replayed: boolean;
  /** 本次新建还是复用既有对象。 */
  reuse: ObjectReuse;
  /** 内容所属 Root 身份，32 位小写十六进制。 */
  root_id: string;
  /** 内容字节数（十进制文本）。 */
  size: string;
  /** 对象区内的规范相对路径（服务端内部路径）。客户端只原样回传，不拼接、不解释它。 */
  storage_path: string;
  /** 上传会话身份，32 位小写十六进制。 */
  upload_id: string;
}

export function decodeFinalizedContent(value: unknown): FinalizedContent {
  const object = expect_object(value, "FinalizedContent");
  return {
    hash: expect_string(object.hash, "FinalizedContent.hash"),
    part_count: expect_decimal(object.part_count, "FinalizedContent.part_count"),
    replayed: expect_boolean(object.replayed, "FinalizedContent.replayed"),
    reuse: decodeObjectReuse(object.reuse),
    root_id: expect_string(object.root_id, "FinalizedContent.root_id"),
    size: expect_decimal(object.size, "FinalizedContent.size"),
    storage_path: expect_string(object.storage_path, "FinalizedContent.storage_path"),
    upload_id: expect_string(object.upload_id, "FinalizedContent.upload_id"),
  };
}

/** 健康探测结果。 */
export interface HealthResponse {
  /** 服务端当前健康状态。 */
  status: HealthStatus;
}

export function decodeHealthResponse(value: unknown): HealthResponse {
  const object = expect_object(value, "HealthResponse");
  return {
    status: decodeHealthStatus(object.status),
  };
}

/** 健康状态闭集。客户端遇到未列出的值必须按协议错误处理，不能当作可用。 */
export type HealthStatus = "ok" | "degraded" | "needs_recovery";

export const HEALTH_STATUS_VALUES = ["ok", "degraded", "needs_recovery"] as const;

export function decodeHealthStatus(value: unknown): HealthStatus {
  if (typeof value !== "string" || !(HEALTH_STATUS_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 HealthStatus 取值：${JSON.stringify(value)}`);
  }
  return value as HealthStatus;
}

/** 登录请求（design §15.2 `POST /v1/auth/login` 的输入）。 */
export interface LoginRequest {
  /** 设备的人类可读标签（不是设备身份，首发没有设备注册表）。 */
  device_label: string;
  /** 登录名（原样比较，不做大小写折叠）。 */
  login: string;
  /** 明文密码；只在本次请求内存在，服务端不记录。 */
  password: string;
}

export function decodeLoginRequest(value: unknown): LoginRequest {
  const object = expect_object(value, "LoginRequest");
  return {
    device_label: expect_string(object.device_label, "LoginRequest.device_label"),
    login: expect_string(object.login, "LoginRequest.login"),
    password: expect_string(object.password, "LoginRequest.password"),
  };
}

/** 仅 owner 管理通道可见的 Root。 */
export interface ManagedRoot {
  /** 本进程采集观测 */
  collection: RootCollection;
  /** 现场挂载和存储状态 */
  health: RootHealth;
  /** 可执行的修复建议 */
  next_step: string;
  /** 已注册服务器绝对路径 */
  path: string;
  /** 稳定故障原因；健康时为空 */
  reason: string;
  /** Root 身份 */
  root_id: string;
  /** 已提交事件序号，不表示采集完成 */
  root_seq: string;
}

export function decodeManagedRoot(value: unknown): ManagedRoot {
  const object = expect_object(value, "ManagedRoot");
  return {
    collection: decodeRootCollection(object.collection),
    health: decodeRootHealth(object.health),
    next_step: expect_string(object.next_step, "ManagedRoot.next_step"),
    path: expect_string(object.path, "ManagedRoot.path"),
    reason: expect_string(object.reason, "ManagedRoot.reason"),
    root_id: expect_string(object.root_id, "ManagedRoot.root_id"),
    root_seq: expect_decimal(object.root_seq, "ManagedRoot.root_seq"),
  };
}

/** owner 管理清单；不可由普通 Root 列表推导路径。 */
export interface ManagedRootList {
  /** 部署预配置允许基目录；空表示未配置且禁止注册。 */
  allowed_bases: Array<string>;
  /** owner 已注册 Root。 */
  roots: Array<ManagedRoot>;
}

export function decodeManagedRootList(value: unknown): ManagedRootList {
  const object = expect_object(value, "ManagedRootList");
  return {
    allowed_bases: expect_array(object.allowed_bases, "ManagedRootList.allowed_bases").map((item) => expect_string(item, "ManagedRootList.allowed_bases 的元素")),
    roots: expect_array(object.roots, "ManagedRootList.roots").map((item) => decodeManagedRoot(item)),
  };
}

/** 一段连续缺失的块索引。 */
export interface MissingRange {
  /** 区间内的块数，至少为 1。 */
  count: string;
  /** 区间起点（含），十进制文本。 */
  first_index: string;
}

export function decodeMissingRange(value: unknown): MissingRange {
  const object = expect_object(value, "MissingRange");
  return {
    count: expect_decimal(object.count, "MissingRange.count"),
    first_index: expect_decimal(object.first_index, "MissingRange.first_index"),
  };
}

/**
 * 内容对象的发布方式闭集（design §11.2 步骤 7）。`freshly_published` 本次新建，
 * `existing_verified` 内容地址上已有同内容对象、未覆盖并复核通过后复用。
 */
export type ObjectReuse = "freshly_published" | "existing_verified";

export const OBJECT_REUSE_VALUES = ["freshly_published", "existing_verified"] as const;

export function decodeObjectReuse(value: unknown): ObjectReuse {
  if (typeof value !== "string" || !(OBJECT_REUSE_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 ObjectReuse 取值：${JSON.stringify(value)}`);
  }
  return value as ObjectReuse;
}

/**
 * 已提交操作附带的冲突信息（design §15.3）：服务器主条目、新建的冲突副本与原因。内容都在
 * 服务器上，客户端据此把本地迟到修改对到副本身份上。
 */
export interface OperationConflict {
  /** 冲突副本的条目身份，32 位小写十六进制。 */
  conflict_entry_id: string;
  /** 服务器主条目的身份，32 位小写十六进制。 */
  entry_id: string;
  /** 产生副本的原因。 */
  reason: ConflictReason;
}

export function decodeOperationConflict(value: unknown): OperationConflict {
  const object = expect_object(value, "OperationConflict");
  return {
    conflict_entry_id: expect_string(object.conflict_entry_id, "OperationConflict.conflict_entry_id"),
    entry_id: expect_string(object.entry_id, "OperationConflict.entry_id"),
    reason: decodeConflictReason(object.reason),
  };
}

/** 操作命令闭集（design §15.2）。未知取值按协议错误处理，不能当作任一命令执行。 */
export type OperationKind = "put_content" | "create" | "move" | "delete" | "restore";

export const OPERATION_KIND_VALUES = ["put_content", "create", "move", "delete", "restore"] as const;

export function decodeOperationKind(value: unknown): OperationKind {
  if (typeof value !== "string" || !(OPERATION_KIND_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 OperationKind 取值：${JSON.stringify(value)}`);
  }
  return value as OperationKind;
}

/**
 * 一次修改命令的输入（design §15.3）。`op_id` 与 `device_id` 一起构成幂等键
 * `(root, device_id, op_id)`；不同命令用不同字段（put 用 `entry_id`/`base_revision`/
 * `upload_id`，create 用 `parent_id`/`name`/`expected_absent`，move 用源
 * `entry_id`/`base_revision` 与目标 `parent_id`/`name`，delete 用
 * `entry_id`/`base_revision`（目录另带 `subtree_token`），restore 用
 * `entry_id`/`base_revision`（`version_id` 表示恢复指定历史版本））。带错字段一律 400，
 * 同一请求里不允许两套命令语义。
 */
export interface OperationRequest {
  /** 客户端计划时的条目 revision（十进制文本）；过期基线不会被当作最新事实接受。 */
  base_revision?: string;
  /** 客户端计划时看到的版本身份（可选，32 位小写十六进制）。 */
  base_version_id?: string;
  /** 过期基线的冲突策略；缺省按 `preserve_both` 处理。 */
  conflict_policy?: ConflictPolicy;
  /**
   * 发起操作的设备身份，32 位小写十六进制。首发没有设备注册表，它是客户端安装实例自报的
   * 稳定身份，只用来划定幂等键的范围（design §7.2），不作为授权依据。
   */
  device_id: string;
  /** 被操作或被覆盖的条目身份（put/delete 的目标、move 的源、restore 的条目），32 位小写十六进制。 */
  entry_id?: string;
  /** 新建时声明“目标原本不存在”；create 必须为 true（design §15.3）。 */
  expected_absent?: boolean;
  /** 命令种类。 */
  kind: OperationKind;
  /**
   * 客户端本地内容代际（十进制文本）。服务器只原样回显供客户端核对，**不**把它当作
   * “客户端确实没有后续修改”的证明（design §15.3）。
   */
  local_generation?: string;
  /** 目标名称（create 必填、move 的目标名称）；不含路径分隔符。 */
  name?: string;
  /** 客户端生成的逻辑操作身份，32 位小写十六进制；重试必须复用同一个值。 */
  op_id: string;
  /** 目标父目录的条目身份（create 必填、move 的目标父目录），32 位小写十六进制。 */
  parent_id?: string;
  /** 客户端事实基础所属的服务器 epoch（十进制文本）。 */
  server_epoch: string;
  /**
   * 目录删除的服务端子树 token（32 位小写十六进制）。它由上一条快照/changes 结果给出；
   * 子树在之后任何变化都让删除返回 409 `SUBTREE_CHANGED`。
   */
  subtree_token?: string;
  /** 已 finalize 的上传会话身份（put 必填，创建文件时 create 也必填；不带它的 create 创建目录），32 位小写十六进制；内容只通过它引用。 */
  upload_id?: string;
  /** 恢复指定的历史版本（可选，32 位小写十六进制）；省略表示恢复回收站里的当前形态。 */
  version_id?: string;
}

export function decodeOperationRequest(value: unknown): OperationRequest {
  const object = expect_object(value, "OperationRequest");
  return {
    base_revision: object.base_revision === undefined ? undefined : expect_decimal(object.base_revision, "OperationRequest.base_revision"),
    base_version_id: object.base_version_id === undefined ? undefined : expect_string(object.base_version_id, "OperationRequest.base_version_id"),
    conflict_policy: object.conflict_policy === undefined ? undefined : decodeConflictPolicy(object.conflict_policy),
    device_id: expect_string(object.device_id, "OperationRequest.device_id"),
    entry_id: object.entry_id === undefined ? undefined : expect_string(object.entry_id, "OperationRequest.entry_id"),
    expected_absent: object.expected_absent === undefined ? undefined : expect_boolean(object.expected_absent, "OperationRequest.expected_absent"),
    kind: decodeOperationKind(object.kind),
    local_generation: object.local_generation === undefined ? undefined : expect_decimal(object.local_generation, "OperationRequest.local_generation"),
    name: object.name === undefined ? undefined : expect_string(object.name, "OperationRequest.name"),
    op_id: expect_string(object.op_id, "OperationRequest.op_id"),
    parent_id: object.parent_id === undefined ? undefined : expect_string(object.parent_id, "OperationRequest.parent_id"),
    server_epoch: expect_decimal(object.server_epoch, "OperationRequest.server_epoch"),
    subtree_token: object.subtree_token === undefined ? undefined : expect_string(object.subtree_token, "OperationRequest.subtree_token"),
    upload_id: object.upload_id === undefined ? undefined : expect_string(object.upload_id, "OperationRequest.upload_id"),
    version_id: object.version_id === undefined ? undefined : expect_string(object.version_id, "OperationRequest.version_id"),
  };
}

/**
 * 一次操作的状态与业务结果（design §15.3）。`pending` 时只有 `op_id`/`state`（可能带
 * `echo_local_generation`）：不出现 `revision`/`root_seq`/`version_id`/`entry_id` 一类
 * “已完成”字段。`replayed` 由端点按本次请求填写（是否命中了已记录的结果）；`request_id`
 * 在错误信封里给出，成功响应只带操作身份。
 */
export interface OperationResult {
  /** committed 且基线与服务器不一致时给出的冲突副本信息。 */
  conflict?: OperationConflict;
  /** 原样回显请求里的本地代际（十进制文本），供客户端核对 ACK 属于哪一代。 */
  echo_local_generation?: string;
  /** 业务结果涉及的条目身份（committed 时的目标条目），32 位小写十六进制。 */
  entry_id?: string;
  /** 稳定拒绝时的错误码（只有 rejected 有）。 */
  error_code?: ErrorCode;
  /** 稳定拒绝的可理解原因（只有 rejected 有，按数据处理）。 */
  error_message?: string;
  /** 逻辑操作身份，32 位小写十六进制。 */
  op_id: string;
  /** 本次响应是否直接来自已记录的结果（响应丢失后的重试为 true）。 */
  replayed?: boolean;
  /** 提交后的条目 revision（只有 committed 有，十进制文本）。 */
  revision?: string;
  /** 该批次占用的 RootSeq（只有 committed 有，十进制文本）。 */
  root_seq?: string;
  /** 操作状态；`pending` 不代表已同步。 */
  state: OperationState;
  /** 提交产生的新版本身份（只有 put/restore 的 committed 有），32 位小写十六进制。 */
  version_id?: string;
}

export function decodeOperationResult(value: unknown): OperationResult {
  const object = expect_object(value, "OperationResult");
  return {
    conflict: object.conflict === undefined ? undefined : decodeOperationConflict(object.conflict),
    echo_local_generation: object.echo_local_generation === undefined ? undefined : expect_decimal(object.echo_local_generation, "OperationResult.echo_local_generation"),
    entry_id: object.entry_id === undefined ? undefined : expect_string(object.entry_id, "OperationResult.entry_id"),
    error_code: object.error_code === undefined ? undefined : decodeErrorCode(object.error_code),
    error_message: object.error_message === undefined ? undefined : expect_string(object.error_message, "OperationResult.error_message"),
    op_id: expect_string(object.op_id, "OperationResult.op_id"),
    replayed: object.replayed === undefined ? undefined : expect_boolean(object.replayed, "OperationResult.replayed"),
    revision: object.revision === undefined ? undefined : expect_decimal(object.revision, "OperationResult.revision"),
    root_seq: object.root_seq === undefined ? undefined : expect_decimal(object.root_seq, "OperationResult.root_seq"),
    state: decodeOperationState(object.state),
    version_id: object.version_id === undefined ? undefined : expect_string(object.version_id, "OperationResult.version_id"),
  };
}

/**
 * 操作状态闭集（`operations.state`）。`pending` 只表示已受理，不占 RootSeq、不判已同步；
 * `committed` 与 `rejected` 是终态，同一 OpId 的结果不再改变。
 */
export type OperationState = "pending" | "committed" | "rejected";

export const OPERATION_STATE_VALUES = ["pending", "committed", "rejected"] as const;

export function decodeOperationState(value: unknown): OperationState {
  if (typeof value !== "string" || !(OPERATION_STATE_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 OperationState 取值：${JSON.stringify(value)}`);
  }
  return value as OperationState;
}

/** 改密输入；禁止记录明文，沿用初始化密码规则。 */
export interface PasswordChangeRequest {
  /** 当前密码，只在本次请求内存在 */
  current_password: string;
  /** 新密码，沿用初始化密码规则 */
  new_password: string;
}

export function decodePasswordChangeRequest(value: unknown): PasswordChangeRequest {
  const object = expect_object(value, "PasswordChangeRequest");
  return {
    current_password: expect_string(object.current_password, "PasswordChangeRequest.current_password"),
    new_password: expect_string(object.new_password, "PasswordChangeRequest.new_password"),
  };
}

/** 协议主次版本；主版本不同必须拒绝通信。 */
export interface ProtocolVersion {
  /** 主版本；与服务端不同即拒绝通信。（取值范围 0..=65535） */
  major: number;
  /** 次版本；服务端可以更大，客户端按自己认识的功能使用。（取值范围 0..=65535） */
  minor: number;
}

export function decodeProtocolVersion(value: unknown): ProtocolVersion {
  const object = expect_object(value, "ProtocolVersion");
  return {
    major: expect_number(object.major, "ProtocolVersion.major"),
    minor: expect_number(object.minor, "ProtocolVersion.minor"),
  };
}

/** 服务端已持久化的一个块。 */
export interface ReceivedPart {
  /** 块内容的 SHA-256，64 位小写十六进制。 */
  hash: string;
  /** 块索引（从 0 起，十进制文本）。 */
  index: string;
  /** 块长度（十进制文本），与块几何一致。 */
  length: string;
}

export function decodeReceivedPart(value: unknown): ReceivedPart {
  const object = expect_object(value, "ReceivedPart");
  return {
    hash: expect_string(object.hash, "ReceivedPart.hash"),
    index: expect_decimal(object.index, "ReceivedPart.index"),
    length: expect_decimal(object.length, "ReceivedPart.length"),
  };
}

/** 历史或回收站中的可选恢复目标；恢复时仍须复核权限、版本保留和当前修订。 */
export interface RecoveryItem {
  /** 冲突副本对应的原条目身份；普通条目省略。 */
  conflict_source?: string;
  /** 原条目身份。 */
  entry_id: string;
  /** UTC Unix 秒到期下限；当前活跃版本省略，不按文件 mtime。 */
  expires_at?: string;
  /** 固定版本摘要，目录省略。 */
  hash?: string;
  /** 分页稳定身份，历史为版本身份，回收站为条目身份。 */
  id: string;
  /** 当前名称或回收站原相对路径。 */
  name: string;
  /** 原父目录身份。 */
  parent_id?: string;
  /** 当前条目修订，恢复提交的 CAS 基础。 */
  revision: string;
  /** 固定版本字节数，目录省略。 */
  size?: string;
  /** 固定内容版本，目录省略。 */
  version_id?: string;
}

export function decodeRecoveryItem(value: unknown): RecoveryItem {
  const object = expect_object(value, "RecoveryItem");
  return {
    conflict_source: object.conflict_source === undefined ? undefined : expect_string(object.conflict_source, "RecoveryItem.conflict_source"),
    entry_id: expect_string(object.entry_id, "RecoveryItem.entry_id"),
    expires_at: object.expires_at === undefined ? undefined : expect_string(object.expires_at, "RecoveryItem.expires_at"),
    hash: object.hash === undefined ? undefined : expect_string(object.hash, "RecoveryItem.hash"),
    id: expect_string(object.id, "RecoveryItem.id"),
    name: expect_string(object.name, "RecoveryItem.name"),
    parent_id: object.parent_id === undefined ? undefined : expect_string(object.parent_id, "RecoveryItem.parent_id"),
    revision: expect_string(object.revision, "RecoveryItem.revision"),
    size: object.size === undefined ? undefined : expect_string(object.size, "RecoveryItem.size"),
    version_id: object.version_id === undefined ? undefined : expect_string(object.version_id, "RecoveryItem.version_id"),
  };
}

/** 有界恢复项目清单；不是同步状态快照，重复或丢失时重新查询。 */
export interface RecoveryPage {
  /** 按身份升序的一页，最多100条。 */
  items: Array<RecoveryItem>;
  /** 下一页 after 身份；没有下一页时省略。 */
  next_cursor?: string;
}

export function decodeRecoveryPage(value: unknown): RecoveryPage {
  const object = expect_object(value, "RecoveryPage");
  return {
    items: expect_array(object.items, "RecoveryPage.items").map((item) => decodeRecoveryItem(item)),
    next_cursor: object.next_cursor === undefined ? undefined : expect_string(object.next_cursor, "RecoveryPage.next_cursor"),
  };
}

/** 刷新请求（design §15.2 `POST /v1/auth/refresh` 的输入）。 */
export interface RefreshRequest {
  /** 上一次签发的刷新凭据；用一次即失效。 */
  refresh_token: string;
}

export function decodeRefreshRequest(value: unknown): RefreshRequest {
  const object = expect_object(value, "RefreshRequest");
  return {
    refresh_token: expect_string(object.refresh_token, "RefreshRequest.refresh_token"),
  };
}

/** 实际最近一轮扫描进度，不代表客户端已同步。 */
export interface RootCollection {
  /** 未完成原因与本进程观测范围 */
  detail: string;
  /** 最近一轮实际检查项数 */
  examined: string;
  /** 最近一轮待稳定或重试文件数 */
  pending: string;
  /** 采集状态 */
  state: RootCollectionState;
}

export function decodeRootCollection(value: unknown): RootCollection {
  const object = expect_object(value, "RootCollection");
  return {
    detail: expect_string(object.detail, "RootCollection.detail"),
    examined: expect_decimal(object.examined, "RootCollection.examined"),
    pending: expect_decimal(object.pending, "RootCollection.pending"),
    state: decodeRootCollectionState(object.state),
  };
}

/** 本进程观测状态；重启后不沿用旧完成标志。 */
export type RootCollectionState = "not_scanned" | "scanning" | "pending" | "complete" | "blocked";

export const ROOT_COLLECTION_STATE_VALUES = ["not_scanned", "scanning", "pending", "complete", "blocked"] as const;

export function decodeRootCollectionState(value: unknown): RootCollectionState {
  if (typeof value !== "string" || !(ROOT_COLLECTION_STATE_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 RootCollectionState 取值：${JSON.stringify(value)}`);
  }
  return value as RootCollectionState;
}

/**
 * Root 健康状态闭集（design §7.2 `roots.health`）。`unavailable` 表示挂载、身份或权限
 * 核对失败，该 Root 暂停同步但绝不据此推导删除。
 */
export type RootHealth = "healthy" | "unavailable";

export const ROOT_HEALTH_VALUES = ["healthy", "unavailable"] as const;

export function decodeRootHealth(value: unknown): RootHealth {
  if (typeof value !== "string" || !(ROOT_HEALTH_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 RootHealth 取值：${JSON.stringify(value)}`);
  }
  return value as RootHealth;
}

/** 授权 Root 清单（design §15.2 `GET /v1/roots`）。 */
export interface RootList {
  /** 当前调用者可见的 Root，按身份顺序稳定排列。 */
  roots: Array<RootSummary>;
}

export function decodeRootList(value: unknown): RootList {
  const object = expect_object(value, "RootList");
  return {
    roots: expect_array(object.roots, "RootList.roots").map((item) => decodeRootSummary(item)),
  };
}

/**
 * 注册 Root 的输入（design §15.2 `POST /v1/admin/roots`）。这是少数接受服务器绝对路径
 * 的网络接口之一，因此必须同时受允许基目录与存储准入约束。
 */
export interface RootRegisterRequest {
  /** 服务端绝对路径；必须在部署配置的允许基目录内。 */
  path: string;
}

export function decodeRootRegisterRequest(value: unknown): RootRegisterRequest {
  const object = expect_object(value, "RootRegisterRequest");
  return {
    path: expect_string(object.path, "RootRegisterRequest.path"),
  };
}

/** 注册结果（design §15.2）。 */
export interface RootRegistered {
  /** 注册时的健康状态。 */
  health: RootHealth;
  /**
   * 该 Root 的既有内容是否还没被采集。注册不导入既有文件，因此新注册的 Root 恒为
   * true；采集完成后由后续的初始采集步骤推进。
   */
  pending_import: boolean;
  /** 新 Root 的身份，32 个小写十六进制字符。 */
  root_id: string;
}

export function decodeRootRegistered(value: unknown): RootRegistered {
  const object = expect_object(value, "RootRegistered");
  return {
    health: decodeRootHealth(object.health),
    pending_import: expect_boolean(object.pending_import, "RootRegistered.pending_import"),
    root_id: expect_string(object.root_id, "RootRegistered.root_id"),
  };
}

/** 一条授权 Root 的对外摘要；不含服务器绝对路径。 */
export interface RootSummary {
  /** 当前健康状态。 */
  health: RootHealth;
  /** Root 目录末级名称，不含父目录路径。旧服务端可省略；仅用于显示，身份仍使用 root_id。 */
  name?: string;
  /** Root 身份，32 个小写十六进制字符。 */
  root_id: string;
}

export function decodeRootSummary(value: unknown): RootSummary {
  const object = expect_object(value, "RootSummary");
  return {
    health: decodeRootHealth(object.health),
    name: object.name === undefined ? undefined : expect_string(object.name, "RootSummary.name"),
    root_id: expect_string(object.root_id, "RootSummary.root_id"),
  };
}

/**
 * SSE 提示的 `data` 内容（design §9.3）：只告诉客户端 Root 的 RootSeq 已到某个值，
 * 不携带任何批次内容。丢失、重复、乱序都由 `changes` 拉取修复。
 */
export interface SeqHint {
  /** Root 身份，32 位小写十六进制。 */
  root_id: string;
  /** 提示发出时 Root 的最新 RootSeq（十进制文本）。 */
  seq: string;
}

export function decodeSeqHint(value: unknown): SeqHint {
  const object = expect_object(value, "SeqHint");
  return {
    root_id: expect_string(object.root_id, "SeqHint.root_id"),
    seq: expect_decimal(object.seq, "SeqHint.seq"),
  };
}

/** 会话凭据对与有效期；除 `session_id` 外都是秘密，不得写日志或落到前端 localStorage。 */
export interface SessionIssued {
  /** 访问凭据到期时间（UTC Unix 秒的十进制文本）。 */
  access_expires_at: string;
  /** 访问凭据，放在 Authorization 头的 Bearer 方案里，短期有效。 */
  access_token: string;
  /** 刷新凭据到期时间（UTC Unix 秒的十进制文本）。 */
  refresh_expires_at: string;
  /** 刷新凭据，只能用于 `/v1/auth/refresh` 且用一次即失效。 */
  refresh_token: string;
  /** 会话身份，32 个小写十六进制字符。 */
  session_id: string;
}

export function decodeSessionIssued(value: unknown): SessionIssued {
  const object = expect_object(value, "SessionIssued");
  return {
    access_expires_at: expect_decimal(object.access_expires_at, "SessionIssued.access_expires_at"),
    access_token: expect_string(object.access_token, "SessionIssued.access_token"),
    refresh_expires_at: expect_decimal(object.refresh_expires_at, "SessionIssued.refresh_expires_at"),
    refresh_token: expect_string(object.refresh_token, "SessionIssued.refresh_token"),
    session_id: expect_string(object.session_id, "SessionIssued.session_id"),
  };
}

/** 注销结果。 */
export interface SessionRevoked {
  /** 失效时间（UTC Unix 秒的十进制文本）。 */
  revoked_at: string;
  /** 被失效的会话身份，32 个小写十六进制字符。 */
  session_id: string;
}

export function decodeSessionRevoked(value: unknown): SessionRevoked {
  const object = expect_object(value, "SessionRevoked");
  return {
    revoked_at: expect_decimal(object.revoked_at, "SessionRevoked.revoked_at"),
    session_id: expect_string(object.session_id, "SessionRevoked.session_id"),
  };
}

/**
 * 一份一致性快照的元数据（design §9.3）。`high_water_seq` 是创建时刻 Root 的 RootSeq：清单恰好是
 * “应用到该 seq 为止”的状态，继续追赶从 `changes?after=<high_water_seq>` 开始。
 */
export interface Snapshot {
  /** 清单里的条目总数（十进制文本）。 */
  entry_count: string;
  /** 清单到期时间（UTC Unix 秒的十进制文本）；此刻及之后翻页返回 410。 */
  expires_at: string;
  /** 创建快照时的 RootSeq（十进制文本）。 */
  high_water_seq: string;
  /** 所属 Root 身份，32 位小写十六进制。 */
  root_id: string;
  /** 创建快照时 Root 的服务器 epoch（十进制文本）。 */
  server_epoch: string;
  /** 快照身份，32 位小写十六进制。 */
  snapshot_id: string;
}

export function decodeSnapshot(value: unknown): Snapshot {
  const object = expect_object(value, "Snapshot");
  return {
    entry_count: expect_decimal(object.entry_count, "Snapshot.entry_count"),
    expires_at: expect_decimal(object.expires_at, "Snapshot.expires_at"),
    high_water_seq: expect_decimal(object.high_water_seq, "Snapshot.high_water_seq"),
    root_id: expect_string(object.root_id, "Snapshot.root_id"),
    server_epoch: expect_decimal(object.server_epoch, "Snapshot.server_epoch"),
    snapshot_id: expect_string(object.snapshot_id, "Snapshot.snapshot_id"),
  };
}

/** 快照清单里的一条条目（创建时的值拷贝，不随之后的写入变化）。 */
export interface SnapshotEntry {
  /** 条目身份，32 位小写十六进制。 */
  entry_id: string;
  /** 当前内容的 SHA-256，64 位小写十六进制（只有带内容的文件有）。 */
  hash?: string;
  /** 文件或目录。 */
  kind: EntryKind;
  /** 原始名称（合成根条目为空串）；按数据处理。 */
  name: string;
  /** 父目录条目身份；Root 的合成根条目没有。 */
  parent_id?: string;
  /** 创建快照时的条目 revision（十进制文本）。 */
  revision: string;
  /** 当前内容字节数（十进制文本，只有带内容的文件有）。 */
  size?: string;
  /** 当前内容版本身份（只有带内容的文件有）；下载内容引用它而不是路径。 */
  version_id?: string;
}

export function decodeSnapshotEntry(value: unknown): SnapshotEntry {
  const object = expect_object(value, "SnapshotEntry");
  return {
    entry_id: expect_string(object.entry_id, "SnapshotEntry.entry_id"),
    hash: object.hash === undefined ? undefined : expect_string(object.hash, "SnapshotEntry.hash"),
    kind: decodeEntryKind(object.kind),
    name: expect_string(object.name, "SnapshotEntry.name"),
    parent_id: object.parent_id === undefined ? undefined : expect_string(object.parent_id, "SnapshotEntry.parent_id"),
    revision: expect_decimal(object.revision, "SnapshotEntry.revision"),
    size: object.size === undefined ? undefined : expect_decimal(object.size, "SnapshotEntry.size"),
    version_id: object.version_id === undefined ? undefined : expect_string(object.version_id, "SnapshotEntry.version_id"),
  };
}

/** 快照清单的一页。 */
export interface SnapshotPage {
  /** 本页条目，按 EntryId 升序。 */
  entries: Array<SnapshotEntry>;
  /** 快照固定的 RootSeq（十进制文本，所有页相同）。 */
  high_water_seq: string;
  /** 下一页的游标（十进制文本）；缺省表示已是最后一页。 */
  next_cursor?: string;
  /** 快照身份，32 位小写十六进制。 */
  snapshot_id: string;
}

export function decodeSnapshotPage(value: unknown): SnapshotPage {
  const object = expect_object(value, "SnapshotPage");
  return {
    entries: expect_array(object.entries, "SnapshotPage.entries").map((item) => decodeSnapshotEntry(item)),
    high_water_seq: expect_decimal(object.high_water_seq, "SnapshotPage.high_water_seq"),
    next_cursor: object.next_cursor === undefined ? undefined : expect_decimal(object.next_cursor, "SnapshotPage.next_cursor"),
    snapshot_id: expect_string(object.snapshot_id, "SnapshotPage.snapshot_id"),
  };
}

/**
 * 目录子树的服务端乐观并发证明（design §10.3、T12）。`token` 覆盖目标条目自身与全部活跃后代
 * 的身份、父目录、名称、种类与 revision；任一变化都会得到不同的 token。删除时原样回传。
 */
export interface SubtreeToken {
  /** 冻结清单里的条目数（含目标自身，十进制文本）。 */
  entries: string;
  /** 目录条目身份，32 位小写十六进制。 */
  entry_id: string;
  /** Root 身份，32 位小写十六进制。 */
  root_id: string;
  /** 子树 token，32 位小写十六进制。 */
  token: string;
}

export function decodeSubtreeToken(value: unknown): SubtreeToken {
  const object = expect_object(value, "SubtreeToken");
  return {
    entries: expect_decimal(object.entries, "SubtreeToken.entries"),
    entry_id: expect_string(object.entry_id, "SubtreeToken.entry_id"),
    root_id: expect_string(object.root_id, "SubtreeToken.root_id"),
    token: expect_string(object.token, "SubtreeToken.token"),
  };
}

/** 块登记结果。 */
export interface UploadPartAck {
  /** 是否为幂等重传（同一索引、同一字节已经登记过）。 */
  already_present: boolean;
  /** 块内容的 SHA-256，64 位小写十六进制。 */
  hash: string;
  /** 块索引（十进制文本）。 */
  index: string;
  /** 块长度（十进制文本）。 */
  length: string;
  /** 登记后的会话状态（块齐之前保持 `open`）。 */
  state: UploadState;
  /** 上传会话身份，32 位小写十六进制。 */
  upload_id: string;
}

export function decodeUploadPartAck(value: unknown): UploadPartAck {
  const object = expect_object(value, "UploadPartAck");
  return {
    already_present: expect_boolean(object.already_present, "UploadPartAck.already_present"),
    hash: expect_string(object.hash, "UploadPartAck.hash"),
    index: expect_decimal(object.index, "UploadPartAck.index"),
    length: expect_decimal(object.length, "UploadPartAck.length"),
    state: decodeUploadState(object.state),
    upload_id: expect_string(object.upload_id, "UploadPartAck.upload_id"),
  };
}

/** 创建上传会话的输入（design §15.2 `POST /v1/roots/{root}/uploads`）。 */
export interface UploadRequest {
  /**
   * 客户端希望的块大小（十进制文本）。0 表示由服务端按能力选择；服务端可以把它调到
   * 自己的上限以内，实际值随会话返回。
   */
  chunk_size: string;
  /** 整份内容的 SHA-256，64 位小写十六进制。 */
  hash: string;
  /** 不可变本地快照的总字节数（十进制文本）。 */
  total_size: string;
}

export function decodeUploadRequest(value: unknown): UploadRequest {
  const object = expect_object(value, "UploadRequest");
  return {
    chunk_size: expect_decimal(object.chunk_size, "UploadRequest.chunk_size"),
    hash: expect_string(object.hash, "UploadRequest.hash"),
    total_size: expect_decimal(object.total_size, "UploadRequest.total_size"),
  };
}

/** 上传会话（design §13.1：绑定总大小、总哈希与块几何）。 */
export interface UploadSession {
  /** 本会话实际使用的块大小（十进制文本）。 */
  chunk_size: string;
  /** 会话到期时间（UTC Unix 秒的十进制文本）。 */
  expires_at: string;
  /** 整份内容的 SHA-256，64 位小写十六进制。 */
  hash: string;
  /** 块数（空内容为 0）。 */
  part_count: string;
  /** 所属 Root 身份，32 位小写十六进制。 */
  root_id: string;
  /** 会话状态；客户端据此决定继续传块、查询结果还是放弃。 */
  state: UploadState;
  /** 快照总字节数（十进制文本）。 */
  total_size: string;
  /** 上传会话身份，32 位小写十六进制。 */
  upload_id: string;
}

export function decodeUploadSession(value: unknown): UploadSession {
  const object = expect_object(value, "UploadSession");
  return {
    chunk_size: expect_decimal(object.chunk_size, "UploadSession.chunk_size"),
    expires_at: expect_decimal(object.expires_at, "UploadSession.expires_at"),
    hash: expect_string(object.hash, "UploadSession.hash"),
    part_count: expect_decimal(object.part_count, "UploadSession.part_count"),
    root_id: expect_string(object.root_id, "UploadSession.root_id"),
    state: decodeUploadState(object.state),
    total_size: expect_decimal(object.total_size, "UploadSession.total_size"),
    upload_id: expect_string(object.upload_id, "UploadSession.upload_id"),
  };
}

/**
 * 上传会话状态闭集。未知取值按协议错误处理；`open` 接收块，`complete` 只等待
 * finalize（S046），`finalized`/`aborted` 是终态。
 */
export type UploadState = "open" | "complete" | "finalized" | "aborted";

export const UPLOAD_STATE_VALUES = ["open", "complete", "finalized", "aborted"] as const;

export function decodeUploadState(value: unknown): UploadState {
  if (typeof value !== "string" || !(UPLOAD_STATE_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 UploadState 取值：${JSON.stringify(value)}`);
  }
  return value as UploadState;
}

/** 续传依据：会话状态加**服务端真实已收块**（design §13.2）。 */
export interface UploadStatus {
  /**
   * 仍缺失的块区间，按索引升序；空表示块已齐。区间而不是逐块列表，因此长度最多是
   * “已收块数 + 1”，不随文件大小增长。
   */
  missing: Array<MissingRange>;
  /** 已收到的块，按索引升序。 */
  received: Array<ReceivedPart>;
  /** 会话状态与块几何。 */
  upload: UploadSession;
}

export function decodeUploadStatus(value: unknown): UploadStatus {
  const object = expect_object(value, "UploadStatus");
  return {
    missing: expect_array(object.missing, "UploadStatus.missing").map((item) => decodeMissingRange(item)),
    received: expect_array(object.received, "UploadStatus.received").map((item) => decodeReceivedPart(item)),
    upload: decodeUploadSession(object.upload),
  };
}

/** 服务器持久化的固定版本保护；expires_at是UTC秒，ttl_seconds是剩余有效秒数。 */
export interface VersionLease {
  /** 显式解除后为true，不可重新续期。 */
  closed: boolean;
  /** 固定Entry身份。 */
  entry_id: string;
  /** UTC到期秒数。 */
  expires_at: string;
  /** 请求租约身份。 */
  lease_id: string;
  /** 固定Root身份。 */
  root_id: string;
  /** 整份内容SHA256。 */
  sha256: string;
  /** 固定版本字节数。 */
  size: string;
  /** 响应形成时剩余秒数，最大120。 */
  ttl_seconds: string;
  /** 固定版本身份。 */
  version_id: string;
}

export function decodeVersionLease(value: unknown): VersionLease {
  const object = expect_object(value, "VersionLease");
  return {
    closed: expect_boolean(object.closed, "VersionLease.closed"),
    entry_id: expect_string(object.entry_id, "VersionLease.entry_id"),
    expires_at: expect_decimal(object.expires_at, "VersionLease.expires_at"),
    lease_id: expect_string(object.lease_id, "VersionLease.lease_id"),
    root_id: expect_string(object.root_id, "VersionLease.root_id"),
    sha256: expect_string(object.sha256, "VersionLease.sha256"),
    size: expect_decimal(object.size, "VersionLease.size"),
    ttl_seconds: expect_decimal(object.ttl_seconds, "VersionLease.ttl_seconds"),
    version_id: expect_string(object.version_id, "VersionLease.version_id"),
  };
}

/** 固定版本内容租约的稳定请求；身份由调用端随机生成并在重试时复用。 */
export interface VersionLeaseRequest {
  /** 32位小写十六进制租约请求身份。 */
  lease_id: string;
  /** 已提交版本的整份SHA256，小写十六进制。 */
  sha256: string;
  /** 已提交版本的逻辑字节数。 */
  size: string;
}

export function decodeVersionLeaseRequest(value: unknown): VersionLeaseRequest {
  const object = expect_object(value, "VersionLeaseRequest");
  return {
    lease_id: expect_string(object.lease_id, "VersionLeaseRequest.lease_id"),
    sha256: expect_string(object.sha256, "VersionLeaseRequest.sha256"),
    size: expect_decimal(object.size, "VersionLeaseRequest.size"),
  };
}
