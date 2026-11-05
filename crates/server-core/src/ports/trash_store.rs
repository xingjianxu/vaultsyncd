//! 删除的存储端口：子树清单与“回收站清单 + 墓碑 + 软删除 + 事件批次 + 意图收尾”的单一事务
//! （design §10.3、§11.2 步骤 7、§7.2 `trash`）。
//!
//! 与 [`crate::ports::namespace_store`] 同一套服务器事实词汇（[`RootFacts`]、[`EntryBaseline`]、
//! [`EntryKind`]），但事务内容不同：删除要一次写四样东西——回收站清单（可恢复的路径与内容引用）、
//! 墓碑（告诉旧设备“这个条目没了”）、条目的软删除与**一批**变更事件——并在同一事务里把意图收尾到
//! `finalized`。任何一样没写进去，就不能 ACK（design §10.3 第三条）。
//!
//! 端口不做判定（epoch 是否过期、token 是否仍成立、子树忙不忙），只如实返回库里的当前值并按调用方
//! 给出的清单落库；读到的行不合规就报 [`TrashStoreError::MalformedRow`]。
//!
//! 三条契约由适配器保证：
//!
//! 1. [`subtree_image`](DeleteStore::subtree_image) 返回的清单必须包含目标条目自身与**所有**活跃后代，
//!    顺序稳定（深度优先、同父按 `name_key`/`name`/`id`），否则子树 token 不能证明“没有漏掉并发新增”；
//! 2. [`apply_delete`](DeleteStore::apply_delete) 必须按`DeleteApply::batches` 的有界批次逐条软删除
//!    （每批不超过 `MAX_DELETE_BATCH_ENTRIES`），不得把整棵树塞进一条语句；所有批次与事件批次必须是
//!    **同一个**数据库事务，最终逻辑删除批次一次完整公布；
//! 3. 删除不动内容：不删 `objects` 行、不删 `versions` 行、不改 `content_version_id`。历史与内容在回收站
//!    保留期内仍可读（design §10.3 最后两条）。

use std::fmt;

use vaultsyncd_domain::ids::{EntryId, IntentId, RootId, TxId, VersionId};
use vaultsyncd_domain::version::{Revision, RootSeq, UtcTimestamp};

use crate::commit::delete::{DeleteBatch, SubtreeImage};
use crate::commit::intent::PutPhase;
use crate::ports::put_store::{EntryBaseline, EntryKind, RootFacts};

/// 回收站清单的一条：被删除条目的原位置、身份与内容引用。
///
/// 目录也有清单行（路径可恢复），因此 `content_version` 对目录恒为 `None`。`expires_at` 是内容可以
/// 被回收的最早时刻；删除条目在保留期内必须保持可恢复（design §10.3）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashManifestEntry {
    /// 被删除的条目身份。
    pub entry: EntryId,
    /// 原父目录；删除 Root 根条目不被允许，因此这里不出现 `None`。
    pub parent: EntryId,
    /// 原位置的 Root 内相对路径（整棵子树逐条记录）。
    pub path: String,
    /// 条目种类。
    pub kind: EntryKind,
    /// 删除时的条目 revision（与墓碑一起证明删的是哪一版）。
    pub revision: Revision,
    /// 删除时的当前内容版本；目录为 `None`。
    pub content_version: Option<VersionId>,
    /// 删除时间。
    pub deleted_at: UtcTimestamp,
    /// 内容最早可被回收的时间（删除时间 + 保留期）。
    pub expires_at: UtcTimestamp,
}

/// 一次删除事务的输入。
#[derive(Clone, Debug)]
pub struct DeleteApply {
    /// 已写进 `fs_intents` 且已推进到 `published` 的意图。
    pub intent_id: IntentId,
    /// 所属 Root。
    pub root: RootId,
    /// 子树根条目（被请求删除的那一个）。
    pub entry: EntryId,
    /// 有界批次：每批至多 `MAX_DELETE_BATCH_ENTRIES` 个条目，合起来就是整棵子树。
    pub batches: Vec<DeleteBatch>,
    /// 回收站清单（与批次条目一一对应，已含保留期）。
    pub trash: Vec<TrashManifestEntry>,
    /// 提交时间（事件批次的时间戳）。
    pub at: UtcTimestamp,
}

impl DeleteApply {
    /// 本次要删除的条目总数（所有批次）。
    #[must_use]
    pub fn entries(&self) -> usize {
        self.batches.iter().map(|batch| batch.entries.len()).sum()
    }
}

/// 一次已提交的删除事务结果（ACK 的证据）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteRecord {
    /// 被收尾的意图。
    pub intent_id: IntentId,
    /// 事务后意图的阶段：必须是 `finalized`，否则核心不返回 ACK。
    pub phase: PutPhase,
    /// 所属 Root。
    pub root: RootId,
    /// 子树根条目。
    pub entry: EntryId,
    /// 本次写进回收站与墓碑的条目数（必须等于清单长度）。
    pub trashed: u64,
    /// 该批次占用的 RootSeq。
    pub seq: RootSeq,
    /// 该批次的事务身份。
    pub tx: TxId,
}

