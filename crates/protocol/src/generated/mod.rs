//! 本文件由 `cargo xtask generate` 从 `api/openapi.yaml` 生成，禁止手改。
//!
//! 契约：vaultsyncd remote API 0.1.0；生成器：vaultsyncd-xtask 0.1.0
//! 服务端远端接口的首发草案（/v1）。JSON 使用 snake_case；所有清单分页有上限；未知枚举
//! 不当作默认成功。健康探测与能力协商不需要身份认证，其余端点需要会话，按 design §15.2
//! 在对应功能卡加入。
//!
//! 生成代码只做 wire 形态：闭集枚举的未知取值在反序列化时报错，十进制长整数只接受
//! 规范文本（见 `crate::decimal`）；domain 类型到 wire 的转换在 `crate::domain_map`。

use std::collections::BTreeMap;

/// 协议版本、服务器身份、epoch、功能集合与限额（design §15.2）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Capabilities {
    /// 服务器 epoch 的十进制文本。恢复后更换，客户端据此丢弃旧 epoch 的响应。
    #[serde(with = "crate::decimal")]
    pub epoch: String,
    /// 服务端支持的功能标记。客户端只使用自己认识的值，未识别的标记按“该功能不可用”
    /// 处理；这里是可扩展标记集合，不是闭集枚举，因此不套用“未知枚举即失败”。
    pub features: Vec<String>,
    /// 客户端必须遵守的服务端限额。
    pub limits: CapabilityLimits,
    /// 协议主次版本。
    pub protocol: ProtocolVersion,
    /// 服务器身份，32 个小写十六进制字符（design §7.1）。
    pub server_id: String,
}

/// 客户端必须遵守的服务端限额。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct CapabilityLimits {
    /// 清单分页的每页上限。（取值范围 1..=10000）
    pub max_page_size: u32,
    /// 单个上传块的最大字节数（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub max_upload_part_bytes: String,
}

/// 错误主体。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ErrorBody {
    /// 闭集错误码；未知码不是可重试错误，按协议错误处理。
    pub code: ErrorCode,
    /// 结构化补充信息，例如 `{"reason": "mount_identity_changed"}`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<BTreeMap<String, String>>,
    /// 面向用户的可理解原因，按数据处理，不当作指令。
    pub message: String,
    /// 服务端记录的请求标识，供日志关联。
    pub request_id: String,
    /// 同一请求稍后原样重试是否可能成功。
    pub retryable: bool,
    /// 相关 Root 的身份（32 个小写十六进制字符）；与 Root 无关时省略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_id: Option<String>,
}

/// 远端错误码闭集（design §15.4）。未知错误码不能被当成可重试或成功；本机 IPC 专用的
/// 错误码（LOCAL_CHANGED、FILE_BUSY、MODE_REQUIRES_LOCAL_COPY、PLACEHOLDER_OFFLINE）
/// 属于 packages/local-ipc-types，不在远端契约内。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum ErrorCode {
    /// 取值 `AUTH_EXPIRED`。
    #[serde(rename = "AUTH_EXPIRED")]
    AuthExpired,
    /// 取值 `ACCESS_DENIED`。
    #[serde(rename = "ACCESS_DENIED")]
    AccessDenied,
    /// 取值 `BASE_MISMATCH`。
    #[serde(rename = "BASE_MISMATCH")]
    BaseMismatch,
    /// 取值 `SUBTREE_CHANGED`。
    #[serde(rename = "SUBTREE_CHANGED")]
    SubtreeChanged,
    /// 取值 `OP_ID_REUSED`。
    #[serde(rename = "OP_ID_REUSED")]
    OpIdReused,
    /// 取值 `RECONCILE_REQUIRED`。
    #[serde(rename = "RECONCILE_REQUIRED")]
    ReconcileRequired,
    /// 取值 `CURSOR_EXPIRED`。
    #[serde(rename = "CURSOR_EXPIRED")]
    CursorExpired,
    /// 取值 `EPOCH_CHANGED`。
    #[serde(rename = "EPOCH_CHANGED")]
    EpochChanged,
    /// 取值 `VERSION_GONE`。
    #[serde(rename = "VERSION_GONE")]
    VersionGone,
    /// 取值 `INVALID_NAME`。
    #[serde(rename = "INVALID_NAME")]
    InvalidName,
    /// 取值 `INVALID_REQUEST`。
    #[serde(rename = "INVALID_REQUEST")]
    InvalidRequest,
    /// 取值 `UNSUPPORTED_ENTRY`。
    #[serde(rename = "UNSUPPORTED_ENTRY")]
    UnsupportedEntry,
    /// 取值 `HASH_MISMATCH`。
    #[serde(rename = "HASH_MISMATCH")]
    HashMismatch,
    /// 取值 `PART_CONFLICT`。
    #[serde(rename = "PART_CONFLICT")]
    PartConflict,
    /// 取值 `UNSUPPORTED_STORAGE_BACKEND`。
    #[serde(rename = "UNSUPPORTED_STORAGE_BACKEND")]
    UnsupportedStorageBackend,
    /// 取值 `SERVER_PLACEHOLDER_UNSUPPORTED`。
    #[serde(rename = "SERVER_PLACEHOLDER_UNSUPPORTED")]
    ServerPlaceholderUnsupported,
    /// 取值 `RATE_LIMITED`。
    #[serde(rename = "RATE_LIMITED")]
    RateLimited,
    /// 取值 `ROOT_UNAVAILABLE`。
    #[serde(rename = "ROOT_UNAVAILABLE")]
    RootUnavailable,
    /// 取值 `NEEDS_RECOVERY`。
    #[serde(rename = "NEEDS_RECOVERY")]
    NeedsRecovery,
    /// 取值 `INSUFFICIENT_STORAGE`。
    #[serde(rename = "INSUFFICIENT_STORAGE")]
    InsufficientStorage,
}

