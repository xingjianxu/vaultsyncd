// 本文件由 `cargo xtask generate` 从 `api/openapi.yaml` 生成，禁止手改。
//
// 契约：vaultsyncd remote API 0.1.0；生成器：vaultsyncd-xtask 0.1.0
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

function expect_array(value: unknown, what: string): unknown[] {
  if (!Array.isArray(value)) {
    throw new Error(`期望 ${what} 是数组`);
  }
  return value;
}

function expect_string(value: unknown, what: string): string {
  if (typeof value !== "string") {
    throw new Error(`期望 ${what} 是字符串`);
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

function expect_boolean(value: unknown, what: string): boolean {
  if (typeof value !== "boolean") {
    throw new Error(`期望 ${what} 是布尔值`);
  }
  return value;
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
 */
export type ErrorCode = "AUTH_EXPIRED" | "ACCESS_DENIED" | "BASE_MISMATCH" | "SUBTREE_CHANGED" | "OP_ID_REUSED" | "RECONCILE_REQUIRED" | "CURSOR_EXPIRED" | "EPOCH_CHANGED" | "VERSION_GONE" | "INVALID_NAME" | "INVALID_REQUEST" | "UNSUPPORTED_ENTRY" | "HASH_MISMATCH" | "PART_CONFLICT" | "UNSUPPORTED_STORAGE_BACKEND" | "SERVER_PLACEHOLDER_UNSUPPORTED" | "RATE_LIMITED" | "ROOT_UNAVAILABLE" | "NEEDS_RECOVERY" | "INSUFFICIENT_STORAGE";

export const ERROR_CODE_VALUES = ["AUTH_EXPIRED", "ACCESS_DENIED", "BASE_MISMATCH", "SUBTREE_CHANGED", "OP_ID_REUSED", "RECONCILE_REQUIRED", "CURSOR_EXPIRED", "EPOCH_CHANGED", "VERSION_GONE", "INVALID_NAME", "INVALID_REQUEST", "UNSUPPORTED_ENTRY", "HASH_MISMATCH", "PART_CONFLICT", "UNSUPPORTED_STORAGE_BACKEND", "SERVER_PLACEHOLDER_UNSUPPORTED", "RATE_LIMITED", "ROOT_UNAVAILABLE", "NEEDS_RECOVERY", "INSUFFICIENT_STORAGE"] as const;

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
  /** Root 身份，32 个小写十六进制字符。 */
  root_id: string;
}

export function decodeRootSummary(value: unknown): RootSummary {
  const object = expect_object(value, "RootSummary");
  return {
    health: decodeRootHealth(object.health),
    root_id: expect_string(object.root_id, "RootSummary.root_id"),
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
