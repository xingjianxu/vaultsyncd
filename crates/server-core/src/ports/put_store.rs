//! put 提交的存储端口：服务器事实、命名空间基线、对象持久化与元数据事务（design §11.2 步骤 3—7）。
//!
//! 端口只回答“库里现在是什么”并执行调用方给出的事务内容，不做业务判定：请求的 epoch 是否过期、
//! 基线 revision 是否已过时、现场指纹是否相符，都由核心比较（[`crate::commit::put`]）。实现方是
//! 服务端存储层；读到的行不合规要报 [`PutStoreError::MalformedIntent`] 一类错误，不要猜一个 revision
//! 或一条路径。

use std::fmt;

use vaultsyncd_domain::ids::{EntryId, IntentId, RootId, TxId, VersionId};
use vaultsyncd_domain::version::{Revision, RootSeq, ServerEpoch, UtcTimestamp};

use crate::commit::intent::PutPhase;
use crate::objects::{ContentDigest, ExpectedContent};

/// 条目种类：只有普通文件能承载内容版本（design §7.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// 普通文件。
    File,
    /// 目录。
    Directory,
}

impl EntryKind {
    /// 是否普通文件。
    #[must_use]
    pub const fn is_file(self) -> bool {
        matches!(self, EntryKind::File)
    }

    /// 稳定短名（日志、意图证据与错误文本用）。
    #[must_use]
    pub const fn as_text(self) -> &'static str {
        match self {
            EntryKind::File => "file",
            EntryKind::Directory => "directory",
        }
    }

    /// 解析 [`EntryKind::as_text`] 的文本；未知文本返回 `None`，由调用方按坏行处理，不猜种类。
    #[must_use]
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "file" => Some(EntryKind::File),
            "directory" => Some(EntryKind::Directory),
            _ => None,
        }
    }
}

/// Root 的服务器事实（提交前要核对的 epoch）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootFacts {
    /// 当前服务器 epoch：恢复后更换，旧 epoch 的预期不得当成同一事实（design §7.1）。
    pub epoch: ServerEpoch,
}

/// 条目当前的命名空间基线：父目录、名称、revision 与种类。
///
/// 客户端提交带的是它计划时的基线；这里给出库里的当前值，由核心判断“过期基线不得覆盖”（I04）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryBaseline {
    /// 条目身份。
    pub entry: EntryId,
    /// 所属 Root。
    pub root: RootId,
    /// 当前父目录条目。
    pub parent: EntryId,
    /// 当前原始名称。
    pub name: String,
    /// 当前 revision（服务器分配，只有服务器推进）。
    pub revision: Revision,
    /// 条目种类。
    pub kind: EntryKind,
}

/// 内容对象是否已持久化。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectState {
    /// 对象已登记且长度是这条记录（发布前必须成立，design §11.2 步骤 3）。
    Persisted {
        /// 登记的字节数。
        bytes: u64,
    },
    /// 内容地址上还没有对象：上传尚未固定内容，不能开始发布。
    Missing,
}

/// 一次元数据事务的输入（design §11.2 步骤 7）。
#[derive(Clone, Debug)]
pub struct PutWrite {
    /// 已写进 `fs_intents` 且已发布到 `published` 的意图。
    pub intent_id: IntentId,
    /// 所属 Root。
    pub root: RootId,
    /// 被覆盖内容的条目。
    pub entry: EntryId,
    /// 调用方观察到的基线 revision；事务内按它做比较并写，已变化即拒绝。
    pub expected_revision: Revision,
    /// 新内容（发布后目标实测等于它）。
    pub content: ExpectedContent,
    /// 目标在 Root 内的相对路径。
    pub target_path: String,
    /// 提交时间。
    pub at: UtcTimestamp,
}

/// 一次已提交的元数据事务结果（ACK 的证据）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutRecord {
    /// 被收尾的意图。
    pub intent_id: IntentId,
    /// 事务后意图的阶段：必须是 `finalized`，否则核心不返回 ACK。
    pub phase: PutPhase,
    /// 新版本身份。
    pub version: VersionId,
    /// 条目新 revision（严格大于 `expected_revision`）。
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

