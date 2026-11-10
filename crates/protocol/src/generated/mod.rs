//! 本文件由 `cargo xtask generate` 从 `api/openapi.yaml` 生成，禁止手改。
//!
//! 契约：vaultsyncd remote API 0.1.0；生成器：vaultsyncd-xtask 0.2.0
//! 服务端远端接口的首发草案（/v1）。JSON 使用 snake_case；所有清单分页有上限；未知枚举
//! 不当作默认成功。健康探测与能力协商不需要身份认证，其余端点需要会话，按 design §15.2
//! 在对应功能卡加入。
//!
//! 生成代码只做 wire 形态：闭集枚举的未知取值在反序列化时报错，十进制长整数只接受
//! 规范文本（见 `crate::decimal`）；domain 类型到 wire 的转换在 `crate::domain_map`。

use std::collections::BTreeMap;

/// 账号操作完成且全部会话已撤销，必须重新登录。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct AccountChanged {
    /// 本次撤销的会话数
    #[serde(with = "crate::decimal")]
    pub sessions_revoked: String,
}

/// 当前已认证账号，不含凭据。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct AccountInfo {
    /// 登录名
    pub login: String,
    /// 会话身份，不是凭据
    pub session_id: String,
    /// 账号身份
    pub user_id: String,
}

/// 按记录身份倒序的一页活动，最多50条。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityPage {
    /// 本页活动
    pub items: Vec<ActivityRecord>,
    /// 下一页before游标，末页缺省
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// 启用记录后的文件同步与传输证据，不代表客户端下载完成。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityRecord {
    /// upload_finalize或sync_commit或download
    pub action: String,
    /// 本次内容字节数
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<String>,
    /// 稳定登录连接身份
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// 登录入口client或browser或unknown
    pub client_kind: String,
    /// 客户端登录标签快照
    pub client_label: String,
    /// 操作声明的设备身份，不代表认证设备
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// upload或download，按客户端方向
    pub direction: String,
    /// 条目身份
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// 失败分类
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// 记录身份，正十进制文本
    pub id: String,
    /// 观测UTC秒
    pub occurred_at: String,
    /// 本次下载起始偏移
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<String>,
    /// 幂等操作身份
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// 事件发生时相对路径快照
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 完整文件字节数
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// ready或committed或served或failed
    pub status: String,
    /// 上传会话身份
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_id: Option<String>,
    /// 固定内容版本
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

/// 固定源内容的完整性，不能代表客户端目标已持久导出。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum BackupIntegrity {
    /// 取值 `unverified`。
    #[serde(rename = "unverified")]
    Unverified,
    /// 取值 `verified`。
    #[serde(rename = "verified")]
    Verified,
    /// 取值 `incomplete`。
    #[serde(rename = "incomplete")]
    Incomplete,
}

/// 备份管理的BackupPrepareRequest契约
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct BackupPrepareRequest {
    /// 调用方稳定请求身份；重试复用。
    pub backup_id: String,
}

/// 备份管理的BackupSnapshotList契约
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct BackupSnapshotList {
    /// 当前页备份状态
    pub items: Vec<BackupSnapshotStatus>,
    /// 下一页号，缺省表示结束
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page: Option<String>,
}

/// 源快照准备/释放阶段；不会把pending当complete。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum BackupSnapshotPhase {
    /// 取值 `preparing`。
    #[serde(rename = "preparing")]
    Preparing,
    /// 取值 `ready`。
    #[serde(rename = "ready")]
    Ready,
    /// 取值 `failed`。
    #[serde(rename = "failed")]
    Failed,
    /// 取值 `releasing`。
    #[serde(rename = "releasing")]
    Releasing,
    /// 取值 `released`。
    #[serde(rename = "released")]
    Released,
    /// 取值 `expired`。
    #[serde(rename = "expired")]
    Expired,
}

