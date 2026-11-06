//! 恢复操作的存储端口：历史版本恢复与回收站恢复的服务器事实、`OpId` 幂等记录与单一事务
//! （design §10.3、§11.2 步骤 7、§15.2 `GET .../versions`、I06、I12、T13）。
//!
//! 两类恢复共用同一套幂等与意图词汇（[`OperationKey`]、[`RequestDigest`]、[`RestoreBegin`]、
//! [`RestoreStoreError`]），但事务内容不同：
//!
//! - **历史版本恢复**（[`RestoreStore`]）按一个旧版本创建**新的**当前版本：新版本行引用**同一个**
//!   内容对象，旧版本行一字不改（I12：已提交 VersionId 的字节不再改变）。事务里做版本追加 + 条目
//!   revision/当前内容比较并写 + 事件批次 + 操作结果 + 意图收尾。
//! - **回收站恢复**（[`TrashRestoreStore`]）把回收站清单里的一个子树放回命名空间：清软删除、按实际
//!   名称重新挂到原父目录、后代结构保持、清单行删除，同样是单一事务（design §10.3：恢复后原位置
//!   若冲突则另名，绝不覆盖现在的新文件）。
//!
//! 端口不做业务判定（epoch 是否过期、清单是否属于该条目、目标名称有没有被占用、租约是否该建），只
//! 如实返回库里的当前值并按调用方给出的内容落库；读到的行不合规就报
//! [`RestoreStoreError::MalformedRow`]，不猜一个 revision、一条路径或一份内容。

use std::fmt;

use vaultsyncd_domain::ids::{EntryId, IntentId, RootId, TxId, VersionId};
use vaultsyncd_domain::version::{Revision, UtcTimestamp};

use crate::commit::intent::PutPhase;
use crate::history::trash::CommittedTrashRestore;
use crate::history::version::CommittedVersionRestore;
use crate::objects::ExpectedContent;
use crate::ports::conflict_store::{OperationKey, RequestDigest};
use crate::ports::put_store::{EntryBaseline, EntryKind, RootFacts};

/// [`RestoreStore::begin_restore`]/[`TrashRestoreStore::begin_restore`] 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreBegin {
    /// 首次见到这个幂等键：已写入 `pending` 行，调用方继续执行恢复。
    Started,
    /// 幂等键已存在：调用方必须重读 [`RestoreStore::operation`]/
    /// [`TrashRestoreStore::operation`] 的记录再决定（重放、拒绝或等恢复），不得重做。
    Recorded,
}

/// 该 `OpId` 已记录的历史版本恢复状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedVersionRestore {
    /// 首次登记时记录的请求摘要。
    pub digest: RequestDigest,
    /// 已提交的业务结果；`None` 表示记录仍是 `pending`（未产生结果，不得重做）。
    pub result: Option<CommittedVersionRestore>,
}

/// 历史版本恢复要核对的一条版本事实：它属于哪个条目、内容是什么、保留到什么时候。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionFacts {
    /// 条目当前命名空间基线（父目录、名称、revision、种类）。
    pub entry: EntryBaseline,
    /// 被恢复版本的内容（发布后目标必须实测等于它）。
    pub content: ExpectedContent,
    /// 该版本可以被恢复到的最后时刻（历史保留期，design §10.3）；
    /// 过期后返回 `VERSION_GONE`，不允许用同路径的新内容冒充旧版本。
    pub retained_until: UtcTimestamp,
}

/// 一次历史版本恢复的元数据事务输入（design §11.2 步骤 7）。
#[derive(Clone, Debug)]
pub struct VersionRestoreWrite {
    /// 已写进 `fs_intents` 且已推进到 `published` 的意图。
    pub intent_id: IntentId,
    /// 幂等键。
    pub key: OperationKey,
    /// 本次请求的规范化摘要（与结果同事务登记）。
    pub digest: RequestDigest,
    /// 被恢复内容的条目。
    pub entry: EntryId,
    /// 调用方观察到的条目基线 revision；事务内比较并写。
    pub expected_revision: Revision,
    /// 恢复来源的旧版本：事务**不得**修改它的任何列（I12）。
    pub restored_from: VersionId,
    /// 旧版本的内容对象（新版本引用同一个对象，不复制字节）。
    pub content: ExpectedContent,
    /// 目标的 Root 内相对路径。
    pub path: String,
    /// 提交时间。
    pub at: UtcTimestamp,
}

/// 一次已提交的历史版本恢复事务结果（ACK 与重放共用的证据）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionRestoreRecord {
    /// 被收尾的意图。
    pub intent_id: IntentId,
    /// 事务后意图的阶段：必须是 `finalized`，否则核心不返回 ACK。
    pub phase: PutPhase,
    /// 该批次的事务身份。
    pub tx: TxId,
    /// 业务结果。
    pub result: CommittedVersionRestore,
}