/// 错误信封（design §15.4）：所有非 2xx 响应都用它，客户端据此决定重试与暂停。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ErrorEnvelope {
    /// 错误主体。
    pub error: ErrorBody,
}

/// finalize 的结果：可提交内容引用（design §15.2 上传 finalize「只生成可提交内容引用，
/// 不修改条目 head」）。它还不是版本：条目 head 由后续操作按 CAS 提交。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct FinalizedContent {
    /// 内容的 SHA-256，64 位小写十六进制；提交时用它引用这份内容。
    pub hash: String,
    /// 块数（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub part_count: String,
    /// 是否为幂等重放（会话本来就已经 `finalized`）。
    pub replayed: bool,
    /// 本次新建还是复用既有对象。
    pub reuse: ObjectReuse,
    /// 内容所属 Root 身份，32 位小写十六进制。
    pub root_id: String,
    /// 内容字节数（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub size: String,
    /// 对象区内的规范相对路径（服务端内部路径）。客户端只原样回传，不拼接、不解释它。
    pub storage_path: String,
    /// 上传会话身份，32 位小写十六进制。
    pub upload_id: String,
}

/// 健康探测结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct HealthResponse {
    /// 服务端当前健康状态。
    pub status: HealthStatus,
}

/// 健康状态闭集。客户端遇到未列出的值必须按协议错误处理，不能当作可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum HealthStatus {
    /// 取值 `ok`。
    #[serde(rename = "ok")]
    Ok,
    /// 取值 `degraded`。
    #[serde(rename = "degraded")]
    Degraded,
    /// 取值 `needs_recovery`。
    #[serde(rename = "needs_recovery")]
    NeedsRecovery,
}

/// 登录请求（design §15.2 `POST /v1/auth/login` 的输入）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct LoginRequest {
    /// 设备的人类可读标签（不是设备身份，首发没有设备注册表）。
    pub device_label: String,
    /// 登录名（原样比较，不做大小写折叠）。
    pub login: String,
    /// 明文密码；只在本次请求内存在，服务端不记录。
    pub password: String,
}

/// 一段连续缺失的块索引。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct MissingRange {
    /// 区间内的块数，至少为 1。
    #[serde(with = "crate::decimal")]
    pub count: String,
    /// 区间起点（含），十进制文本。
    #[serde(with = "crate::decimal")]
    pub first_index: String,
}

/// 内容对象的发布方式闭集（design §11.2 步骤 7）。`freshly_published` 本次新建，
/// `existing_verified` 内容地址上已有同内容对象、未覆盖并复核通过后复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum ObjectReuse {
    /// 取值 `freshly_published`。
    #[serde(rename = "freshly_published")]
    FreshlyPublished,
    /// 取值 `existing_verified`。
    #[serde(rename = "existing_verified")]
    ExistingVerified,
}

/// 协议主次版本；主版本不同必须拒绝通信。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ProtocolVersion {
    /// 主版本；与服务端不同即拒绝通信。（取值范围 0..=65535）
    pub major: u32,
    /// 次版本；服务端可以更大，客户端按自己认识的功能使用。（取值范围 0..=65535）
    pub minor: u32,
}

/// 服务端已持久化的一个块。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ReceivedPart {
    /// 块内容的 SHA-256，64 位小写十六进制。
    pub hash: String,
    /// 块索引（从 0 起，十进制文本）。
    #[serde(with = "crate::decimal")]
    pub index: String,
    /// 块长度（十进制文本），与块几何一致。
    #[serde(with = "crate::decimal")]
    pub length: String,
}

/// 刷新请求（design §15.2 `POST /v1/auth/refresh` 的输入）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct RefreshRequest {
    /// 上一次签发的刷新凭据；用一次即失效。
    pub refresh_token: String,
}

/// Root 健康状态闭集（design §7.2 `roots.health`）。`unavailable` 表示挂载、身份或权限
/// 核对失败，该 Root 暂停同步但绝不据此推导删除。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum RootHealth {
    /// 取值 `healthy`。
    #[serde(rename = "healthy")]
    Healthy,
    /// 取值 `unavailable`。
    #[serde(rename = "unavailable")]
    Unavailable,
}

/// 授权 Root 清单（design §15.2 `GET /v1/roots`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct RootList {
    /// 当前调用者可见的 Root，按身份顺序稳定排列。
    pub roots: Vec<RootSummary>,
}

