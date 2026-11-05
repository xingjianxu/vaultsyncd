//! 冲突内容提交的存储端口：源条目现状、`OpId` 幂等记录与“冲突条目 + 映射 + 结果”事务
//! （design §7.2、§10.1—10.2、§11.2 步骤 7、I01、I06）。
//!
//! 端口只回答“库里现在是什么”并执行调用方给出的事务内容，不做业务判定：请求是否已过期、内容是否
//! 与 head 相同、同一 `OpId` 是不是另一个请求，都由核心比较（[`crate::commit::conflict`]）。实现方
//! 是服务端存储层（`crates/server-store` 的 `operations`/`entries`/`versions`/`changes`/`conflicts`/
//! `fs_intents`）；读到的行不合规要报 [`ConflictStoreError::MalformedRow`]，不要猜一个 revision、
//! 一条路径或一份内容。

use std::fmt;

use vaultsyncd_domain::ids::{DeviceId, EntryId, IntentId, OpId, RootId, TxId};
use vaultsyncd_domain::version::{Revision, ServerEpoch, UtcTimestamp};

use crate::commit::conflict::CommittedConflict;
use crate::commit::intent::PutPhase;
use crate::objects::{ContentDigest, ExpectedContent};
use crate::ports::put_store::EntryKind;

/// 请求摘要的字节长度（SHA-256）。
pub const REQUEST_DIGEST_LEN: usize = 32;

/// 规范化请求语义的摘要（`operations.request_digest`）。
///
/// 核心只比较摘要，不重算：规范化编码（字段名排序、长度前缀、语义版本前缀）只有一处实现，即存储层
/// 的 `vaultsyncd_server_store::operations::request_digest`。调用方必须用同一份规范化语义算摘要，
/// 否则“同一请求”会被判成两个（见 [`crate::commit::conflict::ConflictRequest::digest`]）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RequestDigest([u8; REQUEST_DIGEST_LEN]);

impl RequestDigest {
    /// 由原始摘要字节构造。
    pub const fn from_bytes(bytes: [u8; REQUEST_DIGEST_LEN]) -> Self {
        Self(bytes)
    }

    /// 原始字节（数据库 BLOB）。
    pub const fn as_bytes(&self) -> &[u8; REQUEST_DIGEST_LEN] {
        &self.0
    }

    /// 规范文本形式（小写十六进制），只用于日志与错误信息。
    pub fn to_hex(self) -> String {
        let mut text = String::with_capacity(REQUEST_DIGEST_LEN * 2);
        for byte in self.0 {
            text.push(char::from_digit(u32::from(byte >> 4), 16).expect("半字节是十六进制数字"));
            text.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("半字节是十六进制数字"));
        }
        text
    }
}

impl fmt::Debug for RequestDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RequestDigest({})", self.to_hex())
    }
}

/// 幂等键：同一设备在同一 Root 内的 `OpId`（design §7.2、I06）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationKey {
    /// 操作所在的 Root。
    pub root: RootId,
    /// 发起操作的设备。
    pub device: DeviceId,
    /// 客户端生成的逻辑操作身份。
    pub op: OpId,
}

/// 源条目（客户端想覆盖的那个 head 条目）当前的服务器事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictHead {
    /// Root 当前 epoch：恢复后更换，旧 epoch 的请求不得重放（design §9.4）。
    pub epoch: ServerEpoch,
    /// 源条目现状；条目已不存在时为 `None`（Root 仍存在）。
    pub source: Option<HeadSource>,
}

/// 源条目的现状：revision、种类与当前内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadSource {
    /// 条目身份。
    pub entry: EntryId,
    /// 当前 revision（服务器分配，只有服务器推进）。
    pub revision: Revision,
    /// 条目种类。
    pub kind: EntryKind,
    /// 当前内容摘要；普通文件必须有值，目录为 `None`（目录没有内容版本）。
    pub content: Option<ContentDigest>,
}

