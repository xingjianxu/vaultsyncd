//! 传输执行所需的端口（design §5.4「续传状态与传输端口抽象」）。
//!
//! 传输执行是客户端侧的固定流程（design §13.2）：
//!
//! - 上传：固定本地快照 → 申请/复核会话 → 上传**服务端仍缺**的块 → finalize 校验 → 交给上层提交；
//! - 下载：固定 VersionId → 检查空间 → 下载缺失块到临时文件 → 逐块长度与整份摘要校验 → 交给上层安装。
//!
//! 这些流程属于纯规则，放在本 crate；真正的 HTTP、文件系统与 SQLite 都在端口后面，由适配器实现
//! （`crates/http-client` 的 `uploads`/`content`，`crates/agent-core` 的本地临时文件与进度表）。
//! 因此本 crate 仍然不读文件、不建连接、不看时钟，测试用内存替身即可完整覆盖续传语义。
//!
//! 端口失败必须**分类**而不是笼统报错：调度器要能区分“退避后原样重试”“需要重新登录”“服务端
//! 明确拒绝”“本地磁盘/权限问题”（design §13.3）。分类在适配器里做，因为只有它看得到 HTTP 状态、
//! 错误信封与操作系统的 `errno`。

use core::future::Future;
use core::pin::Pin;
use std::time::Duration;

use crate::bitmap::{PartBitmap, PartRange};
use crate::hash::DIGEST_BYTES;

/// 端口 future：借用适配器与参数，可装箱（与 `crates/http-client` 的传输端口同一风格）。
pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 本地 I/O 失败的类别（详情在 `detail` 里，面向诊断）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalErrorKind {
    /// 读取快照/临时文件失败（I/O 错误、内容在读取期间变化）。
    Read,
    /// 写入临时文件失败。
    Write,
    /// 空间不足（`ENOSPC`）：不删除既有内容，也不安装半份内容。
    NoSpace,
    /// 目标不存在（快照被删除、临时文件被清理）。
    Missing,
}

impl LocalErrorKind {
    /// 稳定的短标签，供日志与状态展示。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::NoSpace => "no_space",
            Self::Missing => "missing",
        }
    }
}

/// 端口失败：含义由适配器给出，调度由上层按类别决定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortError {
    /// 暂时性失败（连接、超时、5xx、限流）：退避后原样重试可能成功。
    Unavailable {
        /// 面向诊断的原因。
        detail: String,
        /// 服务端 `Retry-After`（秒数形式）。
        retry_after: Option<Duration>,
    },
    /// 需要用户重新登录；本地进度与未确认内容都必须保留（design §13.3）。
    ReauthRequired {
        /// 面向诊断的原因。
        detail: String,
    },
    /// 调用方取消。
    Cancelled,
    /// 服务端明确拒绝（403、410、冲突、会话关闭、块不符）：原样重试不会成功。
    Rejected {
        /// 契约里的错误码（`api/openapi.yaml` 的 `ErrorCode`）。
        code: String,
        /// 面向诊断的原因。
        detail: String,
    },
    /// 响应形状不合契约或请求无法构造：重试改变不了结论。
    Protocol {
        /// 面向诊断的原因。
        detail: String,
    },
    /// 本地 I/O 失败。
    Local {
        /// 失败类别。
        kind: LocalErrorKind,
        /// 面向诊断的原因。
        detail: String,
    },
}

impl PortError {
    /// 暂时性失败，退避后可原样重试。
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }

    /// 调用方取消。
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// 需要重新登录。
    #[must_use]
    pub const fn needs_reauth(&self) -> bool {
        matches!(self, Self::ReauthRequired { .. })
    }

    /// 本地空间不足。
    #[must_use]
    pub const fn is_no_space(&self) -> bool {
        matches!(
            self,
            Self::Local {
                kind: LocalErrorKind::NoSpace,
                ..
            }
        )
    }

    /// 服务端错误码（不是 `Rejected` 时为 `None`）。
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Rejected { code, .. } => Some(code),
            _ => None,
        }
    }

    /// 构造一个“暂时不可用”错误。
    #[must_use]
    pub fn unavailable(detail: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self::Unavailable {
            detail: detail.into(),
            retry_after,
        }
    }

    /// 构造一个“服务端拒绝”错误。
    #[must_use]
    pub fn rejected(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Rejected {
            code: code.into(),
            detail: detail.into(),
        }
    }

    /// 构造一个“协议/请求不合规”错误。
    #[must_use]
    pub fn protocol(detail: impl Into<String>) -> Self {
        Self::Protocol {
            detail: detail.into(),
        }
    }

    /// 构造一个本地 I/O 错误。
    #[must_use]
    pub fn local(kind: LocalErrorKind, detail: impl Into<String>) -> Self {
        Self::Local {
            kind,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { detail, .. } => write!(f, "暂时不可用：{detail}"),
            Self::ReauthRequired { detail } => write!(f, "需要重新登录：{detail}"),
            Self::Cancelled => f.write_str("已取消"),
            Self::Rejected { code, detail } => write!(f, "服务端拒绝（{code}）：{detail}"),
            Self::Protocol { detail } => write!(f, "协议或请求错误：{detail}"),
            Self::Local { kind, detail } => write!(f, "本地 {} 失败：{detail}", kind.as_str()),
        }
    }
}