/// 备份管理的BackupSnapshotStatus契约
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct BackupSnapshotStatus {
    /// 稳定备份身份，32位小写十六进制
    pub backup_id: String,
    /// 创建UTC Unix秒
    #[serde(with = "crate::decimal")]
    pub created_at: String,
    /// 当前原因或错误说明
    pub detail: String,
    /// 租约到期UTC Unix秒
    #[serde(with = "crate::decimal")]
    pub expires_at: String,
    /// 源快照完整性，独立于客户端导出
    pub integrity: BackupIntegrity,
    /// 持久作业是否尚未结束
    pub job_pending: bool,
    /// 固定manifest SHA-256，准备成功后才提供
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_hash: Option<String>,
    /// 快照准备或释放阶段
    pub phase: BackupSnapshotPhase,
}

/// 浏览器只读会话元信息；凭据只在 HttpOnly cookie 中。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct BrowserSession {
    /// 访问会话到期 UTC Unix 秒
    #[serde(with = "crate::decimal")]
    pub access_expires_at: String,
    /// 会话身份，不是凭据
    pub session_id: String,
}

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

/// 一个事务批次（design §9.3）：VaultSeq、TxId 与该事务的全部条目变化。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ChangeBatch {
    /// 批次提交时间（UTC Unix 秒的十进制文本）。
    #[serde(with = "crate::decimal")]
    pub created_at: String,
    /// 该事务全部条目变化与冲突关联的 JSON 数组文本；按数据处理，客户端整批原子应用。
    pub events_json: String,
    /// 批次序号（该 Vault 的第 seq 个事务批次，十进制文本）。
    #[serde(with = "crate::decimal")]
    pub seq: String,
    /// 产生该批次的服务端事务身份，32 位小写十六进制。
    pub tx_id: String,
}

/// 一页有序事务批次。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ChangeList {
    /// 批次，严格按 seq 升序，每个批次完整。
    pub batches: Vec<ChangeBatch>,
    /// 读取时 `next_after` 之后是否还有批次。
    pub has_more: bool,
    /// 读取时 Vault 的最新 VaultSeq（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub head_seq: String,
    /// 下一次拉取应带的 `after`：本页最后一个批次的 seq；本页为空时等于请求的 `after`。
    #[serde(with = "crate::decimal")]
    pub next_after: String,
    /// Vault 当前的服务器 epoch（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub server_epoch: String,
    /// Vault 身份，32 位小写十六进制。
    pub vault_id: String,
}

/// 当前账号一次登录的连接记录，不代表可信设备身份或同步状态。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ClientConnection {
    /// 接入类型
    pub client_kind: ClientConnectionClientKind,
    /// 跨凭据刷新稳定的连接身份
    pub connection_id: String,
    /// 首次登录UTC秒
    #[serde(with = "crate::decimal")]
    pub created_at: String,
    /// 是否为本次请求使用的连接
    pub current: bool,
    /// 登录提交的非可信设备标签
    pub device_label: String,
    /// 最近成功认证请求UTC秒，旧记录缺省
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    /// 首次成功登录的IPv4或IPv6来源地址，旧记录缺省
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_ip: Option<String>,
    /// 当前刷新凭据到期UTC秒，备份恢复后缺省
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_expires_at: Option<String>,
    /// 撤销UTC秒
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
    /// 观测时会话状态
    pub state: ClientConnectionState,
}

/// 服务端登录入口所确认的接入类型，旧记录未知。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum ClientConnectionClientKind {
    /// 取值 `unknown`。
    #[serde(rename = "unknown")]
    Unknown,
    /// 取值 `client`。
    #[serde(rename = "client")]
    Client,
    /// 取值 `browser`。
    #[serde(rename = "browser")]
    Browser,
}

/// 当前账号一页客户端连接记录。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ClientConnectionPage {
    /// 按首次登录时间及连接身份降序，最多50条
    pub connections: Vec<ClientConnection>,
    /// 下一页游标，末页缺省
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// 服务端观测UTC秒
    #[serde(with = "crate::decimal")]
    pub observed_at: String,
}

