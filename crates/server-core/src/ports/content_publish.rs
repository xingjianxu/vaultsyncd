//! 把上传的块拼装并发布成不可变内容对象（design §11.1—11.2、§13.2、I05、I12）。
//!
//! finalize 需要两件本层做不到的事情：把私有暂存的块按序拼装成一个完整文件并发布到对象区（文件
//! 系统动作）、把发布结果登记进 `objects`（数据库动作）。两个端口各自只报告事实——发布了几何
//! 大小的哪份内容、登记了哪一行——判定“这份内容能不能成为可提交引用”留在
//! [`crate::api::content`]。
//!
//! 契约：
//!
//! 1. [`ContentPublisher`] 必须把块按索引升序拼进**对象区私有暂存副本**，边写边算 SHA-256，读完
//!    每块后核对长度；发布用不覆盖语义（内容地址上已有同内容对象时复用）。它只报告实测事实，
//!    不得把“总量与期望不同”当成成功，也不得改写块字节来凑期望。
//! 2. [`ContentObjectStore`] 的登记必须幂等：同一 `(vault, hash)` 的相同登记返回成功，不同长度/路径
//!    报冲突（同一内容地址不能对应两份字节，I12）。
//! 3. 两者失败都必须保留可追溯的阶段文本（`stage`），不得用空结果或默认成功代替。

use std::path::PathBuf;

use vaultsyncd_domain::ids::{UploadId, VaultId};
use vaultsyncd_domain::version::UtcTimestamp;

use crate::api::content::PublishedContent;
use crate::api::uploads::StoredPart;
use crate::objects::ContentDigest;

/// 拼装/发布失败的分类。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishFailure {
    /// 某个已登记块的暂存字节缺失或读不出来。
    PartUnreadable {
        /// 块索引。
        index: u64,
        /// 原始错误文本（不含内容）。
        detail: String,
    },
    /// 暂存块的实际长度与块行记录不符：不能把两段不同长度的字节拼成一份内容。
    PartLength {
        /// 块索引。
        index: u64,
        /// 块行记录的期望长度。
        expected: u64,
        /// 实际读到的长度。
        actual: u64,
    },
    /// 块缺失（块行说收过，文件却不在）：不完整内容不得成为对象（I05）。
    PartMissing {
        /// 块索引。
        index: u64,
    },
    /// 存储动作失败（含满盘）。
    Storage {
        /// 失败阶段（稳定短名，例如 `assemble`、`publish`、`verify`）。
        stage: &'static str,
        /// 空间不足。
        no_space: bool,
        /// 原始错误文本。
        detail: String,
    },
    /// 发布后的对象复核不通过（长度或摘要与写入值不同）：字节在发布前后变了，不能当不可变对象。
    Verification {
        /// 原始错误文本。
        detail: String,
    },
    /// 登记被拒绝（同一内容地址已有不同长度/路径的登记）。
    ObjectConflict {
        /// 原因文本。
        detail: String,
    },
    /// 数据库/存储层失败。
    StorageLayer {
        /// 原始错误文本。
        detail: String,
    },
}

impl PublishFailure {
    /// 稳定短名（日志与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::PartUnreadable { .. } => "part_unreadable",
            Self::PartLength { .. } => "part_length_mismatch",
            Self::PartMissing { .. } => "part_missing",
            Self::Storage { stage, .. } => stage,
            Self::Verification { .. } => "object_verification_failed",
            Self::ObjectConflict { .. } => "object_conflict",
            Self::StorageLayer { .. } => "storage_layer_failed",
        }
    }

    /// 是否空间不足（API 层据此给 507）。
    #[must_use]
    pub fn is_no_space(&self) -> bool {
        matches!(self, Self::Storage { no_space: true, .. })
    }
}

impl std::fmt::Display for PublishFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PartUnreadable { index, detail } => {
                write!(f, "块 {index} 的暂存字节不可读：{detail}")
            }
            Self::PartLength {
                index,
                expected,
                actual,
            } => write!(f, "块 {index} 长度应为 {expected}，实际 {actual}"),
            Self::PartMissing { index } => write!(f, "块 {index} 的暂存字节缺失"),
            Self::Storage {
                stage,
                no_space,
                detail,
            } => {
                if *no_space {
                    write!(f, "拼装/发布阶段 {stage} 空间不足：{detail}")
                } else {
                    write!(f, "拼装/发布阶段 {stage} 失败：{detail}")
                }
            }
            Self::Verification { detail } => write!(f, "对象复核不通过：{detail}"),
            Self::ObjectConflict { detail } => write!(f, "内容登记冲突：{detail}"),
            Self::StorageLayer { detail } => write!(f, "内容登记存储失败：{detail}"),
        }
    }
}

impl std::error::Error for PublishFailure {}

/// 把块拼装并发布成不可变对象的端口。
///
/// 返回的 [`PublishedContent`] 是**实测**事实：拼装出的字节数、它的 SHA-256、对象区内的规范相对
/// 路径，以及对象是否本次新建。核心拿它与上传会话的总大小/总哈希比较，不符即丢弃（不提交、不引用）。
pub trait ContentPublisher {
    /// 重放 finalize 时沿用发布布局（包括已登记旧路径），不重新猜测存储位置。
    ///
    /// 默认实现只适用于全局旧布局的测试/旧适配器；按 Vault 隔离写入的适配器**必须**覆写，
    /// 否则重放响应会把已登记旧路径报成新的 Vault 分区路径，登记随即冲突。
    fn storage_path(&self, digest: &ContentDigest) -> PathBuf {
        crate::objects::ObjectLayout::relative_object_path(digest)
    }

    /// 按索引升序拼装 `parts` 并发布。
    ///
    /// # Errors
    ///
    /// 见 [`PublishFailure`]：块缺失/长度不符/不可读、存储失败（含满盘）、发布后复核不通过。
    fn publish_parts(
        &mut self,
        upload: UploadId,
        staging_name: &str,
        parts: &[StoredPart],
    ) -> Result<PublishedContent, PublishFailure>;
}

/// 登记已发布内容的端口（`objects` 表）。
///
/// 登记之后该内容才有持久身份：版本行可以引用它，GC 也才把它当成“已知内容”而不是未知残留。
#[allow(async_fn_in_trait)]
pub trait ContentObjectStore {
    /// 登记一份已发布内容；同一 `(vault, hash)` 的相同登记幂等。
    ///
    /// # Errors
    ///
    /// 见 [`PublishFailure`]：`ObjectConflict`（同地址不同长度/路径）、`StorageLayer`（存储层失败）。
    async fn record_published(
        &mut self,
        vault: VaultId,
        content: &PublishedContent,
        verified_at: UtcTimestamp,
    ) -> Result<(), PublishFailure>;
}

/// 内容地址与对象区相对路径（供适配器把 [`PublishedContent`] 翻译成存储行）。
#[must_use]
pub fn storage_path_of(content: &PublishedContent) -> PathBuf {
    PathBuf::from(&content.storage_path)
}