/// 注册 Root 的输入（design §15.2 `POST /v1/admin/roots`）。这是少数接受服务器绝对路径
/// 的网络接口之一，因此必须同时受允许基目录与存储准入约束。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct RootRegisterRequest {
    /// 服务端绝对路径；必须在部署配置的允许基目录内。
    pub path: String,
}

/// 注册结果（design §15.2）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct RootRegistered {
    /// 注册时的健康状态。
    pub health: RootHealth,
    /// 该 Root 的既有内容是否还没被采集。注册不导入既有文件，因此新注册的 Root 恒为
    /// true；采集完成后由后续的初始采集步骤推进。
    pub pending_import: bool,
    /// 新 Root 的身份，32 个小写十六进制字符。
    pub root_id: String,
}

/// 一条授权 Root 的对外摘要；不含服务器绝对路径。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct RootSummary {
    /// 当前健康状态。
    pub health: RootHealth,
    /// Root 身份，32 个小写十六进制字符。
    pub root_id: String,
}

/// 会话凭据对与有效期；除 `session_id` 外都是秘密，不得写日志或落到前端 localStorage。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct SessionIssued {
    /// 访问凭据到期时间（UTC Unix 秒的十进制文本）。
    #[serde(with = "crate::decimal")]
    pub access_expires_at: String,
    /// 访问凭据，放在 Authorization 头的 Bearer 方案里，短期有效。
    pub access_token: String,
    /// 刷新凭据到期时间（UTC Unix 秒的十进制文本）。
    #[serde(with = "crate::decimal")]
    pub refresh_expires_at: String,
    /// 刷新凭据，只能用于 `/v1/auth/refresh` 且用一次即失效。
    pub refresh_token: String,
    /// 会话身份，32 个小写十六进制字符。
    pub session_id: String,
}

/// 注销结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct SessionRevoked {
    /// 失效时间（UTC Unix 秒的十进制文本）。
    #[serde(with = "crate::decimal")]
    pub revoked_at: String,
    /// 被失效的会话身份，32 个小写十六进制字符。
    pub session_id: String,
}

/// 块登记结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct UploadPartAck {
    /// 是否为幂等重传（同一索引、同一字节已经登记过）。
    pub already_present: bool,
    /// 块内容的 SHA-256，64 位小写十六进制。
    pub hash: String,
    /// 块索引（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub index: String,
    /// 块长度（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub length: String,
    /// 登记后的会话状态（块齐之前保持 `open`）。
    pub state: UploadState,
    /// 上传会话身份，32 位小写十六进制。
    pub upload_id: String,
}

/// 创建上传会话的输入（design §15.2 `POST /v1/roots/{root}/uploads`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct UploadRequest {
    /// 客户端希望的块大小（十进制文本）。0 表示由服务端按能力选择；服务端可以把它调到
    /// 自己的上限以内，实际值随会话返回。
    #[serde(with = "crate::decimal")]
    pub chunk_size: String,
    /// 整份内容的 SHA-256，64 位小写十六进制。
    pub hash: String,
    /// 不可变本地快照的总字节数（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub total_size: String,
}

/// 上传会话（design §13.1：绑定总大小、总哈希与块几何）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct UploadSession {
    /// 本会话实际使用的块大小（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub chunk_size: String,
    /// 会话到期时间（UTC Unix 秒的十进制文本）。
    #[serde(with = "crate::decimal")]
    pub expires_at: String,
    /// 整份内容的 SHA-256，64 位小写十六进制。
    pub hash: String,
    /// 块数（空内容为 0）。
    #[serde(with = "crate::decimal")]
    pub part_count: String,
    /// 所属 Root 身份，32 位小写十六进制。
    pub root_id: String,
    /// 会话状态；客户端据此决定继续传块、查询结果还是放弃。
    pub state: UploadState,
    /// 快照总字节数（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub total_size: String,
    /// 上传会话身份，32 位小写十六进制。
    pub upload_id: String,
}

/// 上传会话状态闭集。未知取值按协议错误处理；`open` 接收块，`complete` 只等待
/// finalize（S046），`finalized`/`aborted` 是终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum UploadState {
    /// 取值 `open`。
    #[serde(rename = "open")]
    Open,
    /// 取值 `complete`。
    #[serde(rename = "complete")]
    Complete,
    /// 取值 `finalized`。
    #[serde(rename = "finalized")]
    Finalized,
    /// 取值 `aborted`。
    #[serde(rename = "aborted")]
    Aborted,
}

/// 续传依据：会话状态加**服务端真实已收块**（design §13.2）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct UploadStatus {
    /// 仍缺失的块区间，按索引升序；空表示块已齐。区间而不是逐块列表，因此长度最多是
    /// “已收块数 + 1”，不随文件大小增长。
    pub missing: Vec<MissingRange>,
    /// 已收到的块，按索引升序。
    pub received: Vec<ReceivedPart>,
    /// 会话状态与块几何。
    pub upload: UploadSession,
}