/// 有效会话五分钟内（含300秒）有认证活动为recent（已连接），其余inactive（未连接）；unknown为会话已清理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum ClientConnectionState {
    /// 取值 `recent`。
    #[serde(rename = "recent")]
    Recent,
    /// 取值 `inactive`。
    #[serde(rename = "inactive")]
    Inactive,
    /// 取值 `revoked`。
    #[serde(rename = "revoked")]
    Revoked,
    /// 取值 `expired`。
    #[serde(rename = "expired")]
    Expired,
    /// 取值 `unknown`。
    #[serde(rename = "unknown")]
    Unknown,
}

/// 过期基线的冲突策略闭集（design §15.3）。首发只有 `preserve_both`：服务器主条目与冲突副本
/// 都保留，绝不丢内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum ConflictPolicy {
    /// 取值 `preserve_both`。
    #[serde(rename = "preserve_both")]
    PreserveBoth,
}

/// 提交成功但基线与服务器不一致的原因闭集（`preserve_both` 下产生冲突副本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum ConflictReason {
    /// 取值 `stale_base`。
    #[serde(rename = "stale_base")]
    StaleBase,
}

/// 条目种类闭集（design §7.2 `entries.kind`）。未知取值按协议错误处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum EntryKind {
    /// 取值 `file`。
    #[serde(rename = "file")]
    File,
    /// 取值 `directory`。
    #[serde(rename = "directory")]
    Directory,
}

/// 错误主体。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ErrorBody {
    /// 闭集错误码；未知码不是可重试错误，按协议错误处理。
    pub code: ErrorCode,
    /// 结构化补充信息，例如 `{"reason": "mount_identity_changed"}`。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<BTreeMap<String, String>>,
    /// 面向用户的可理解原因，按数据处理，不当作指令。
    pub message: String,
    /// 服务端记录的请求标识，供日志关联。
    pub request_id: String,
    /// 同一请求稍后原样重试是否可能成功。
    pub retryable: bool,
    /// 相关 Vault 的身份（32 个小写十六进制字符）；与 Vault 无关时省略。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault_id: Option<String>,
}

/// 远端错误码闭集（design §15.4）。未知错误码不能被当成可重试或成功；本机 IPC 专用的
/// 错误码（LOCAL_CHANGED、FILE_BUSY、MODE_REQUIRES_LOCAL_COPY、PLACEHOLDER_OFFLINE）
/// 属于 packages/local-ipc-types，不在远端契约内。
///
/// SERVICE_NOT_READY 是“服务端这一版还没有装配该端点所需的执行链路”的显式拒绝（本卡起用于
/// 操作命令端点），与 NEEDS_RECOVERY 的“意图无法解释、需要人工介入”不是一回事；它待与
/// docs/design.md §15.4 的表格同步。
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
    /// 取值 `VAULT_UNAVAILABLE`。
    #[serde(rename = "VAULT_UNAVAILABLE")]
    VaultUnavailable,
    /// 取值 `NEEDS_RECOVERY`。
    #[serde(rename = "NEEDS_RECOVERY")]
    NeedsRecovery,
    /// 取值 `INSUFFICIENT_STORAGE`。
    #[serde(rename = "INSUFFICIENT_STORAGE")]
    InsufficientStorage,
    /// 取值 `SERVICE_NOT_READY`。
    #[serde(rename = "SERVICE_NOT_READY")]
    ServiceNotReady,
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
    /// 内容字节数（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub size: String,
    /// 对象区内的规范相对路径（服务端内部路径）。客户端只原样回传，不拼接、不解释它。
    pub storage_path: String,
    /// 上传会话身份，32 位小写十六进制。
    pub upload_id: String,
    /// 内容所属 Vault 身份，32 位小写十六进制。
    pub vault_id: String,
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
#[derive(Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct LoginRequest {
    /// 设备的人类可读标签（不是设备身份，首发没有设备注册表）。
    pub device_label: String,
    /// 登录名（原样比较，不做大小写折叠）。
    pub login: String,
    /// 明文密码；只在本次请求内存在，服务端不记录。
    pub password: String,
}

impl core::fmt::Debug for LoginRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut value = f.debug_struct("LoginRequest");
        value.field("device_label", &self.device_label);
        value.field("login", &self.login);
        value.field("password", &"[REDACTED]");
        value.finish()
    }
}