/// 该 `OpId` 已记录的回收站恢复状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedTrashRestore {
    /// 首次登记时记录的请求摘要。
    pub digest: RequestDigest,
    /// 已提交的业务结果；`None` 表示记录仍是 `pending`。
    pub result: Option<CommittedTrashRestore>,
}

/// 回收站清单里的一条事实：原位置、原身份、删除时的 revision 与内容引用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashEntryFacts {
    /// 被删除条目的身份（恢复后不变：恢复不造新身份）。
    pub entry: EntryId,
    /// 删除时的父目录。
    pub parent: EntryId,
    /// 删除时的 Root 内相对路径。
    pub path: String,
    /// 条目种类。
    pub kind: EntryKind,
    /// 删除时的 revision。
    pub revision: Revision,
    /// 删除时的当前内容版本；目录为 `None`。
    pub content_version: Option<VersionId>,
    /// 删除时间。
    pub deleted_at: UtcTimestamp,
    /// 内容最早可被回收的时间；过期后清单不再可恢复（design §10.3）。
    pub expires_at: UtcTimestamp,
}

/// 一个回收站子树的清单：根条目在第一位，其余是它的活跃后代（顺序由端口保证稳定）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashSubtree {
    /// 清单行；至少一行（根）。
    pub entries: Vec<TrashEntryFacts>,
}

impl TrashSubtree {
    /// 子树根（清单第一行）。
    #[must_use]
    pub fn root(&self) -> &TrashEntryFacts {
        &self.entries[0]
    }
}

/// 回收站恢复要写回命名空间的一条。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashRestoreEntry {
    /// 条目身份（保持不变）。
    pub entry: EntryId,
    /// 恢复后的父目录（根用请求目标父目录，后代用清单里的父目录）。
    pub parent: EntryId,
    /// 恢复后的名称（只有根可能被另名，后代名称不变）。
    pub name: String,
    /// 恢复后的 Root 内相对路径。
    pub path: String,
    /// 条目种类。
    pub kind: EntryKind,
    /// 恢复后的当前内容版本；目录为 `None`。
    pub content_version: Option<VersionId>,
    /// 清单里的 revision：事务内比较并写，清单过期就整批拒绝。
    pub expected_revision: Revision,
}

/// 一次回收站恢复的元数据事务输入。
#[derive(Clone, Debug)]
pub struct TrashRestoreWrite {
    /// 为本次恢复中每个文件内容的发布而写的意图，顺序与 `entries` 里的文件一致。
    /// 事务必须把这些意图与结果一起收尾到 `finalized`；意图本身由核心在发布前才创建，身份在
    /// [`TrashRestoreStore::begin_restore`] 时已经持久化进计划。
    pub intents: Vec<IntentId>,
    /// 为本次恢复中每个目录的 `mkdir` 而写的意图，顺序与 `entries` 里的目录一致。存储适配器在
    /// `begin_restore` 的同一事务内创建它们（`prepared`），事务收尾时随结果一起推进到 `finalized`：
    /// 目录没有内容，但同样要有重启可见的证据，否则半途崩溃的 `mkdir` 会被后台观察当成外部新建。
    pub directory_intents: Vec<IntentId>,
    /// 幂等键。
    pub key: OperationKey,
    /// 本次请求的规范化摘要。
    pub digest: RequestDigest,
    /// 被恢复的回收站条目（子树根）。
    pub entry: EntryId,
    /// 清单里根的 revision（比较并写）。
    pub expected_revision: Revision,
    /// 恢复后的根路径（可能与原路径不同：原位置被占用时另名，T13）。
    pub path: String,
    /// 恢复后的根名称。
    pub name: String,
    /// 是否另名恢复（核心按占用事实决定；事务必须原样回报，重放才有同一份结果）。
    pub renamed: bool,
    /// 要写回命名空间的条目（根在第一位，其余按路径深度）。
    pub entries: Vec<TrashRestoreEntry>,
    /// 提交时间。
    pub at: UtcTimestamp,
}

/// 一次已提交的回收站恢复事务结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashRestoreRecord {
    /// 被收尾的意图（与 [`TrashRestoreWrite::intents`] 同一集合）。
    pub intents: Vec<IntentId>,
    /// 事务后这些意图的阶段：必须是 `finalized`。
    pub phase: PutPhase,
    /// 该批次的事务身份。
    pub tx: TxId,
    /// 业务结果。
    pub result: CommittedTrashRestore,
}

