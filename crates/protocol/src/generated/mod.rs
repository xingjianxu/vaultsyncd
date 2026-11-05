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
    /// 取值 `UNSUPPORTED_ENTRY`。
    #[serde(rename = "UNSUPPORTED_ENTRY")]
    UnsupportedEntry,
    /// 取值 `HASH_MISMATCH`。
    #[serde(rename = "HASH_MISMATCH")]
    HashMismatch,
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

/// 协议主次版本；主版本不同必须拒绝通信。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ProtocolVersion {
    /// 主版本；与服务端不同即拒绝通信。（取值范围 0..=65535）
    pub major: u32,
    /// 次版本；服务端可以更大，客户端按自己认识的功能使用。（取值范围 0..=65535）
    pub minor: u32,
}