/// 仅 owner 管理通道可见的 Vault。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ManagedVault {
    /// 本进程采集观测
    pub collection: VaultCollection,
    /// 当前已提交且未删除文件的逻辑总字节数；按文件计数，不含历史与回收站，不代表物理占用。旧服务端可缺省。
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_bytes: Option<String>,
    /// 现场挂载和存储状态
    pub health: VaultHealth,
    /// 可执行的修复建议
    pub next_step: String,
    /// 已注册服务器绝对路径
    pub path: String,
    /// 稳定故障原因；健康时为空
    pub reason: String,
    /// Vault 身份
    pub vault_id: String,
    /// 已提交事件序号，不表示采集完成
    #[serde(with = "crate::decimal")]
    pub vault_seq: String,
}

/// owner 管理清单；不可由普通 Vault 列表推导路径。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ManagedVaultList {
    /// 部署预配置允许基目录；空表示未配置且禁止注册。
    pub allowed_bases: Vec<String>,
    /// owner 已注册 Vault。
    pub vaults: Vec<ManagedVault>,
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

/// 已提交操作附带的冲突信息（design §15.3）：服务器主条目、新建的冲突副本与原因。内容都在
/// 服务器上，客户端据此把本地迟到修改对到副本身份上。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct OperationConflict {
    /// 冲突副本的条目身份，32 位小写十六进制。
    pub conflict_entry_id: String,
    /// 服务器主条目的身份，32 位小写十六进制。
    pub entry_id: String,
    /// 产生副本的原因。
    pub reason: ConflictReason,
}

/// 操作命令闭集（design §15.2）。未知取值按协议错误处理，不能当作任一命令执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum OperationKind {
    /// 取值 `put_content`。
    #[serde(rename = "put_content")]
    PutContent,
    /// 取值 `create`。
    #[serde(rename = "create")]
    Create,
    /// 取值 `move`。
    #[serde(rename = "move")]
    Move,
    /// 取值 `delete`。
    #[serde(rename = "delete")]
    Delete,
    /// 取值 `restore`。
    #[serde(rename = "restore")]
    Restore,
}

/// 一次修改命令的输入（design §15.3）。`op_id` 与 `device_id` 一起构成幂等键
/// `(vault, device_id, op_id)`；不同命令用不同字段（put 用 `entry_id`/`base_revision`/
/// `upload_id`，create 用 `parent_id`/`name`/`expected_absent`，move 用源
/// `entry_id`/`base_revision` 与目标 `parent_id`/`name`，delete 用
/// `entry_id`/`base_revision`（目录另带 `subtree_token`），restore 用
/// `entry_id`/`base_revision`（`version_id` 表示恢复指定历史版本））。带错字段一律 400，
/// 同一请求里不允许两套命令语义。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct OperationRequest {
    /// 客户端计划时的条目 revision（十进制文本）；过期基线不会被当作最新事实接受。
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    /// 客户端计划时看到的版本身份（可选，32 位小写十六进制）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_version_id: Option<String>,
    /// 过期基线的冲突策略；缺省按 `preserve_both` 处理。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_policy: Option<ConflictPolicy>,
    /// 发起操作的设备身份，32 位小写十六进制。首发没有设备注册表，它是客户端安装实例自报的
    /// 稳定身份，只用来划定幂等键的范围（design §7.2），不作为授权依据。
    pub device_id: String,
    /// 被操作或被覆盖的条目身份（put/delete 的目标、move 的源、restore 的条目），32 位小写十六进制。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// 新建时声明“目标原本不存在”；create 必须为 true（design §15.3）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_absent: Option<bool>,
    /// 命令种类。
    pub kind: OperationKind,
    /// 客户端本地内容代际（十进制文本）。服务器只原样回显供客户端核对，**不**把它当作
    /// “客户端确实没有后续修改”的证明（design §15.3）。
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_generation: Option<String>,
    /// 目标名称（create 必填、move 的目标名称）；不含路径分隔符。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 客户端生成的逻辑操作身份，32 位小写十六进制；重试必须复用同一个值。
    pub op_id: String,
    /// 目标父目录的条目身份（create 必填、move 的目标父目录），32 位小写十六进制。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// 客户端事实基础所属的服务器 epoch（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub server_epoch: String,
    /// 目录删除的服务端子树 token（32 位小写十六进制）。它由上一条快照/changes 结果给出；
    /// 子树在之后任何变化都让删除返回 409 `SUBTREE_CHANGED`。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtree_token: Option<String>,
    /// 已 finalize 的上传会话身份（put 必填，创建文件时 create 也必填；不带它的 create 创建目录），32 位小写十六进制；内容只通过它引用。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_id: Option<String>,
    /// 恢复指定的历史版本（可选，32 位小写十六进制）；省略表示恢复回收站里的当前形态。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