impl std::error::Error for PortError {}

/// 一次上传会话的申请参数：总大小、整份摘要与希望的块大小。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionSpec {
    /// 快照总字节数。
    pub total_size: u64,
    /// 整份内容的 SHA-256。
    pub digest: [u8; DIGEST_BYTES],
    /// 客户端希望的块大小（服务端可以按自己的上限调整）。
    pub chunk_size: u64,
}

/// 服务端会话状态（与契约 `UploadState` 一一对应）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadSessionState {
    /// 接收块。
    Open,
    /// 块已齐，等待 finalize。
    Complete,
    /// 已 finalize（重复 finalize 是幂等重放）。
    Finalized,
    /// 已放弃，不能再传块。
    Aborted,
}

impl UploadSessionState {
    /// 稳定的短标签，供日志与状态展示。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Complete => "complete",
            Self::Finalized => "finalized",
            Self::Aborted => "aborted",
        }
    }
}

/// 服务端给出的会话事实：块几何与整份摘要都由服务端回报，客户端据此算块边界。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadSessionInfo {
    /// 会话身份（服务端 32 位小写十六进制）。
    pub id: String,
    /// 本会话实际使用的块大小。
    pub chunk_size: u64,
    /// 块数（空内容为 0）。
    pub part_count: u64,
    /// 快照总字节数。
    pub total_size: u64,
    /// 整份内容的 SHA-256。
    pub digest: [u8; DIGEST_BYTES],
    /// 会话状态。
    pub state: UploadSessionState,
}

/// 服务端已持久化的一个块（设计 §9.3：已收块以服务端为准）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceivedPart {
    /// 块索引（从 0 起）。
    pub index: u64,
    /// 块长度。
    pub length: u64,
    /// 块内容的 SHA-256。
    pub digest: [u8; DIGEST_BYTES],
}

/// 会话状态 + 服务端真实已收块 + 缺失区间（design §13.2 的续传依据）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadProgressInfo {
    /// 会话事实。
    pub session: UploadSessionInfo,
    /// 已收块，按索引升序。
    pub received: Vec<ReceivedPart>,
    /// 仍缺失的块区间，按索引升序；空表示块已齐。
    pub missing: Vec<PartRange>,
}

/// 一个块的登记结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartAck {
    /// 块索引。
    pub index: u64,
    /// 服务端登记的长度。
    pub length: u64,
    /// 服务端登记的块摘要。
    pub digest: [u8; DIGEST_BYTES],
    /// 是否为幂等重传（同索引同字节已经登记过）。
    pub already_present: bool,
}

/// finalize 的结果：可提交内容引用（**不是**已提交版本，design §15.2）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizedContent {
    /// 整份内容的 SHA-256。
    pub digest: [u8; DIGEST_BYTES],
    /// 字节数。
    pub size: u64,
    /// 块数。
    pub part_count: u64,
    /// 是否为幂等重放（会话本来就已 finalize）。
    pub replayed: bool,
    /// 对象区内的规范相对路径（服务端内部路径，客户端只原样带回）。
    pub storage_path: String,
}

/// 固定本地快照的只读读取端口。
///
/// 适配器必须保证**同一份快照**：`identity` 标识本地固定下来的内容（例如客户端本地代际/版本），
/// 读取按快照内容回答，而不是“边读边被外部修改的当前路径”（design §13.1）。
pub trait SnapshotSource: Send + Sync {
    /// 固定快照的本地身份（只用于进度绑定与诊断，不参与 wire 协议）。
    fn identity(&self) -> &str;

    /// 快照总字节数。
    fn total_size(&self) -> u64;

    /// 读取 `[offset, offset + length)`；必须返回恰好 `length` 字节，短读是错误。
    fn read(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>>;
}

/// 服务端上传会话端口。
pub trait UploadTarget: Send + Sync {
    /// 申请新会话（绑定总大小、整份摘要与块几何）。
    fn begin<'a>(
        &'a self,
        spec: &'a SessionSpec,
    ) -> PortFuture<'a, Result<UploadSessionInfo, PortError>>;