/// 该 `OpId` 已记录的幂等状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedOperation {
    /// 首次登记时记录的请求摘要。
    pub digest: RequestDigest,
    /// 已提交的业务结果；`None` 表示记录仍是 `pending`（未产生结果，不得重做）。
    pub result: Option<CommittedConflict>,
}

/// [`ConflictCommitStore::begin_conflict`] 的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictBegin {
    /// 首次见到这个幂等键：已写入 `pending` 行，调用方继续执行冲突提交。
    Started,
    /// 幂等键已存在：调用方必须按记录处理（重放、拒绝或等恢复），不得重做。
    Recorded(RecordedOperation),
}

/// 一次冲突提交的元数据事务输入（design §11.2 步骤 7）。
#[derive(Clone, Debug)]
pub struct ConflictWrite {
    /// 已写进 `fs_intents` 且已发布到 `published` 的意图。
    pub intent_id: IntentId,
    /// 幂等键。
    pub key: OperationKey,
    /// 本次请求的规范化摘要（与结果同事务登记）。
    pub digest: RequestDigest,
    /// 源条目（服务器 head）。
    pub source: EntryId,
    /// 冲突条目的身份（核心分配；事务必须原样使用，重试才拿到同一个副本）。
    pub sibling: EntryId,
    /// 冲突条目在 Root 内的相对路径（首次提交时生成，重试不改名）。
    pub path: String,
    /// 迟到分支的内容（对象必须已持久化）。
    pub content: ExpectedContent,
    /// 提交时间。
    pub at: UtcTimestamp,
}

/// 一次已提交的冲突事务结果（ACK 与重放共用的证据）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRecord {
    /// 被收尾的意图。
    pub intent_id: IntentId,
    /// 事务后意图的阶段：必须是 `finalized`，否则核心不返回 ACK。
    pub phase: PutPhase,
    /// 事务提交后的业务结果（冲突条目、映射与内容）。
    pub conflict: CommittedConflict,
    /// 该批次的事务身份。
    pub tx: TxId,
}

/// 冲突提交存储端口的失败。
#[derive(Clone, Debug)]
pub enum ConflictStoreError {
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
    /// 目标路径已被占用：不覆盖目标，重新规划另名（design §10.1 末段）。
    PathOccupied {
        /// 被占用的 Root 内相对路径。
        path: String,
    },
    /// 同一幂等键已被别的请求占用（绕过端口写入的坏数据）。
    DuplicateOperation {
        /// 被重复使用的操作身份。
        op: OpId,
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
        /// 稳定短名，例如 `invalid_conflict_path`。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
    /// 读到的行无法还原（文件条目缺少当前内容、revision 越界等）。
    MalformedRow {
        /// 原因文本。
        detail: String,
    },
}

impl ConflictStoreError {
    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ConflictStoreError::Storage { .. } => "conflict_store_failed",
            ConflictStoreError::OutOfScope => "out_of_scope",
            ConflictStoreError::UnknownRoot { .. } => "unknown_root",
            ConflictStoreError::PathOccupied { .. } => "target_path_occupied",
            ConflictStoreError::DuplicateOperation { .. } => "duplicate_operation",
            ConflictStoreError::IllegalIntentTransition { .. } => "illegal_intent_transition",
            ConflictStoreError::Rejected { code, .. } => code,
            ConflictStoreError::MalformedRow { .. } => "malformed_conflict_row",
        }
    }
}

impl fmt::Display for ConflictStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConflictStoreError::Storage { detail } => write!(f, "冲突存储失败：{detail}"),
            ConflictStoreError::OutOfScope => f.write_str("Root 或条目不在调用方的 owner scope 内"),
            ConflictStoreError::UnknownRoot { root } => write!(f, "Root {root} 不存在"),
            ConflictStoreError::PathOccupied { path } => {
                write!(f, "冲突路径 {path:?} 已被占用，不能覆盖")
            }
            ConflictStoreError::DuplicateOperation { op } => {
                write!(f, "操作 {op} 的幂等键已被其它请求占用")
            }
            ConflictStoreError::IllegalIntentTransition { from, to } => {
                write!(f, "意图不能从 {} 收尾到 {}", from.as_text(), to.as_text())
            }
            ConflictStoreError::Rejected { code, detail } => {
                write!(f, "冲突写入被拒绝（{code}）：{detail}")
            }
            ConflictStoreError::MalformedRow { detail } => write!(f, "冲突行无法还原：{detail}"),
        }
    }
}