/// 一次操作的状态与业务结果（design §15.3）。`pending` 时只有 `op_id`/`state`（可能带
/// `echo_local_generation`）：不出现 `revision`/`vault_seq`/`version_id`/`entry_id` 一类
/// “已完成”字段。`replayed` 由端点按本次请求填写（是否命中了已记录的结果）；`request_id`
/// 在错误信封里给出，成功响应只带操作身份。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct OperationResult {
    /// committed 且基线与服务器不一致时给出的冲突副本信息。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<OperationConflict>,
    /// 原样回显请求里的本地代际（十进制文本），供客户端核对 ACK 属于哪一代。
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echo_local_generation: Option<String>,
    /// 业务结果涉及的条目身份（committed 时的目标条目），32 位小写十六进制。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// 稳定拒绝时的错误码（只有 rejected 有）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ErrorCode>,
    /// 稳定拒绝的可理解原因（只有 rejected 有，按数据处理）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// 逻辑操作身份，32 位小写十六进制。
    pub op_id: String,
    /// 本次响应是否直接来自已记录的结果（响应丢失后的重试为 true）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replayed: Option<bool>,
    /// 提交后的条目 revision（只有 committed 有，十进制文本）。
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// 操作状态；`pending` 不代表已同步。
    pub state: OperationState,
    /// 该批次占用的 VaultSeq（只有 committed 有，十进制文本）。
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault_seq: Option<String>,
    /// 提交产生的新版本身份（只有 put/restore 的 committed 有），32 位小写十六进制。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

/// 操作状态闭集（`operations.state`）。`pending` 只表示已受理，不占 VaultSeq、不判已同步；
/// `committed` 与 `rejected` 是终态，同一 OpId 的结果不再改变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum OperationState {
    /// 取值 `pending`。
    #[serde(rename = "pending")]
    Pending,
    /// 取值 `committed`。
    #[serde(rename = "committed")]
    Committed,
    /// 取值 `rejected`。
    #[serde(rename = "rejected")]
    Rejected,
}

/// 改密输入；禁止记录明文，沿用初始化密码规则。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct PasswordChangeRequest {
    /// 当前密码，只在本次请求内存在
    pub current_password: String,
    /// 新密码，沿用初始化密码规则
    pub new_password: String,
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

/// 历史或回收站中的可选恢复目标；恢复时仍须复核权限、版本保留和当前修订。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct RecoveryItem {
    /// 冲突副本对应的原条目身份；普通条目省略。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_source: Option<String>,
    /// 原条目身份。
    pub entry_id: String,
    /// UTC Unix 秒到期下限；当前活跃版本省略，不按文件 mtime。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// 固定版本摘要，目录省略。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// 分页稳定身份，历史为版本身份，回收站为条目身份。
    pub id: String,
    /// 当前名称或回收站原相对路径。
    pub name: String,
    /// 原父目录身份。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// 当前条目修订，恢复提交的 CAS 基础。
    pub revision: String,
    /// 固定版本字节数，目录省略。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// 固定内容版本，目录省略。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

/// 有界恢复项目清单；不是同步状态快照，重复或丢失时重新查询。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct RecoveryPage {
    /// 按身份升序的一页，最多100条。
    pub items: Vec<RecoveryItem>,
    /// 下一页 after 身份；没有下一页时省略。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// 刷新请求（design §15.2 `POST /v1/auth/refresh` 的输入）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct RefreshRequest {
    /// 上一次签发的刷新凭据；用一次即失效。
    pub refresh_token: String,
}

