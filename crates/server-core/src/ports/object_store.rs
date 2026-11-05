//! 内容对象字节层端口：对象区与工作文件的实际读写（design §11.1—11.2、§11.5）。
//!
//! 端口只有“读、建私有副本、发布、丢弃、复核”这几种动作，且每个动作都必须报告它实际观察到的
//! 事实（来源方式、长度、SHA-256、文件身份）。核心据此判定可否成为对象；适配器不得把
//! `EIO`/`ENOSPC`/权限错误报告成“不支持 reflink”或“复制成功”，也不得用共享 inode 的硬链接
//! 冒充独立副本（design §11.5、C37）。

use std::fmt;
use std::path::Path;

use crate::objects::ContentDigest;

/// 端口报告的文件身份与大小。
///
/// `device`/`inode` 一起回答“是不是同一个 inode”，`links` 回答“有没有别的名字指向同一
/// inode（硬链接别名）”。inode 只在所属子卷/设备内解释，因此两个字段必须成对比较。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileKey {
    /// 设备号（`st_dev`）。
    pub device: u64,
    /// inode 号（`st_ino`）。
    pub inode: u64,
    /// 硬链接数（`st_nlink`）。
    pub links: u32,
    /// 可见字节数（`st_size`）。
    pub bytes: u64,
}

impl FileKey {
    /// 两个观察是否指向同一个 inode。
    #[must_use]
    pub fn same_inode(&self, other: &Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}

/// 副本的实际来源方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyMethod {
    /// 文件系统的克隆（`FICLONE`）：共享 extent 的 CoW 副本。
    Reflink,
    /// 完整复制：逐字节写入目标侧暂存文件并读回校验。
    VerifiedCopy,
}

impl CopyMethod {
    /// 稳定短名（日志与验收脚本按它断言）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CopyMethod::Reflink => "reflink",
            CopyMethod::VerifiedCopy => "verified_copy",
        }
    }
}

impl fmt::Display for CopyMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 没走克隆的原因：两者都允许退回完整复制，但必须暴露原因（design §11.5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFallback {
    /// 源与对象区不在同一文件系统（`EXDEV`）：不能依赖一次 `rename`，必须复制到对象区。
    CrossDevice,
    /// 该文件系统不支持 reflink（`EOPNOTSUPP`/`ENOTTY`/`EINVAL`）。
    ReflinkUnsupported,
}

impl CopyFallback {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CopyFallback::CrossDevice => "exdev",
            CopyFallback::ReflinkUnsupported => "unsupported",
        }
    }
}

impl fmt::Display for CopyFallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 对象区内的私有暂存副本及其观察结果。
///
/// `staged` 必须是**新 inode**：与工作文件共用可写 inode 的副本在源被改写时会一起变化，
/// 不能当不可变对象（design §11.1、C37）。`digest` 与 `staged.bytes` 是对暂存文件读回后
/// 计算的结果，不是源文件的猜测值。
#[derive(Clone, Debug)]
pub struct StagedBlob {
    /// 暂存文件路径。
    pub staging: std::path::PathBuf,
    /// 建立副本前读到的源文件身份。
    pub source: FileKey,
    /// 暂存副本自身的身份（含读回校验后的字节数）。
    pub staged: FileKey,
    /// 暂存副本内容的 SHA-256。
    pub digest: ContentDigest,
    /// 副本来源方式。
    pub method: CopyMethod,
    /// 没走克隆的原因。
    pub fallback: Option<CopyFallback>,
}

/// 发布结果。
#[derive(Clone, Debug)]
pub struct PublishedBlob {
    /// 发布后的对象路径。
    pub object: std::path::PathBuf,
    /// 目标内容地址上原本就存在同内容对象时为真（此时既有对象未被改写或覆盖）。
    pub already_present: bool,
}

/// 一次完整读取的结果（含字节）。
#[derive(Clone, Debug)]
pub struct StoredBytes {
    /// 对象全部字节。
    pub data: Vec<u8>,
    /// 读出的字节数（必须等于 `data.len()`）。
    pub bytes: u64,
    /// 读出内容的 SHA-256。
    pub digest: ContentDigest,
}

/// 只核对不保留字节时的读取结果（大对象复核用）。
#[derive(Clone, Copy, Debug)]
pub struct StoredFacts {
    /// 读出的字节数。
    pub bytes: u64,
    /// 读出内容的 SHA-256。
    pub digest: ContentDigest,
}

/// 存储失败分类：`ENOSPC` 等真实错误不能被报告成“能力不支持”或成功（design §11.5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageFailure {
    /// 空间不足（`ENOSPC`）。
    NoSpace,
    /// 超出配额（`EDQUOT`）。
    QuotaExceeded,
    /// 设备/文件系统读写错误（`EIO` 等）。
    Io,
    /// 权限不足（`EACCES`/`EPERM`）。
    PermissionDenied,
    /// 只读文件系统（`EROFS`）。
    ReadOnly,
    /// 其它错误（保留原始文本）。
    Other,
}