/// 删除存储端口的失败。
#[derive(Clone, Debug)]
pub enum TrashStoreError {
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
    /// 条目不存在（含已经软删除）。
    EntryMissing {
        /// 被查询的条目。
        entry: EntryId,
    },
    /// 子树在事务中被别的提交改动（比较并写失败）：不覆盖别人的提交。
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
    /// 回收站里已经有该条目的清单行：同一身份不能被删两次。
    AlreadyInTrash {
        /// 命中的条目。
        entry: EntryId,
    },
    /// 声明要删的条目在库里已经不只一次出现，或清单与库不一致（漏项/多项）。
    ManifestMismatch {
        /// 原因文本。
        detail: String,
    },
    /// 写入被存储层约束拒绝（路径、JSON 形态等）：核心应在写库前就拦住。
    Rejected {
        /// 稳定短名。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
    /// 读到的行无法还原（revision 越界、父目录缺失等）。
    MalformedRow {
        /// 原因文本。
        detail: String,
    },
}

impl TrashStoreError {
    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            TrashStoreError::Storage { .. } => "trash_store_failed",
            TrashStoreError::OutOfScope => "out_of_scope",
            TrashStoreError::UnknownRoot { .. } => "unknown_root",
            TrashStoreError::EntryMissing { .. } => "entry_missing",
            TrashStoreError::RevisionChanged { .. } => "revision_changed",
            TrashStoreError::IllegalIntentTransition { .. } => "illegal_intent_transition",
            TrashStoreError::AlreadyInTrash { .. } => "already_in_trash",
            TrashStoreError::ManifestMismatch { .. } => "manifest_mismatch",
            TrashStoreError::Rejected { code, .. } => code,
            TrashStoreError::MalformedRow { .. } => "malformed_trash_row",
        }
    }
}

impl fmt::Display for TrashStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrashStoreError::Storage { detail } => write!(f, "删除存储失败：{detail}"),
            TrashStoreError::OutOfScope => f.write_str("Root 或条目不在调用方的 owner scope 内"),
            TrashStoreError::UnknownRoot { root } => write!(f, "Root {root} 不存在"),
            TrashStoreError::EntryMissing { entry } => write!(f, "条目 {entry} 不存在"),
            TrashStoreError::RevisionChanged { expected, actual } => {
                write!(f, "条目 revision 已从 {expected} 变为 {actual}")
            }
            TrashStoreError::IllegalIntentTransition { from, to } => {
                write!(f, "意图不能从 {} 收尾到 {}", from.as_text(), to.as_text())
            }
            TrashStoreError::AlreadyInTrash { entry } => {
                write!(f, "条目 {entry} 已经在回收站清单里")
            }
            TrashStoreError::ManifestMismatch { detail } => {
                write!(f, "删除清单与库不一致：{detail}")
            }
            TrashStoreError::Rejected { code, detail } => {
                write!(f, "删除写入被拒绝（{code}）：{detail}")
            }
            TrashStoreError::MalformedRow { detail } => write!(f, "删除行无法还原：{detail}"),
        }
    }
}

impl std::error::Error for TrashStoreError {}

/// 删除的存储端口。
///
/// 用 `async fn` 是为了直接对接服务端的异步 SQLx 事务；因此本 trait 不是 dyn 兼容的，核心的编排
/// 函数以泛型参数接受实现者。
// `#[allow(async_fn_in_trait)]`：本 trait 只在编排函数里以泛型参数使用，不外发、不做 dyn 对象。
#[allow(async_fn_in_trait)]
pub trait DeleteStore {
    /// 读 Root 当前事实（epoch）。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、Root 不存在。
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, TrashStoreError>;

    /// 读条目当前命名空间基线（活跃条目）。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、条目不存在、行无法还原。
    async fn entry_baseline(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<EntryBaseline, TrashStoreError>;

    /// 冻结一棵逻辑子树：目标条目自身 + 所有活跃后代，顺序稳定。
    ///
    /// 清单是 token 与批次清单的唯一来源：核心据此判断“并发新增是否被漏掉”、生成有界批次并把回收站
    /// 清单写进同一个事务。实现必须在与 [`apply_delete`](DeleteStore::apply_delete) 的写事务**不同**的
    /// 读快照上也能自洽：清单里的 revision 就是事务里比较并写的基线。
    ///
    /// # Errors
    ///
    /// 见 [`TrashStoreError`]：条目不存在、越权、存储失败、行无法还原。
    async fn subtree_image(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<SubtreeImage, TrashStoreError>;

    /// 在一个数据库事务里写回收站清单、墓碑、条目软删除与事件批次，并把意图收尾到 `finalized`。
    ///
    /// 实现必须满足的契约（design §10.3、§11.2 步骤 7）：
    ///
    /// 1. 按 `batches` 逐批软删除（每批 ≤ `MAX_DELETE_BATCH_ENTRIES`），不把整棵树塞进一条语句；
    /// 2. 每个条目写一条回收站清单行（原父目录、原路径、revision、内容版本、`expires_at`）与一条墓碑；
    /// 3. 不改 `objects`/`versions` 行、不改 `entries.content_version_id`：历史与内容仍可读；
    /// 4. 比较并写：库里的 revision 与清单不一致返回 [`TrashStoreError::RevisionChanged`]；声明要删的
    ///    条目已不在活跃集合里返回 [`TrashStoreError::ManifestMismatch`]；
    /// 5. 事件批次、回收站清单、墓碑、软删除与 `fs_intents` 的 `published → finalized` 同一事务提交；
    ///    意图不是 `published` 时返回 [`TrashStoreError::IllegalIntentTransition`]；
    /// 6. 返回的 [`DeleteRecord`] 是事务提交后的实测事实，`phase` 必须是 `finalized`。
    ///
    /// # Errors
    ///
    /// 见 [`TrashStoreError`]。
    async fn apply_delete(&mut self, apply: &DeleteApply) -> Result<DeleteRecord, TrashStoreError>;
}