/// SSE 提示的 `data` 内容（design §9.3）：只告诉客户端 Vault 的 VaultSeq 已到某个值，
/// 不携带任何批次内容。丢失、重复、乱序都由 `changes` 拉取修复。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct SeqHint {
    /// 提示发出时 Vault 的最新 VaultSeq（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub seq: String,
    /// Vault 身份，32 位小写十六进制。
    pub vault_id: String,
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

/// 一份一致性快照的元数据（design §9.3）。`high_water_seq` 是创建时刻 Vault 的 VaultSeq：清单恰好是
/// “应用到该 seq 为止”的状态，继续追赶从 `changes?after=<high_water_seq>` 开始。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Snapshot {
    /// 清单里的条目总数（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub entry_count: String,
    /// 清单到期时间（UTC Unix 秒的十进制文本）；此刻及之后翻页返回 410。
    #[serde(with = "crate::decimal")]
    pub expires_at: String,
    /// 创建快照时的 VaultSeq（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub high_water_seq: String,
    /// 创建快照时 Vault 的服务器 epoch（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub server_epoch: String,
    /// 快照身份，32 位小写十六进制。
    pub snapshot_id: String,
    /// 所属 Vault 身份，32 位小写十六进制。
    pub vault_id: String,
}

/// 快照清单里的一条条目（创建时的值拷贝，不随之后的写入变化）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct SnapshotEntry {
    /// 条目身份，32 位小写十六进制。
    pub entry_id: String,
    /// 当前内容的 SHA-256，64 位小写十六进制（只有带内容的文件有）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// 文件或目录。
    pub kind: EntryKind,
    /// 服务端记录的最后修改时间（UTC Unix 秒的十进制文本）：创建、移动/改名、内容提交与回收站恢复时写入，
    /// 软删除同时写入。它是服务端接受改动的时刻，不是文件系统 mtime；升级前已存在的条目没有历史可回填，
    /// 首次修改前省略。
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,
    /// 原始名称（合成根条目为空串）；按数据处理。
    pub name: String,
    /// 父目录条目身份；Vault 的合成根条目没有。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// 创建快照时的条目 revision（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub revision: String,
    /// 当前内容字节数（十进制文本，只有带内容的文件有）。
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// 当前内容版本身份（只有带内容的文件有）；下载内容引用它而不是路径。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

/// 快照清单的一页。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct SnapshotPage {
    /// 本页条目，按 EntryId 升序。
    pub entries: Vec<SnapshotEntry>,
    /// 快照固定的 VaultSeq（十进制文本，所有页相同）。
    #[serde(with = "crate::decimal")]
    pub high_water_seq: String,
    /// 下一页的游标（十进制文本）；缺省表示已是最后一页。
    #[serde(with = "crate::decimal::option")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// 快照身份，32 位小写十六进制。
    pub snapshot_id: String,
}

/// 目录子树的服务端乐观并发证明（design §10.3、T12）。`token` 覆盖目标条目自身与全部活跃后代
/// 的身份、父目录、名称、种类与 revision；任一变化都会得到不同的 token。删除时原样回传。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct SubtreeToken {
    /// 冻结清单里的条目数（含目标自身，十进制文本）。
    #[serde(with = "crate::decimal")]
    pub entries: String,
    /// 目录条目身份，32 位小写十六进制。
    pub entry_id: String,
    /// 子树 token，32 位小写十六进制。
    pub token: String,
    /// Vault 身份，32 位小写十六进制。
    pub vault_id: String,
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

/// 创建上传会话的输入（design §15.2 `POST /v1/vaults/{vault}/uploads`）。
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
    /// 会话状态；客户端据此决定继续传块、查询结果还是放弃。
    pub state: UploadState,
    /// 快照总字节数（十进制文本）。
    #[serde(with = "crate::decimal")]
    pub total_size: String,
    /// 上传会话身份，32 位小写十六进制。
    pub upload_id: String,
    /// 所属 Vault 身份，32 位小写十六进制。
    pub vault_id: String,
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