    /// 读取会话现状与**服务端真实已收块**（续传的唯一权威）。
    fn status<'a>(
        &'a self,
        session: &'a str,
    ) -> PortFuture<'a, Result<UploadProgressInfo, PortError>>;

    /// 上传一个块；`digest` 是 `bytes` 的 SHA-256，供服务端在写盘前核对。
    fn put_part<'a>(
        &'a self,
        session: &'a str,
        index: u64,
        digest: &'a [u8; DIGEST_BYTES],
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<PartAck, PortError>>;

    /// finalize：拼装并复核整份内容，返回可提交内容引用（**不提交条目 head**）。
    fn finalize<'a>(
        &'a self,
        session: &'a str,
    ) -> PortFuture<'a, Result<FinalizedContent, PortError>>;
}

/// 一个固定版本的内容读取端口（按字节区间取）。
pub trait VersionedContent: Send + Sync {
    /// 读取固定版本的 `[offset, offset + length)`；必须返回恰好 `length` 字节。
    fn read_range<'a>(
        &'a self,
        pin: &'a VersionPin,
        offset: u64,
        length: u64,
    ) -> PortFuture<'a, Result<Vec<u8>, PortError>>;
}

/// 下载的固定版本身份：条目 + 版本 + 该版本的整份摘要与大小。
///
/// 请求与校验都绑在这三项上：服务端若拿另一个版本的字节来回答，整份摘要对不上，就不会安装。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionPin {
    /// 客户端绑定身份（进度键的一部分）。
    pub binding: String,
    /// 条目身份。
    pub entry: String,
    /// 版本身份（固定 VersionId）。
    pub version: String,
    /// 该版本的整份内容 SHA-256（来自版本事实，不是本地猜测）。
    pub digest: [u8; DIGEST_BYTES],
    /// 该版本的内容字节数。
    pub size: u64,
}

impl VersionPin {
    /// 构造并做基本形态校验（身份非空、不含控制字符，块大小另行校验）。
    ///
    /// # Errors
    ///
    /// 身份为空或含控制字符时返回说明原因的字符串。
    pub fn new(
        binding: impl Into<String>,
        entry: impl Into<String>,
        version: impl Into<String>,
        digest: [u8; DIGEST_BYTES],
        size: u64,
    ) -> Result<Self, String> {
        let pin = Self {
            binding: validate_label("binding", binding.into())?,
            entry: validate_label("entry", entry.into())?,
            version: validate_label("version", version.into())?,
            digest,
            size,
        };
        Ok(pin)
    }
}

/// 临时内容的写入端口：可续传的下载先落到临时文件，校验通过后才由上层安装。
///
/// 适配器绑定**一个**临时路径（`path`），因此进度记录里的路径与本次运行的端口不一致时，旧位图
/// 必须作废（临时文件已经不可用）。
pub trait TempContent: Send + Sync {
    /// 临时文件路径（写进进度记录，供重启后核对）。
    fn path(&self) -> &str;

    /// 检查空间：不足时立即失败，不删除既有内容，也不写半份内容。
    fn ensure_capacity(&self, needed: u64) -> PortFuture<'_, Result<(), PortError>>;

    /// 在 `offset` 写入整块字节（长度由调用方按块几何决定）。
    fn write_at<'a>(
        &'a self,
        offset: u64,
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<(), PortError>>;

    /// 读取 `[offset, offset + length)`（整份摘要校验用，流式读取）。
    fn read_at(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>>;

    /// 落盘（`fsync`）：校验前必须让字节真正到达磁盘。
    fn sync(&self) -> PortFuture<'_, Result<(), PortError>>;

    /// 丢弃临时内容（只删除可重建的临时部分）。
    fn discard(&self) -> PortFuture<'_, Result<(), PortError>>;
}

/// 下载进度的持久化键：**包含 VersionId**，因此不同版本永远不会共用一份位图。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DownloadKey {
    /// 客户端绑定身份。
    pub binding: String,
    /// 条目身份。
    pub entry: String,
    /// 固定版本身份。
    pub version: String,
}

impl DownloadKey {
    /// 构造并校验身份形态。
    ///
    /// # Errors
    ///
    /// 任一身份为空或含控制字符时返回说明原因的字符串。
    pub fn new(
        binding: impl Into<String>,
        entry: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, String> {
        Ok(Self {
            binding: validate_label("binding", binding.into())?,
            entry: validate_label("entry", entry.into())?,
            version: validate_label("version", version.into())?,
        })
    }

    /// 稳定的文本键（适配器可以直接用它做主键的一部分）。
    #[must_use]
    pub fn as_key(&self) -> String {
        format!("{}:{}:{}", self.binding, self.entry, self.version)
    }
}

/// 一条持久化的下载进度。
///
/// 它只描述“哪些块已经在临时文件里”，不描述内容是什么：内容身份由 [`VersionPin`] 固定，整份摘要
/// 校验通过后才能安装。`chunk_size` 与 `total_size` 一起把位图钉在同一个块几何上——参数变了，
/// 旧位图必须作废，不能拿旧块边界套新文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredProgress {
    chunk_size: u64,
    total_size: u64,
    temp_path: String,
    bitmap: PartBitmap,
}

