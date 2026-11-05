//! 外部内容采纳的存储端口：路径 → 已知条目查询，以及一个事务内的“登记对象 + 写版本 + 追加事件”
//! （design §12.1“与客户端同一提交流程 → 事件传播”、§11.2 步骤 7）。
//!
//! 外部内容已经在工作目录里，服务端不需要（也不得）再把它发布回去覆盖：采纳只改元数据。因此这里
//! 没有意图与发布阶段，只有一个与 [`crate::ports::put_store::PutCommitStore::apply_put`] 同级别的
//! 事务：比较并写 revision、版本行、`changes` 批次与操作结果一起提交。

use std::fmt;

use vaultsyncd_domain::ids::{EntryId, RootId, TxId, VersionId};
use vaultsyncd_domain::version::{Revision, RootSeq, UtcTimestamp};

use crate::objects::{ContentDigest, ExpectedContent};
use crate::ports::put_store::EntryKind;

/// 库里按路径查到的已知条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownEntry {
    /// 条目身份。
    pub entry: EntryId,
    /// 当前 revision。
    pub revision: Revision,
    /// 条目种类。
    pub kind: EntryKind,
    /// 当前内容版本的摘要与长度；目录或尚无内容版本时为 `None`。
    pub content: Option<ExpectedContent>,
}

/// 采纳写入的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdoptKind {
    /// 库里没有这个路径：创建条目（缺失的父目录条目由存储层在同一事务里补建）与首个版本。
    Create,
    /// 库里已有这个普通文件：在期望 revision 上追加新版本。
    Update {
        /// 已知条目。
        entry: EntryId,
        /// 调用方观察到的 revision；事务内按它比较并写。
        expected_revision: Revision,
    },
}

/// 一次采纳事务的输入。
#[derive(Clone, Debug)]
pub struct ExternalWrite {
    /// 所属 Root。
    pub root: RootId,
    /// 文件在 Root 内的相对路径。
    pub path: String,
    /// 创建还是更新。
    pub kind: AdoptKind,
    /// 新内容的摘要与长度（已固定成不可变对象）。
    pub content: ExpectedContent,
    /// 写进 `objects.storage_path` 的相对路径；对象行不存在时由事务登记，已存在则必须一致。
    pub storage_path: String,
    /// 采纳时间。
    pub at: UtcTimestamp,
}

/// 一次已提交的采纳事务结果（证据）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalRecord {
    /// 条目身份（创建时为新分配的身份）。
    pub entry: EntryId,
    /// 新版本身份（外部导入首次生成 VersionId，design §12.1）。
    pub version: VersionId,
    /// 条目新 revision；更新时必须严格大于 `expected_revision`。
    pub revision: Revision,
    /// 该批次占用的 RootSeq。
    pub seq: RootSeq,
    /// 该批次的事务身份。
    pub tx: TxId,
    /// 版本内容摘要（必须等于请求内容）。
    pub digest: ContentDigest,
    /// 版本内容长度（必须等于请求内容）。
    pub size: u64,
}

/// 外部采纳存储端口的失败。
#[derive(Clone, Debug)]
pub enum ExternalStoreError {
    /// 存储层失败（SQLite 等），保留原始错误文本。
    Storage {
        /// 原始错误文本。
        detail: String,
    },
    /// Root 不在调用方范围内，或不存在。
    UnknownRoot {
        /// 被查询的 Root。
        root: RootId,
    },
    /// 事务内发现条目 revision 已不是调用方观察到的基线（比较并写失败）：他人已提交，重新规划。
    RevisionChanged {
        /// 调用方给的基线。
        expected: Revision,
        /// 库里的当前值。
        actual: Revision,
    },
    /// 路径与库里条目冲突（父路径是普通文件、目标是目录等）：不覆盖、不改写，等待人工协调。
    PathConflict {
        /// 原因文本。
        detail: String,
    },
    /// 写入被存储层约束拒绝（路径、对象登记不一致等）。
    Rejected {
        /// 稳定短名。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
}

impl ExternalStoreError {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ExternalStoreError::Storage { .. } => "external_store_failed",
            ExternalStoreError::UnknownRoot { .. } => "unknown_root",
            ExternalStoreError::RevisionChanged { .. } => "revision_changed",
            ExternalStoreError::PathConflict { .. } => "path_conflict",
            ExternalStoreError::Rejected { code, .. } => code,
        }
    }
}

impl fmt::Display for ExternalStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExternalStoreError::Storage { detail } => write!(f, "采纳存储失败：{detail}"),
            ExternalStoreError::UnknownRoot { root } => write!(f, "Root {root} 不存在"),
            ExternalStoreError::RevisionChanged { expected, actual } => {
                write!(f, "条目 revision 已从 {expected} 变为 {actual}")
            }
            ExternalStoreError::PathConflict { detail } => {
                write!(f, "路径与已知条目冲突：{detail}")
            }
            ExternalStoreError::Rejected { code, detail } => {
                write!(f, "采纳写入被拒绝（{code}）：{detail}")
            }
        }
    }
}

impl std::error::Error for ExternalStoreError {}

/// 外部内容采纳的存储端口。
///
/// 用 `async fn` 对接异步 SQLx 事务；因此本 trait 不是 dyn 兼容的，核心以泛型参数接受实现者。
// `#[allow(async_fn_in_trait)]`：只在编排函数里以泛型参数使用，不外发、不做 dyn 对象。
#[allow(async_fn_in_trait)]
pub trait ExternalAdoptStore {
    /// 按路径查已知条目；没有返回 `None`。
    ///
    /// # Errors
    ///
    /// 存储失败或 Root 不存在。
    async fn lookup_path(
        &mut self,
        root: RootId,
        path: &str,
    ) -> Result<Option<KnownEntry>, ExternalStoreError>;

    /// 在一个数据库事务里登记对象（若尚未登记）、写新版本、推进条目 revision/当前版本、追加
    /// `changes` 批次与操作结果。
    ///
    /// 实现必须满足的契约（design §11.2 步骤 7、I01、I09）：
    ///
    /// 1. [`AdoptKind::Update`] 用比较并写核对 `expected_revision`，已变化返回
    ///    [`ExternalStoreError::RevisionChanged`]，不覆盖别人的提交；
    /// 2. 版本行只引用已登记对象与本事务追加的批次；
    /// 3. 全部写入同一事务；返回的 [`ExternalRecord`] 是提交后的实测事实；
    /// 4. 该事务**不**触碰工作目录。
    ///
    /// # Errors
    ///
    /// 见 [`ExternalStoreError`]。
    async fn apply_external(
        &mut self,
        write: &ExternalWrite,
    ) -> Result<ExternalRecord, ExternalStoreError>;
}
