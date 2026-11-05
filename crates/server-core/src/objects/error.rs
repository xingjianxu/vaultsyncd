//! 对象写入/读取的失败模型：每个失败都带稳定错误码，并说明未发布副本是否已清理。

use std::fmt;

use crate::objects::{ContentDigest, DigestParseError, InvalidStagingName};
use crate::ports::object_store::{BlobError, FileKey};

/// 未发布暂存副本的清理结果。
///
/// 失败路径必须报告它：残留副本本身不构成引用，但静默留下垃圾会让空间与恢复核对失真。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscardOutcome {
    /// 还没有建立暂存副本，无需清理。
    NotNeeded,
    /// 暂存副本已删除。
    Discarded,
    /// 暂存副本删除失败，需要人工核查（不改变“未发布”的结论）。
    Failed,
}

impl DiscardOutcome {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DiscardOutcome::NotNeeded => "not_needed",
            DiscardOutcome::Discarded => "discarded",
            DiscardOutcome::Failed => "failed",
        }
    }
}

impl fmt::Display for DiscardOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 采集（工作文件 → 不可变对象）失败的原因。
#[derive(Clone, Debug)]
pub enum CaptureFailureReason {
    /// 请求本身不成立（暂存名、期望内容），未做任何 I/O。
    InvalidRequest {
        /// 原因文本。
        detail: String,
    },
    /// 端口报告的错误（源不可读、存储失败、暂存校验失败等）。
    Port(BlobError),
    /// 暂存副本与工作文件是同一个 inode：源被改写会一起变化，不能当不可变对象。
    SharedWritableInode {
        /// 源文件身份。
        source: FileKey,
        /// 暂存副本身份。
        staged: FileKey,
    },
    /// 暂存副本还有别的硬链接名字：别名可以被外部写入，不算私有副本。
    StagingAlias {
        /// 暂存副本的链接数。
        links: u32,
    },
    /// 副本长度与期望内容不符（含短写、截断、源在复制期间增长）。
    LengthMismatch {
        /// 期望字节数。
        expected: u64,
        /// 实测字节数。
        observed: u64,
    },
    /// 副本摘要与期望内容不符。
    DigestMismatch {
        /// 期望摘要。
        expected: ContentDigest,
        /// 实测摘要。
        observed: ContentDigest,
    },
    /// 内容地址上已有对象，但复核发现它不是这份内容。
    ExistingObjectMismatch {
        /// 期望（也是内容地址对应的）摘要。
        expected: ContentDigest,
        /// 既有对象实测摘要。
        observed: ContentDigest,
        /// 既有对象实测字节数。
        observed_bytes: u64,
    },
    /// 发布后的复核不通过：对象路径上的内容与期望不符。
    PostPublishMismatch {
        /// 期望摘要。
        expected: ContentDigest,
        /// 复核实测摘要。
        observed: ContentDigest,
        /// 复核实测字节数。
        observed_bytes: u64,
    },
}

impl CaptureFailureReason {
    /// 稳定错误码（API 映射、日志与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            CaptureFailureReason::InvalidRequest { .. } => "invalid_request",
            CaptureFailureReason::Port(error) => error.code(),
            CaptureFailureReason::SharedWritableInode { .. } => "shared_writable_inode",
            CaptureFailureReason::StagingAlias { .. } => "staging_alias",
            CaptureFailureReason::LengthMismatch { .. } => "length_mismatch",
            CaptureFailureReason::DigestMismatch { .. } => "digest_mismatch",
            CaptureFailureReason::ExistingObjectMismatch { .. } => "existing_object_mismatch",
            CaptureFailureReason::PostPublishMismatch { .. } => "post_publish_mismatch",
        }
    }
}