/// 实际最近一轮扫描进度，不代表客户端已同步。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct VaultCollection {
    /// 未完成原因与本进程观测范围
    pub detail: String,
    /// 最近一轮实际检查项数
    #[serde(with = "crate::decimal")]
    pub examined: String,
    /// 最近一轮待稳定或重试文件数
    #[serde(with = "crate::decimal")]
    pub pending: String,
    /// 采集状态
    pub state: VaultCollectionState,
}

/// 本进程观测状态；重启后不沿用旧完成标志。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum VaultCollectionState {
    /// 取值 `not_scanned`。
    #[serde(rename = "not_scanned")]
    NotScanned,
    /// 取值 `scanning`。
    #[serde(rename = "scanning")]
    Scanning,
    /// 取值 `pending`。
    #[serde(rename = "pending")]
    Pending,
    /// 取值 `complete`。
    #[serde(rename = "complete")]
    Complete,
    /// 取值 `blocked`。
    #[serde(rename = "blocked")]
    Blocked,
}

/// Vault 健康状态闭集（design §7.2 `vaults.health`）。`unavailable` 表示挂载、身份或权限
/// 核对失败，该 Vault 暂停同步但绝不据此推导删除。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum VaultHealth {
    /// 取值 `healthy`。
    #[serde(rename = "healthy")]
    Healthy,
    /// 取值 `unavailable`。
    #[serde(rename = "unavailable")]
    Unavailable,
}

/// 授权 Vault 清单（design §15.2 `GET /v1/vaults`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct VaultList {
    /// 当前调用者可见的 Vault，按身份顺序稳定排列。
    pub vaults: Vec<VaultSummary>,
}

/// 注册 Vault 的输入（design §15.2 `POST /v1/admin/vaults`）。这是少数接受服务器绝对路径
/// 的网络接口之一，因此必须同时受允许基目录与存储准入约束。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct VaultRegisterRequest {
    /// 服务端绝对路径；必须在部署配置的允许基目录内。
    pub path: String,
}

/// 注册结果（design §15.2）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct VaultRegistered {
    /// 注册时的健康状态。
    pub health: VaultHealth,
    /// 该 Vault 的既有内容是否还没被采集。注册不导入既有文件，因此新注册的 Vault 恒为
    /// true；采集完成后由后续的初始采集步骤推进。
    pub pending_import: bool,
    /// 新 Vault 的身份，32 个小写十六进制字符。
    pub vault_id: String,
}

/// 一条授权 Vault 的对外摘要；不含服务器绝对路径。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct VaultSummary {
    /// 当前健康状态。
    pub health: VaultHealth,
    /// Vault 目录末级名称，不含父目录路径。旧服务端可省略；仅用于显示，身份仍使用 vault_id。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Vault 身份，32 个小写十六进制字符。
    pub vault_id: String,
}

/// 服务器持久化的固定版本保护；expires_at是UTC秒，ttl_seconds是剩余有效秒数。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct VersionLease {
    /// 显式解除后为true，不可重新续期。
    pub closed: bool,
    /// 固定Entry身份。
    pub entry_id: String,
    /// UTC到期秒数。
    #[serde(with = "crate::decimal")]
    pub expires_at: String,
    /// 请求租约身份。
    pub lease_id: String,
    /// 整份内容SHA256。
    pub sha256: String,
    /// 固定版本字节数。
    #[serde(with = "crate::decimal")]
    pub size: String,
    /// 响应形成时剩余秒数，最大120。
    #[serde(with = "crate::decimal")]
    pub ttl_seconds: String,
    /// 固定Vault身份。
    pub vault_id: String,
    /// 固定版本身份。
    pub version_id: String,
}

/// 固定版本内容租约的稳定请求；身份由调用端随机生成并在重试时复用。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct VersionLeaseRequest {
    /// 32位小写十六进制租约请求身份。
    pub lease_id: String,
    /// 已提交版本的整份SHA256，小写十六进制。
    pub sha256: String,
    /// 已提交版本的逻辑字节数。
    #[serde(with = "crate::decimal")]
    pub size: String,
}
