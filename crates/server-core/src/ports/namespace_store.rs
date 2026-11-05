//! 命名空间提交的存储端口：条目基线、父目录状态与命名空间元数据事务（design §9.5、§11.2 步骤 7）。
//!
//! 与 [`crate::ports::put_store`] 同一套服务器事实词汇（[`RootFacts`]、[`EntryBaseline`]、
//! [`EntryKind`]），但事务内容不同：这里只改父关系与名称，不写版本、不碰内容对象。端口不做判定
//! （epoch 是否过期、基线是否落后、父目录是否可挂），只如实返回库里的当前值与事务结果，读到的行
//! 不合规就报 [`NamespaceStoreError::MalformedRow`]。
//!
//! [`apply_namespace`](NamespaceCommitStore::apply_namespace) 的契约是本卡“移动只变父关系、不重新上传
//! 子内容”的落点：适配器不得为移动写版本行、不得改 `entries.content_version_id`、不得触发任何内容
//! 拷贝；目录创建只插入一条目录条目（`kind = directory`、无内容版本）。

use std::fmt;

use vaultsyncd_domain::ids::{EntryId, IntentId, RootId, TxId};
use vaultsyncd_domain::version::{Revision, RootSeq, UtcTimestamp};

use crate::commit::intent::PutPhase;
use crate::ports::put_store::{EntryBaseline, EntryKind, RootFacts};

/// 一个条目在 Root 内的位置：父目录 + 名称。
///
/// 名称与父目录分开记录：`entries` 的唯一性约束是 `(root_id, parent_id, name_key)`，移动要比较并写的
/// 正是这两个字段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceLocation {
    /// 父目录条目。
    pub parent: EntryId,
    /// 名称。
    pub name: String,
}

impl NamespaceLocation {
    /// 由一条完整基线取位置。
    #[must_use]
    pub fn of(baseline: &EntryBaseline) -> Self {
        NamespaceLocation {
            parent: baseline.parent,
            name: baseline.name.clone(),
        }
    }
}

impl fmt::Display for NamespaceLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{:?}", self.parent, self.name)
    }
}

/// 一次命名空间元数据事务的输入。
#[derive(Clone, Debug)]
pub struct NamespaceWrite {
    /// 已写进 `fs_intents` 且已推进到 `published` 的意图。
    pub intent_id: IntentId,
    /// 所属 Root。
    pub root: RootId,
    /// 条目身份（新建时就是新条目的身份，保持 EntryId）。
    pub entry: EntryId,
    /// 条目种类。
    pub kind: EntryKind,
    /// 事务内比较并写的基线 revision；新建时为 `None`（条目必须还不存在）。
    pub expected_revision: Option<Revision>,
    /// 移动的源位置；新建时为 `None`。
    pub from: Option<NamespaceLocation>,
    /// 目标位置。
    pub to: NamespaceLocation,
    /// 提交时间。
    pub at: UtcTimestamp,
}

/// 一次已提交的命名空间事务结果（ACK 的证据）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceRecord {
    /// 被收尾的意图。
    pub intent_id: IntentId,
    /// 事务后意图的阶段：必须是 `finalized`，否则核心不返回 ACK。
    pub phase: PutPhase,
    /// 条目身份：必须等于请求条目（移动不得换身份）。
    pub entry: EntryId,
    /// 条目新 revision：新建为 0，移动严格大于 `expected_revision`。
    pub revision: Revision,
    /// 该批次占用的 RootSeq。
    pub seq: RootSeq,
    /// 该批次的事务身份。
    pub tx: TxId,
}

/// 命名空间存储端口的失败。
#[derive(Clone, Debug)]
pub enum NamespaceStoreError {
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
    /// 新建时该身份已经有条目：不复用别人的身份。
    EntryExists {
        /// 冲突的条目。
        entry: EntryId,
    },
    /// 目标位置的同父活跃名称已被占用：不覆盖目标（design §10.1）。
    SlotOccupied {
        /// 目标父目录。
        parent: EntryId,
        /// 目标名称。
        name: String,
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
        /// 稳定短名。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
    /// 读到的行无法还原（revision 越界、缺父目录等）。
    MalformedRow {
        /// 原因文本。
        detail: String,
    },
}

impl NamespaceStoreError {
    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            NamespaceStoreError::Storage { .. } => "namespace_store_failed",
            NamespaceStoreError::OutOfScope => "out_of_scope",
            NamespaceStoreError::UnknownRoot { .. } => "unknown_root",
            NamespaceStoreError::EntryMissing { .. } => "entry_missing",
            NamespaceStoreError::RevisionChanged { .. } => "revision_changed",
            NamespaceStoreError::EntryExists { .. } => "entry_exists",
            NamespaceStoreError::SlotOccupied { .. } => "slot_occupied",
            NamespaceStoreError::IllegalIntentTransition { .. } => "illegal_intent_transition",
            NamespaceStoreError::Rejected { code, .. } => code,
            NamespaceStoreError::MalformedRow { .. } => "malformed_namespace_row",
        }
    }
}