impl StorageFailure {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StorageFailure::NoSpace => "no_space",
            StorageFailure::QuotaExceeded => "quota_exceeded",
            StorageFailure::Io => "io",
            StorageFailure::PermissionDenied => "permission_denied",
            StorageFailure::ReadOnly => "read_only",
            StorageFailure::Other => "other",
        }
    }
}

impl fmt::Display for StorageFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 端口错误。每个变体都保留可追溯到具体阶段的文本，便于恢复与诊断。
#[derive(Clone, Debug)]
pub enum BlobError {
    /// 源文件不可读或已消失：不得解释为删除。
    SourceUnreadable {
        /// 原始错误文本。
        detail: String,
    },
    /// 存储动作失败（含分类）。
    Storage {
        /// 发生失败的阶段（稳定短名，例如 `stage_copy`、`publish_rename`）。
        stage: &'static str,
        /// 失败分类。
        failure: StorageFailure,
        /// 原始错误文本。
        detail: String,
    },
    /// 暂存副本读回校验失败（长度或摘要不符）。
    Verification {
        /// 原始错误文本。
        detail: String,
    },
    /// 目标对象不存在。
    ObjectMissing {
        /// 被查询的路径或原因文本。
        detail: String,
    },
    /// 请求本身不成立（路径、暂存名等），适配器未做任何 I/O。
    InvalidRequest {
        /// 原因文本。
        detail: String,
    },
}

impl BlobError {
    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            BlobError::SourceUnreadable { .. } => "source_unreadable",
            BlobError::Storage { failure, .. } => failure.as_str(),
            BlobError::Verification { .. } => "stage_verification_failed",
            BlobError::ObjectMissing { .. } => "object_missing",
            BlobError::InvalidRequest { .. } => "invalid_request",
        }
    }
}

impl fmt::Display for BlobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlobError::SourceUnreadable { detail } => write!(f, "源文件不可读：{detail}"),
            BlobError::Storage {
                stage,
                failure,
                detail,
            } => write!(f, "存储失败（{stage}/{failure}）：{detail}"),
            BlobError::Verification { detail } => write!(f, "暂存校验失败：{detail}"),
            BlobError::ObjectMissing { detail } => write!(f, "对象不存在：{detail}"),
            BlobError::InvalidRequest { detail } => write!(f, "请求不成立：{detail}"),
        }
    }
}

impl std::error::Error for BlobError {}

/// 对象区字节层端口。
///
/// 实现方（Linux 适配器）必须满足的契约：
///
/// 1. [`stage_working_copy`](ObjectBlobPort::stage_working_copy) 在对象区建立**独占新 inode**
///    的副本；禁止硬链接源文件。优先 `FICLONE`；`EXDEV` 或文件系统明确不支持时退回完整复制并
///    如实设置 `fallback`；`EIO`/`ENOSPC`/`EDQUOT`/权限错误一律失败，不退回、不伪装。
/// 2. 副本建立后必须读回核对长度与 SHA-256、`fsync` 文件，并在 [StagedBlob] 里报告实测值。
///    任何失败都必须删除未验证的暂存文件（核心在端口报错时不再尝试丢弃）。
/// 3. [`publish_staged`](ObjectBlobPort::publish_staged) 不覆盖既有对象：目标已存在时报告
///    `already_present = true` 且不改动目标；成功后必须刷文件与目录。
/// 4. [`verify_object`](ObjectBlobPort::verify_object)/[`read_object`](ObjectBlobPort::read_object)
///    读的是已发布对象，返回实测长度与 SHA-256；读不到、读不完都是错误，不能用空内容代替。
pub trait ObjectBlobPort {
    /// 把工作文件复制成对象区内的私有暂存副本。
    ///
    /// # Errors
    ///
    /// 见 [`BlobError`]：源不可读、存储失败（含分类）、读回校验失败。
    fn stage_working_copy(&mut self, source: &Path, staging: &Path) -> Result<StagedBlob, BlobError>;

    /// 把已校验的暂存副本发布到内容地址路径（不覆盖既有对象，成功后刷文件与目录）。
    ///
    /// # Errors
    ///
    /// 见 [`BlobError`]；目标存在时**不是**错误，返回 `already_present = true`。
    fn publish_staged(&mut self, staged: &StagedBlob, object: &Path) -> Result<PublishedBlob, BlobError>;

    /// 丢弃未发布的暂存副本。
    ///
    /// # Errors
    ///
    /// 删除失败时返回错误（残留必须如实报告，不能静默忽略）。
    fn discard_staged(&mut self, staging: &Path) -> Result<(), BlobError>;

    /// 读取已发布对象并核对长度与 SHA-256（不保留字节）。
    ///
    /// # Errors
    ///
    /// 见 [`BlobError`]：对象不存在、读取失败或读回校验失败。
    fn verify_object(&mut self, object: &Path) -> Result<StoredFacts, BlobError>;

    /// 读出已发布对象的全部字节（固定版本内容下载用）。
    ///
    /// # Errors
    ///
    /// 见 [`BlobError`]：对象不存在、读取失败。
    fn read_object(&mut self, object: &Path) -> Result<StoredBytes, BlobError>;
}