impl StoredProgress {
    /// 构造；位图几何必须与 `chunk_size`/`total_size` 一致，临时路径非空且不含控制字符。
    ///
    /// # Errors
    ///
    /// 几何不符或路径非法时返回说明原因的字符串。
    pub fn new(
        chunk_size: u64,
        total_size: u64,
        temp_path: impl Into<String>,
        bitmap: PartBitmap,
    ) -> Result<Self, String> {
        let geometry = bitmap.geometry();
        if geometry.chunk_size() != chunk_size || geometry.total_size() != total_size {
            return Err(format!(
                "位图几何（块 {} / 总 {}）与进度记录（块 {chunk_size} / 总 {total_size}）不一致",
                geometry.chunk_size(),
                geometry.total_size()
            ));
        }
        Ok(Self {
            chunk_size,
            total_size,
            temp_path: validate_label("temp_path", temp_path.into())?,
            bitmap,
        })
    }

    /// 块大小。
    #[must_use]
    pub const fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    /// 内容总大小。
    #[must_use]
    pub const fn total_size(&self) -> u64 {
        self.total_size
    }

    /// 临时文件路径。
    #[must_use]
    pub fn temp_path(&self) -> &str {
        &self.temp_path
    }

    /// 已收块位图。
    #[must_use]
    pub const fn bitmap(&self) -> &PartBitmap {
        &self.bitmap
    }
}

/// 下载进度的持久化端口（客户端 SQLite 的 `transfers` 表即其一种适配）。
///
/// 写入必须是“整条进度”的语义：适配器在同一个事务里更新位图与临时路径，不能出现“位图记了块、
/// 临时文件却没写”的顺序错觉。`discard` 只清理可重建的临时部分，不删唯一数据。
pub trait DownloadProgressStore: Send + Sync {
    /// 读取进度；没有记录时返回 `None`。
    fn load<'a>(
        &'a self,
        key: &'a DownloadKey,
    ) -> PortFuture<'a, Result<Option<StoredProgress>, PortError>>;

    /// 保存进度（覆盖旧值）。
    fn save<'a>(
        &'a self,
        key: &'a DownloadKey,
        progress: &'a StoredProgress,
    ) -> PortFuture<'a, Result<(), PortError>>;

    /// 丢弃进度（下次从第 0 块开始）。
    fn discard<'a>(&'a self, key: &'a DownloadKey) -> PortFuture<'a, Result<(), PortError>>;
}

/// 校验一个非空、无控制字符的标识（身份或路径）。
fn validate_label(field: &str, value: String) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("{field} 不能为空"));
    }
    if value.chars().any(|ch| ch.is_control()) {
        return Err(format!("{field} 不能包含控制字符"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_reject_empty_and_control_characters() {
        assert!(VersionPin::new("", "e", "v", [0; 32], 0).is_err());
        assert!(VersionPin::new("b", "e\n", "v", [0; 32], 0).is_err());
        let pin = VersionPin::new("b", "e", "v", [7; 32], 3).expect("合法身份");
        assert_eq!(pin.size, 3);
        assert_eq!(pin.digest, [7; 32]);
    }

    #[test]
    fn download_key_never_collides_across_versions() {
        let one = DownloadKey::new("b", "e", "v1").expect("键 1");
        let two = DownloadKey::new("b", "e", "v2").expect("键 2");
        assert_ne!(one.as_key(), two.as_key());
        assert_eq!(one.as_key(), "b:e:v1");
    }

    #[test]
    fn stored_progress_pins_the_block_geometry() {
        let geometry = crate::chunk::ChunkGeometry::new(10, 4).expect("块几何");
        let bitmap = PartBitmap::empty(geometry);
        assert!(StoredProgress::new(4, 10, "temp", bitmap.clone()).is_ok());
        assert!(StoredProgress::new(8, 10, "temp", bitmap.clone()).is_err());
        assert!(StoredProgress::new(4, 11, "", bitmap).is_err());
    }

    #[test]
    fn port_error_classification_is_explicit() {
        assert!(PortError::unavailable("连接被重置", None).is_retryable());
        assert!(!PortError::rejected("ACCESS_DENIED", "跨 Vault").is_retryable());
        assert!(PortError::Cancelled.is_cancelled());
        assert_eq!(
            PortError::rejected("PART_CONFLICT", "同索引不同字节").code(),
            Some("PART_CONFLICT")
        );
        assert!(PortError::local(LocalErrorKind::NoSpace, "ENOSPC").is_no_space());
    }
}