impl fmt::Display for CaptureFailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureFailureReason::InvalidRequest { detail } => {
                write!(f, "请求不成立：{detail}")
            }
            CaptureFailureReason::Port(error) => write!(f, "{error}"),
            CaptureFailureReason::SharedWritableInode { source, staged } => write!(
                f,
                "暂存副本与工作文件是同一 inode（设备 {} inode {}），不是独立副本",
                source.device, staged.inode
            ),
            CaptureFailureReason::StagingAlias { links } => {
                write!(f, "暂存副本有 {links} 个硬链接，可能被外部写入")
            }
            CaptureFailureReason::LengthMismatch { expected, observed } => {
                write!(f, "副本长度 {observed} 与期望 {expected} 不符")
            }
            CaptureFailureReason::DigestMismatch { expected, observed } => {
                write!(f, "副本 SHA-256 {observed} 与期望 {expected} 不符")
            }
            CaptureFailureReason::ExistingObjectMismatch {
                expected,
                observed,
                observed_bytes,
            } => write!(
                f,
                "内容地址 {expected} 上已有对象，但实测 {observed}（{observed_bytes} 字节）"
            ),
            CaptureFailureReason::PostPublishMismatch {
                expected,
                observed,
                observed_bytes,
            } => write!(
                f,
                "发布后复核失败：期望 {expected}，实测 {observed}（{observed_bytes} 字节）"
            ),
        }
    }
}

/// 采集失败：原因 + 未发布副本的清理结果。成功路径不产生该值，因此不存在“部分成功”。
#[derive(Clone, Debug)]
pub struct CaptureFailure {
    /// 失败原因。
    pub reason: CaptureFailureReason,
    /// 未发布暂存副本的清理结果。
    pub staging: DiscardOutcome,
}

impl CaptureFailure {
    /// 用原因与清理结果构造。
    #[must_use]
    pub fn new(reason: CaptureFailureReason, staging: DiscardOutcome) -> Self {
        CaptureFailure { reason, staging }
    }

    /// 失败是否可能产生可提交引用：恒为 `false`（对象区的失败不会让核心返回成功）。
    #[must_use]
    pub fn is_committable(&self) -> bool {
        false
    }

    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.reason.code()
    }
}

impl fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}（暂存清理 {}）", self.reason, self.staging)
    }
}

impl std::error::Error for CaptureFailure {}

/// 读取失败的原因。
#[derive(Clone, Debug)]
pub enum ReadFailureReason {
    /// 内容地址格式不合法。
    Digest(DigestParseError),
    /// 端口错误（对象缺失、读取失败、读回校验失败）。
    Port(BlobError),
    /// 对象长度与元数据记录不符（截断或被改写）。
    LengthMismatch {
        /// 元数据记录的字节数。
        expected: u64,
        /// 实测字节数。
        observed: u64,
    },
    /// 对象摘要与请求的内容地址不符（I12：已提交字节不得改变）。
    DigestMismatch {
        /// 请求的内容地址。
        expected: ContentDigest,
        /// 实测摘要。
        observed: ContentDigest,
    },
}

impl ReadFailureReason {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ReadFailureReason::Digest(_) => "invalid_digest",
            ReadFailureReason::Port(error) => error.code(),
            ReadFailureReason::LengthMismatch { .. } => "length_mismatch",
            ReadFailureReason::DigestMismatch { .. } => "digest_mismatch",
        }
    }
}

impl fmt::Display for ReadFailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadFailureReason::Digest(error) => write!(f, "{error}"),
            ReadFailureReason::Port(error) => write!(f, "{error}"),
            ReadFailureReason::LengthMismatch { expected, observed } => {
                write!(f, "对象长度 {observed} 与记录的 {expected} 不符")
            }
            ReadFailureReason::DigestMismatch { expected, observed } => {
                write!(f, "对象 SHA-256 {observed} 与内容地址 {expected} 不符")
            }
        }
    }
}

/// 读取失败。
#[derive(Clone, Debug)]
pub struct ReadFailure {
    /// 失败原因。
    pub reason: ReadFailureReason,
}

impl ReadFailure {
    /// 用原因构造。
    #[must_use]
    pub fn new(reason: ReadFailureReason) -> Self {
        ReadFailure { reason }
    }

    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.reason.code()
    }
}

impl fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl std::error::Error for ReadFailure {}

/// 暂存名非法时转换成请求错误。
impl From<InvalidStagingName> for CaptureFailureReason {
    fn from(error: InvalidStagingName) -> Self {
        CaptureFailureReason::InvalidRequest {
            detail: error.to_string(),
        }
    }
}