/// 恢复存储端口的失败。
#[derive(Clone, Debug)]
pub enum RestoreStoreError {
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
    /// 条目不存在（含已经软删除、已不在活跃命名空间里）。
    EntryMissing {
        /// 被查询的条目。
        entry: EntryId,
    },
    /// 版本不存在，或不属于该条目/不在保留期内。
    VersionMissing {
        /// 被查询的版本。
        version: VersionId,
    },
    /// 同一幂等键已被别的请求占用（绕过端口写入的坏数据）。
    DuplicateOperation,
    /// 意图不存在，或阶段不是 `published`：没有发布证据就不能收尾。
    IllegalIntentTransition {
        /// 已记录的阶段。
        from: PutPhase,
        /// 请求推进到的阶段。
        to: PutPhase,
    },
    /// 事务内发现条目 revision 已不是清单/请求的基线（比较并写失败）。
    RevisionChanged {
        /// 调用方给的基线。
        expected: Revision,
        /// 库里的当前值。
        actual: Revision,
    },
    /// 事务内发现恢复目标名称已被活跃条目占用（并发新建）：另名或重新规划，不覆盖。
    NameOccupied {
        /// 被占用的 Root 内相对路径。
        path: String,
    },
    /// 写入被存储层约束拒绝（路径、名称、JSON 形态等）：核心应在写库前就拦住。
    Rejected {
        /// 稳定短名，例如 `invalid_restore_path`。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
    /// 读到的行无法还原（revision 越界、种类未知等）。
    MalformedRow {
        /// 原因文本。
        detail: String,
    },
}

impl RestoreStoreError {
    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            RestoreStoreError::Storage { .. } => "restore_store_failed",
            RestoreStoreError::OutOfScope => "out_of_scope",
            RestoreStoreError::UnknownRoot { .. } => "unknown_root",
            RestoreStoreError::EntryMissing { .. } => "entry_missing",
            RestoreStoreError::VersionMissing { .. } => "version_missing",
            RestoreStoreError::DuplicateOperation => "duplicate_operation",
            RestoreStoreError::IllegalIntentTransition { .. } => "illegal_intent_transition",
            RestoreStoreError::RevisionChanged { .. } => "revision_changed",
            RestoreStoreError::NameOccupied { .. } => "name_occupied",
            RestoreStoreError::Rejected { code, .. } => code,
            RestoreStoreError::MalformedRow { .. } => "malformed_restore_row",
        }
    }
}

impl fmt::Display for RestoreStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreStoreError::Storage { detail } => write!(f, "恢复存储失败：{detail}"),
            RestoreStoreError::OutOfScope => f.write_str("Root 或条目不在调用方的 owner scope 内"),
            RestoreStoreError::UnknownRoot { root } => write!(f, "Root {root} 不存在"),
            RestoreStoreError::EntryMissing { entry } => write!(f, "条目 {entry} 不存在"),
            RestoreStoreError::VersionMissing { version } => write!(f, "版本 {version} 不可用"),
            RestoreStoreError::DuplicateOperation => f.write_str("同一 OpId 已被别的请求占用"),
            RestoreStoreError::IllegalIntentTransition { from, to } => {
                write!(f, "意图不能从 {} 收尾到 {}", from.as_text(), to.as_text())
            }
            RestoreStoreError::RevisionChanged { expected, actual } => {
                write!(f, "条目 revision 已从 {expected} 变为 {actual}")
            }
            RestoreStoreError::NameOccupied { path } => {
                write!(f, "恢复目标 {path:?} 已被占用，不能覆盖")
            }
            RestoreStoreError::Rejected { code, detail } => {
                write!(f, "恢复写入被拒绝（{code}）：{detail}")
            }
            RestoreStoreError::MalformedRow { detail } => write!(f, "恢复行无法还原：{detail}"),
        }
    }
}

impl std::error::Error for RestoreStoreError {}

/// 历史版本恢复的存储端口。
///
/// 实现必须满足的契约：
///
/// 1. [`version_facts`](RestoreStore::version_facts) 返回的版本必须属于该条目且在保留期内；
///    不在保留期就报 [`RestoreStoreError::VersionMissing`]，不返回一份“差不多”的内容；
/// 2. [`apply_version_restore`](RestoreStore::apply_version_restore) 在一个事务里：追加**新**版本行
///    （引用旧版本的同一内容对象）、按比较并写推进条目 revision/当前内容、追加事件批次、写操作结果、
///    把意图收尾到 `finalized`；**不修改**旧版本行（I12）；
/// 3. 恢复后的版本必须能被 `version_history` 看到，且旧版本仍在清单里、哈希不变。
#[allow(async_fn_in_trait)]
pub trait RestoreStore {
    /// 读 Root 当前事实（epoch）。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、Root 不存在。
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, RestoreStoreError>;

    /// 读该 `OpId` 已记录的恢复状态。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、行无法还原。
    async fn operation(
        &mut self,
        key: OperationKey,
    ) -> Result<Option<RecordedVersionRestore>, RestoreStoreError>;