/// put 存储端口的失败。
#[derive(Clone, Debug)]
pub enum PutStoreError {
    /// 存储层失败（SQLite 等），保留原始错误文本。
    Storage {
        /// 原始错误文本。
        detail: String,
    },
    /// Root 或条目不在调用方的 owner scope 内。
    OutOfScope,
    /// Root 不存在。
    UnknownRoot {
        /// 被查询的 Root。
        root: RootId,
    },
    /// 条目不存在。
    EntryMissing {
        /// 被查询的条目。
        entry: EntryId,
    },
    /// 事务内发现条目 revision 已不是调用方观察到的基线（比较并写失败）。
    RevisionChanged {
        /// 调用方给的基线。
        expected: Revision,
        /// 库里的当前值。
        actual: Revision,
    },
    /// 意图不存在，或阶段不是 `published`：没有发布证据就不能收尾。
    IllegalIntentTransition {
        /// 已记录的阶段。
        from: PutPhase,
        /// 请求推进到的阶段。
        to: PutPhase,
    },
    /// 写入被存储层约束拒绝（路径、JSON 形态等）：核心应在写库前就拦住。
    Rejected {
        /// 稳定短名，例如 `invalid_target_path`。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
    /// 读到的行无法还原（revision 越界、缺少当前版本等）。
    MalformedIntent {
        /// 原因文本。
        detail: String,
    },
}

impl PutStoreError {
    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            PutStoreError::Storage { .. } => "put_store_failed",
            PutStoreError::OutOfScope => "out_of_scope",
            PutStoreError::UnknownRoot { .. } => "unknown_root",
            PutStoreError::EntryMissing { .. } => "entry_missing",
            PutStoreError::RevisionChanged { .. } => "revision_changed",
            PutStoreError::IllegalIntentTransition { .. } => "illegal_intent_transition",
            PutStoreError::Rejected { code, .. } => code,
            PutStoreError::MalformedIntent { .. } => "malformed_put_row",
        }
    }
}

impl fmt::Display for PutStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PutStoreError::Storage { detail } => write!(f, "提交存储失败：{detail}"),
            PutStoreError::OutOfScope => f.write_str("Root 或条目不在调用方的 owner scope 内"),
            PutStoreError::UnknownRoot { root } => write!(f, "Root {root} 不存在"),
            PutStoreError::EntryMissing { entry } => write!(f, "条目 {entry} 不存在"),
            PutStoreError::RevisionChanged { expected, actual } => {
                write!(f, "条目 revision 已从 {expected} 变为 {actual}")
            }
            PutStoreError::IllegalIntentTransition { from, to } => {
                write!(f, "意图不能从 {} 收尾到 {}", from.as_text(), to.as_text())
            }
            PutStoreError::Rejected { code, detail } => {
                write!(f, "提交写入被拒绝（{code}）：{detail}")
            }
            PutStoreError::MalformedIntent { detail } => write!(f, "提交行无法还原：{detail}"),
        }
    }
}

impl std::error::Error for PutStoreError {}

/// put 提交的存储端口。
///
/// 用 `async fn` 是为了直接对接服务端的异步 SQLx 事务；因此本 trait 不是 dyn 兼容的，核心的编排
/// 函数以泛型参数接受实现者。
// `#[allow(async_fn_in_trait)]`：本 trait 只在编排函数里以泛型参数使用，不外发、不做 dyn 对象。
#[allow(async_fn_in_trait)]
pub trait PutCommitStore {
    /// 读 Root 当前事实（epoch）。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、Root 不存在。
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, PutStoreError>;

    /// 读条目当前命名空间基线。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、条目不存在、行无法还原。
    async fn entry_baseline(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<EntryBaseline, PutStoreError>;

    /// 查询内容地址上的对象是否已持久化及其登记长度。
    ///
    /// # Errors
    ///
    /// 存储失败、越权。
    async fn object_state(
        &mut self,
        root: RootId,
        digest: ContentDigest,
    ) -> Result<ObjectState, PutStoreError>;

    /// 在一个数据库事务里写新版本、条目 revision/当前内容、事件批次与操作结果，并把意图收尾到
    /// `finalized`。
    ///
    /// 实现必须满足的契约（design §11.2 步骤 7、I01、I09）：
    ///
    /// 1. 事务内用比较并写核对 `expected_revision`：已变化返回
    ///    [`PutStoreError::RevisionChanged`]，不覆盖别人的提交；
    /// 2. 版本行只能引用**已登记**内容对象与本事务追加的批次（S028 的复合外键）；
    /// 3. `changes` 批次与 `entries.revision`/当前版本、`fs_intents` 的 `published → finalized`
    ///    同一事务提交；意图不是 `published` 时返回 [`PutStoreError::IllegalIntentTransition`]；
    /// 4. 返回的 [`PutRecord`] 是事务提交后的实测事实，`phase` 必须是 `finalized`。
    ///
    /// # Errors
    ///
    /// 见 [`PutStoreError`]：存储失败、越权、条目缺失、revision 已变、意图阶段不合法。
    async fn apply_put(&mut self, write: &PutWrite) -> Result<PutRecord, PutStoreError>;
}