impl std::error::Error for ConflictStoreError {}

/// 冲突内容提交的存储端口。
///
/// 用 `async fn` 是为了直接对接服务端的异步 SQLx 事务；因此本 trait 不是 dyn 兼容的，核心的编排
/// 函数以泛型参数接受实现者。
// `#[allow(async_fn_in_trait)]`：本 trait 只在编排函数里以泛型参数使用，不外发、不做 dyn 对象。
#[allow(async_fn_in_trait)]
pub trait ConflictCommitStore {
    /// 读 Root 当前 epoch 与源条目现状。
    ///
    /// 普通文件没有当前内容版本时报 [`ConflictStoreError::MalformedRow`]，不要给一个空摘要。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、Root 不存在、行无法还原。
    async fn conflict_head(
        &mut self,
        root: RootId,
        source: EntryId,
    ) -> Result<ConflictHead, ConflictStoreError>;

    /// 读该幂等键已记录的请求摘要与结果，不写库。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、Root 不存在。
    async fn operation(
        &mut self,
        key: OperationKey,
    ) -> Result<Option<RecordedOperation>, ConflictStoreError>;

    /// 登记该幂等键（首次写入 `pending`）并返回记录；原子“插入或读”。
    ///
    /// 契约（design I01、I06、§9.4）：
    ///
    /// 1. 幂等键不存在时写入 `pending` 行并返回 [`ConflictBegin::Started`]；
    /// 2. 已存在时**不**改写摘要、不覆盖结果，返回 [`ConflictBegin::Recorded`]；
    /// 3. `pending` 行不随设备代际失效删除，也不携带结果（pending 不是 committed）。
    ///
    /// # Errors
    ///
    /// 存储失败、越权、Root 不存在、幂等键被坏数据占用（[`ConflictStoreError::DuplicateOperation`]）。
    async fn begin_conflict(
        &mut self,
        key: OperationKey,
        digest: RequestDigest,
    ) -> Result<ConflictBegin, ConflictStoreError>;

    /// 在一个数据库事务里创建冲突条目与版本、写冲突映射、追加事件批次、登记操作结果，并把意图
    /// 收尾到 `finalized`。
    ///
    /// 实现必须满足的契约（design §7.4 `conflicts`、§10.2、§11.2 步骤 7、I01）：
    ///
    /// 1. 新条目用 [`ConflictWrite::sibling`] 作为身份、挂在[`ConflictWrite::source`] 所在父目录下、
    ///    名称为 [`ConflictWrite::path`] 的最后一段；路径已被占用返回
    ///    [`ConflictStoreError::PathOccupied`]；
    /// 2. 版本行只能引用**已登记**内容对象与本事务追加的批次（S028 的复合外键），摘要与长度等于
    ///    [`ConflictWrite::content`]；
    /// 3. 冲突映射（源条目 → 冲突条目、原因）与操作结果（同一 `OpId` 的终态结果）在同一事务提交，
    ///    重试时才可能拿到同一副本；
    /// 4. `changes` 批次、条目行与 `fs_intents` 的 `published → finalized` 同一事务提交；意图不是
    ///    `published` 时返回 [`ConflictStoreError::IllegalIntentTransition`]；
    /// 5. 返回的 [`ConflictRecord`] 是事务提交后的实测事实，`phase` 必须是 `finalized`。
    ///
    /// # Errors
    ///
    /// 见 [`ConflictStoreError`]：存储失败、越权、路径被占用、意图阶段不合法。
    async fn apply_conflict(
        &mut self,
        write: &ConflictWrite,
    ) -> Result<ConflictRecord, ConflictStoreError>;
}
