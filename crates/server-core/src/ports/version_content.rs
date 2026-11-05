//! 固定版本内容的元数据事实（design §15.2 `GET .../versions/{version}/content`、I12、I15）。
//!
//! 下载必须按“条目的某个具名版本”取字节，而不是按路径取当前内容：路径上的 head 随时可能被新版本
//! 取代，按路径解析会把“旧版本的下载”变成“新版本的内容”（design §15.4 `VERSION_GONE` 明确要求
//! 不这样做）。因此本端口只回答一件事：这个条目、这个版本、这份内容地址与长度是什么，以及它是否
//! 还允许读。
//!
//! 契约：
//!
//! - 所有查询都在调用方的 owner scope 内；Root 不存在与不在 scope 内一律
//!   [`VersionContentError::OutOfScope`]，不泄露 Root 是否存在（I15）。
//! - 条目或版本不存在、已经过期并被清理时返回 [`VersionContentError::VersionGone`]——调用方据此给
//!   410 `VERSION_GONE`，不得退化成“按当前 head 返回同路径的新内容”。
//! - 条目是目录时返回 [`VersionContentError::NotAFile`]：目录没有内容版本。
//! - 返回的长度来自持久化的 `versions.size`/`objects.size`，不由读到的字节反推。

use vaultsyncd_domain::ids::{EntryId, RootId, VersionId};

use crate::api::content::VersionContent;

/// 版本内容查询的失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionContentError {
    /// 数据库/存储层失败。
    Storage {
        /// 原始错误文本。
        detail: String,
    },
    /// Root 不在调用方的 owner scope 内（含 Root 不存在）。
    OutOfScope,
    /// 条目不存在，或不在该 Root 内。
    EntryNotFound,
    /// 条目是目录：没有内容版本。
    NotAFile,
    /// 版本不存在或已过期被清理（design §15.4 `VERSION_GONE`）。
    VersionGone,
    /// 版本行指向的内容对象没有登记（数据不一致）：不能凭版本行猜字节。
    ObjectUnavailable {
        /// 相关的内容地址文本。
        digest: String,
    },
}

impl VersionContentError {
    /// 稳定短名（日志与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Storage { .. } => "storage_failed",
            Self::OutOfScope => "out_of_scope",
            Self::EntryNotFound => "entry_not_found",
            Self::NotAFile => "not_a_file",
            Self::VersionGone => "version_gone",
            Self::ObjectUnavailable { .. } => "object_unavailable",
        }
    }
}

impl std::fmt::Display for VersionContentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage { detail } => write!(f, "版本内容存储失败：{detail}"),
            Self::OutOfScope => f.write_str("Root 不在调用方的授权范围"),
            Self::EntryNotFound => f.write_str("条目不存在或不在该 Root 内"),
            Self::NotAFile => f.write_str("条目是目录，没有内容版本"),
            Self::VersionGone => f.write_str("版本已过期或被清理"),
            Self::ObjectUnavailable { digest } => {
                write!(f, "版本内容 {digest} 没有登记的对象")
            }
        }
    }
}

impl std::error::Error for VersionContentError {}

/// 版本内容事实的来源。
#[allow(async_fn_in_trait)]
pub trait VersionContentStore {
    /// 解析 `(root, entry, version)` 到固定内容地址与长度。
    ///
    /// # Errors
    ///
    /// 见 [`VersionContentError`]：越权、条目不存在/不是文件、版本已过期、内容对象缺失、存储失败。
    async fn version_content(
        &mut self,
        root: RootId,
        entry: EntryId,
        version: VersionId,
    ) -> Result<VersionContent, VersionContentError>;
}