impl fmt::Display for NamespaceStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceStoreError::Storage { detail } => write!(f, "命名空间存储失败：{detail}"),
            NamespaceStoreError::OutOfScope => {
                f.write_str("Root 或条目不在调用方的 owner scope 内")
            }
            NamespaceStoreError::UnknownRoot { root } => write!(f, "Root {root} 不存在"),
            NamespaceStoreError::EntryMissing { entry } => write!(f, "条目 {entry} 不存在"),
            NamespaceStoreError::RevisionChanged { expected, actual } => {
                write!(f, "条目 revision 已从 {expected} 变为 {actual}")
            }
            NamespaceStoreError::EntryExists { entry } => {
                write!(f, "条目身份 {entry} 已被占用")
            }
            NamespaceStoreError::SlotOccupied { parent, name } => {
                write!(f, "目标位置 {parent}/{name:?} 已有活跃条目")
            }
            NamespaceStoreError::IllegalIntentTransition { from, to } => {
                write!(f, "意图不能从 {} 收尾到 {}", from.as_text(), to.as_text())
            }
            NamespaceStoreError::Rejected { code, detail } => {
                write!(f, "命名空间写入被拒绝（{code}）：{detail}")
            }
            NamespaceStoreError::MalformedRow { detail } => {
                write!(f, "命名空间行无法还原：{detail}")
            }
        }
    }
}

impl std::error::Error for NamespaceStoreError {}

/// 命名空间提交的存储端口。
///
/// 用 `async fn` 是为了直接对接服务端的异步 SQLx 事务；因此本 trait 不是 dyn 兼容的，核心的编排
/// 函数以泛型参数接受实现者。
// `#[allow(async_fn_in_trait)]`：本 trait 只在编排函数里以泛型参数使用，不外发、不做 dyn 对象。
#[allow(async_fn_in_trait)]
pub trait NamespaceCommitStore {
    /// 读 Root 当前事实（epoch）。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、Root 不存在。
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, NamespaceStoreError>;

    /// 读条目当前命名空间基线。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、条目不存在、行无法还原。
    async fn entry_baseline(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<EntryBaseline, NamespaceStoreError>;

    /// 在一个数据库事务里写命名空间变化、事件批次，并把意图收尾到 `finalized`。
    ///
    /// 实现必须满足的契约（design §9.5、§11.2 步骤 7、I01、I03）：
    ///
    /// 1. 移动只改 `entries.parent_id`/`name`/`name_key`/`revision`：**不写版本行、不改
    ///    `content_version_id`、不做任何内容拷贝**，`EntryId` 保持不变；
    /// 2. 新建只插入一条 `kind = directory` 的条目（无内容版本，revision 0）；
    /// 3. 事务内用法比较并写核对 `expected_revision`：已变化返回
    ///    [`NamespaceStoreError::RevisionChanged`]，不覆盖别人的提交；新建时条目已存在返回
    ///    [`NamespaceStoreError::EntryExists`]；
    /// 4. 目标位置的同父活跃名称已被占用时返回 [`NamespaceStoreError::SlotOccupied`]（数据库唯一索引
    ///    是最终权威），绝不覆盖目标；
    /// 5. `changes` 批次、`entries` 更新与 `fs_intents` 的 `published → finalized` 同一事务提交；
    ///    意图不是 `published` 时返回 [`NamespaceStoreError::IllegalIntentTransition`]；
    /// 6. 返回的 [`NamespaceRecord`] 是事务提交后的实测事实，`phase` 必须是 `finalized`。
    ///
    /// # Errors
    ///
    /// 见 [`NamespaceStoreError`]。
    async fn apply_namespace(
        &mut self,
        write: &NamespaceWrite,
    ) -> Result<NamespaceRecord, NamespaceStoreError>;
}