    /// 为本次恢复登记 `pending` 或返回“已记录”。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、幂等键被别的请求占用、行无法还原。
    async fn begin_restore(
        &mut self,
        key: OperationKey,
        digest: RequestDigest,
    ) -> Result<RestoreBegin, RestoreStoreError>;

    /// 读条目的当前基线、被恢复版本的内容与保留期末尾。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、条目不存在、版本不可用、行无法还原。
    async fn version_facts(
        &mut self,
        root: RootId,
        entry: EntryId,
        version: VersionId,
    ) -> Result<VersionFacts, RestoreStoreError>;

    /// 在单一事务里按旧版本创建新的当前版本并收尾意图。
    ///
    /// # Errors
    ///
    /// 见 [`RestoreStoreError`]：存储失败、越权、条目缺失、版本缺失、revision 已变、意图阶段不合法。
    async fn apply_version_restore(
        &mut self,
        write: &VersionRestoreWrite,
    ) -> Result<VersionRestoreRecord, RestoreStoreError>;
}

/// 回收站恢复的存储端口。
///
/// 实现必须满足的契约：
///
/// 1. [`trash_subtree`](TrashRestoreStore::trash_subtree) 返回该条目的**整棵**清单（根在第一位、
///    后续为全部后代，顺序稳定），不在回收站/已过期时返回 `None`；
/// 2. [`name_occupant`](TrashRestoreStore::name_occupant) 只把**活跃**条目算作占用：软删除的条目不算，
///    因此“恢复回原位”不会被自己的旧行挡住；被占用时核心另名（T13）；
/// 3. [`apply_trash_restore`](TrashRestoreStore::apply_trash_restore) 在一个事务里：清软删除、改回
///    父目录与名称、推进 revision、删掉这些条目的回收站清单行、追加事件批次、写操作结果，并把这批
///    意图收尾到 `finalized`；事务内再核对一次名称占用（并发新建 → [`RestoreStoreError::NameOccupied`]）。
#[allow(async_fn_in_trait)]
pub trait TrashRestoreStore {
    /// 读 Root 当前事实（epoch）。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、Root 不存在。
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, RestoreStoreError>;

    /// 读该 `OpId` 已记录的回收站恢复状态。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、行无法还原。
    async fn operation(
        &mut self,
        key: OperationKey,
    ) -> Result<Option<RecordedTrashRestore>, RestoreStoreError>;

    /// 在**同一事务**里持久化整组恢复计划并登记 `pending`：操作记录、冻结清单（含条目身份与
    /// 内容版本）、实际路径与另名决定、文件意图身份，以及全部目录的 `prepared` 意图。已记录则返回
    /// “已记录”，不改写任何已有计划。计划落盘之前核心不得 `mkdir` 或发布任何文件。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、幂等键被别的请求占用、行无法还原。
    async fn begin_restore(
        &mut self,
        key: OperationKey,
        digest: RequestDigest,
        plan: &TrashRestoreWrite,
    ) -> Result<RestoreBegin, RestoreStoreError>;

    /// 读一个回收站子树的清单；不在回收站（或已过期/已恢复）时返回 `None`。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、行无法还原。
    async fn trash_subtree(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<Option<TrashSubtree>, RestoreStoreError>;

    /// 读恢复后父目录的种类；父目录不存在时返回 `None`。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、行无法还原。
    async fn parent_kind(
        &mut self,
        root: RootId,
        parent: EntryId,
    ) -> Result<Option<EntryKind>, RestoreStoreError>;

    /// 读活跃目录**当前**的 Root 内相对路径（Root 顶层目录为空串）；不存在返回 `None`。
    /// 原父目录在删除之后被移动或改名时，恢复位置跟随它，而不是清单里的旧路径前缀。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、行无法还原。
    async fn parent_path(
        &mut self,
        root: RootId,
        parent: EntryId,
    ) -> Result<Option<String>, RestoreStoreError>;

    /// 读同父目录下该名称的活跃占用者。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、行无法还原。
    async fn name_occupant(
        &mut self,
        root: RootId,
        parent: EntryId,
        name: &str,
    ) -> Result<Option<EntryId>, RestoreStoreError>;

    /// 读某个版本的内容（恢复时要把它重新发布到工作目录）。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、行无法还原。
    async fn content_facts(
        &mut self,
        root: RootId,
        version: VersionId,
    ) -> Result<Option<ExpectedContent>, RestoreStoreError>;

    /// 在单一事务里把清单写回命名空间、删清单行并收尾意图。
    ///
    /// # Errors
    ///
    /// 见 [`RestoreStoreError`]：存储失败、越权、revision 已变、名称被占用、意图阶段不合法。
    async fn apply_trash_restore(
        &mut self,
        write: &TrashRestoreWrite,
    ) -> Result<TrashRestoreRecord, RestoreStoreError>;
}
