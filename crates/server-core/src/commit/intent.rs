//! put 的持久提交意图状态机（design §11.2 步骤 4—7、§11.3）。
//!
//! 一次单文件内容替换只有三个可持久化的成功阶段，顺序固定：
//!
//! 1. `prepared`：意图已写进 `fs_intents`，里面记着**现场前提**（`expected_before`）、
//!    **目标内容**（`desired_after`）、目标路径与暂存名。此时工作文件还没被改动。
//! 2. `published`：工作文件已按 `desired_after` 发布并复核过实测身份与摘要；元数据事务（entries/
//!    versions/changes/operations）尚未完成。
//! 3. `finalized`：元数据事务已提交（终态）。
//!
//! 另有一个非成功的出口 `needs_recovery`：现场无法解释时由恢复流程置入，暂停该 Root 的破坏性写入，
//! 不自动推进（[`crate::recovery`]）。
//!
//! 本模块固定的规则：
//!
//! - **意图先于磁盘改动**（步骤 5）：[`publish_put`] 先写 `prepared`，写不成功就不发布；
//! - **发布后必须复核**：发布出来的实测摘要与长度不等于 `desired_after` 时不推进阶段，意图留在
//!   `prepared` 作为现场证据；
//! - **错误永远不是 committed**：任何失败都返回 [`PutFailure`]，它的
//!   [`is_committed`](PutFailure::is_committed) 恒为 `false`，且不会删除已持久化的意图；
//! - **证据可重建**：`expected_before`/`desired_after` 是自描述的 JSON object（编码见
//!   [`TargetBefore::to_json`] / [`TargetAfter::to_json`]），恢复只读数据库行与工作目录现场就能
//!   判断该重做、该收尾、该协调还是该停下（design §11.3）。
//!
//! 本卡只做 put 的单文件 happy path 与可恢复状态：基线 revision/epoch/父目录与名称的完整 CAS 复核
//! 属 S035，冲突条目属 S036，崩溃注入与全 Root 恢复入口属 S037。create/move/delete 复用同一意图
//! 机制（S038/S039）。

use std::collections::BTreeMap;
use std::fmt;

use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{EntryId, IntentId, OpId, RootId};
use vaultsyncd_domain::version::{Revision, ServerEpoch, UtcTimestamp};

use crate::commit::put::CommitGateError;
use crate::objects::{ContentDigest, DigestParseError, ObjectLayout};
use crate::ports::conflict_store::{ConflictStoreError, RequestDigest};
use crate::ports::intent_log::{IntentLog, IntentLogError};
use crate::ports::object_store::FileKey;
use crate::ports::put_store::PutStoreError;
use crate::ports::target::{
    PublishRequest, PublishedTarget, TargetError, TargetObservation, TargetPort,
};

/// put 意图的持久化阶段（`fs_intents.phase`），与 design §11.3 恢复矩阵的四行证据一一对应。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutPhase {
    /// 意图已持久化，工作文件未发布：前提满足就可以重做（§11.3 第 3 行）。
    Prepared,
    /// 工作文件已按 `desired_after` 发布，元数据事务未完成（§11.3 第 4 行）。
    Published,
    /// 元数据事务已完成：终态。
    Finalized,
    /// 现场与意图都无法解释：暂停破坏性写入等人工核对（§11.3 第 6 行）。
    NeedsRecovery,
}

impl PutPhase {
    /// 数据库里的规范文本（与 `fs_intents.phase` 的 CHECK 同一套取值）。
    #[must_use]
    pub const fn as_text(self) -> &'static str {
        match self {
            PutPhase::Prepared => "prepared",
            PutPhase::Published => "published",
            PutPhase::Finalized => "finalized",
            PutPhase::NeedsRecovery => "needs_recovery",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜阶段。
    #[must_use]
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "prepared" => Some(PutPhase::Prepared),
            "published" => Some(PutPhase::Published),
            "finalized" => Some(PutPhase::Finalized),
            "needs_recovery" => Some(PutPhase::NeedsRecovery),
            _ => None,
        }
    }

    /// 是否已收尾。
    #[must_use]
    pub const fn is_finished(self) -> bool {
        matches!(self, PutPhase::Finalized)
    }

    /// 是否还需要恢复流程处理。
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(
            self,
            PutPhase::Prepared | PutPhase::Published | PutPhase::NeedsRecovery
        )
    }

    /// 允许的阶段转换。
    ///
    /// `prepared → published`（发布并复核通过）、`published → finalized`（元数据事务提交）、
    /// 两个开放阶段 → `needs_recovery`（现场无法解释）、`needs_recovery → finalized`（人工核对后
    /// 按已发布内容收尾）。`prepared` 不能直接 `finalized`：没有发布证据就没有可提交的内容。
    #[must_use]
    pub const fn allows_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (PutPhase::Prepared, PutPhase::Published)
                | (PutPhase::Prepared, PutPhase::NeedsRecovery)
                | (PutPhase::Published, PutPhase::Finalized)
                | (PutPhase::Published, PutPhase::NeedsRecovery)
                | (PutPhase::NeedsRecovery, PutPhase::Finalized)
        )
    }
}

impl fmt::Display for PutPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_text())
    }
}

/// 意图开始时目标的现场快照（`expected_before` 的 `present: true` 形态）。
///
/// 字段都是“当时观察到的事实”，恢复靠它们判断目标有没有被动过：`identity` 说明是不是同一个
/// inode，`digest`/`size` 说明内容还是不是那份内容。revision 与 entry 身份用来让协调者看到是哪
/// 个已确认版本被覆盖（完整 CAS 复核属 S035）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetSnapshot {
    /// 条目身份。
    pub entry: EntryId,
    /// 条目 revision（十进制无符号值）。
    pub revision: u64,
    /// 内容 SHA-256。
    pub digest: ContentDigest,
    /// 文件身份（含可见字节数）。
    pub identity: FileKey,
}

impl TargetSnapshot {
    /// 现场是否仍然是这份快照：同 inode、同摘要、同长度。
    #[must_use]
    pub fn matches(&self, observation: &TargetObservation) -> bool {
        match observation {
            TargetObservation::Absent => false,
            TargetObservation::Present { identity, digest } => {
                identity.same_inode(&self.identity)
                    && identity.bytes == self.identity.bytes
                    && digest == &self.digest
            }
        }
    }
}

/// 意图前提：目标在意图开始时是什么。
///
/// “当时不存在”必须是一个字段（[`TargetBefore::Absent`]），不能靠缺行或 NULL 暗示——恢复时
/// “新建”和“读不出来”是两件不同的事（design §11.3）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetBefore {
    /// 目标当时不存在。
    Absent,
    /// 目标当时是这份已确认内容。
    Present(TargetSnapshot),
}

impl TargetBefore {
    /// 现场是否满足这个前提。
    #[must_use]
    pub fn satisfied_by(&self, observation: &TargetObservation) -> bool {
        match (self, observation) {
            (TargetBefore::Absent, TargetObservation::Absent) => true,
            (TargetBefore::Absent, TargetObservation::Present { .. }) => false,
            (TargetBefore::Present(_), TargetObservation::Absent) => false,
            (TargetBefore::Present(snapshot), observation) => snapshot.matches(observation),
        }
    }

    /// `expected_before` 的规范 JSON 文本（恢复时按同一函数还原）。
    ///
    /// 计数与字节一律写成十进制**字符串**：与 `vaultsyncd-domain` 的 `to_decimal` 惯例一致，不用
    /// JSON number 丢精度。键按固定顺序输出，便于比对与日志取证。
    #[must_use]
    pub fn to_json(&self) -> String {
        match self {
            TargetBefore::Absent => r#"{"present":false}"#.to_owned(),
            TargetBefore::Present(snapshot) => format!(
                concat!(
                    r#"{{"present":true"#,
                    r#","entry":"{}""#,
                    r#","revision":"{}""#,
                    r#","digest":"{}""#,
                    r#","size":"{}""#,
                    r#","device":"{}""#,
                    r#","inode":"{}""#,
                    r#","links":"{}"}}"#
                ),
                snapshot.entry.to_hex(),
                snapshot.revision,
                snapshot.digest.to_hex(),
                snapshot.identity.bytes,
                snapshot.identity.device,
                snapshot.identity.inode,
                snapshot.identity.links
            ),
        }
    }

    /// 从 `expected_before` 的 JSON 文本还原；任何不符合本模块编码的文本都报错，不猜现场。
    ///
    /// # Errors
    ///
    /// 见 [`StateJsonError`]：不是 JSON object、缺字段、类型不符、未知键、非法值。
    pub fn from_json(text: &str) -> Result<Self, StateJsonError> {
        let mut fields = parse_flat_object(text)?;
        let present = take_bool(&mut fields, "present")?;
        if !present {
            return finish_object(&fields, TargetBefore::Absent);
        }
        let entry = take_entry(&mut fields, "entry")?;
        let revision = take_u64(&mut fields, "revision")?;
        let digest = take_digest(&mut fields, "digest")?;
        let bytes = take_u64(&mut fields, "size")?;
        let device = take_u64(&mut fields, "device")?;
        let inode = take_u64(&mut fields, "inode")?;
        let links = u32::try_from(take_u64(&mut fields, "links")?).map_err(|_| {
            StateJsonError::InvalidValue {
                field: "links",
                detail: "超出 u32".to_owned(),
            }
        })?;
        finish_object(
            &fields,
            TargetBefore::Present(TargetSnapshot {
                entry,
                revision,
                digest,
                identity: FileKey {
                    device,
                    inode,
                    links,
                    bytes,
                },
            }),
        )
    }
}

/// 意图目标：发布完成后目标应有的内容。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetAfter {
    /// 内容 SHA-256。
    pub digest: ContentDigest,
    /// 内容字节数。
    pub size: u64,
}

impl TargetAfter {
    /// 现场是否已经是这份内容（不看 inode：发布换过文件，内容才是判据）。
    #[must_use]
    pub fn satisfied_by(&self, observation: &TargetObservation) -> bool {
        match observation {
            TargetObservation::Absent => false,
            TargetObservation::Present { identity, digest } => {
                digest == &self.digest && identity.bytes == self.size
            }
        }
    }

    /// 发布结果是否与目标一致（含长度：短写不能当成发布成功）。
    #[must_use]
    pub fn satisfied_by_published(&self, published: &PublishedTarget) -> bool {
        published.digest == self.digest && published.identity.bytes == self.size
    }

    /// `desired_after` 的规范 JSON 文本。
    #[must_use]
    pub fn to_json(self) -> String {
        format!(
            r#"{{"present":true,"digest":"{}","size":"{}"}}"#,
            self.digest.to_hex(),
            self.size
        )
    }

    /// 从 `desired_after` 的 JSON 文本还原；put 的目标必须是“存在”的形态。
    ///
    /// # Errors
    ///
    /// 见 [`StateJsonError`]；`{"present":false}` 不是合法的 put 目标。
    pub fn from_json(text: &str) -> Result<Self, StateJsonError> {
        let mut fields = parse_flat_object(text)?;
        if !take_bool(&mut fields, "present")? {
            return Err(StateJsonError::InvalidValue {
                field: "present",
                detail: "put 的目标必须是存在的内容".to_owned(),
            });
        }
        let digest = take_digest(&mut fields, "digest")?;
        let size = take_u64(&mut fields, "size")?;
        finish_object(&fields, TargetAfter { digest, size })
    }
}

/// 一条新 put 意图的输入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPutIntent {
    /// 意图身份（由调用方生成，重试必须复用同一个身份）。
    pub intent_id: IntentId,
    /// 所属 Root。
    pub root: RootId,
    /// 目标在 Root 内的相对路径。
    pub target_path: String,
    /// 目标所在子卷内的暂存文件名（单个文件名，发布端口按它定位暂存文件）。
    pub staging_path: String,
    /// 现场前提。
    pub before: TargetBefore,
    /// 目标内容。
    pub after: TargetAfter,
}

impl NewPutIntent {
    /// 用 `prepared` 阶段表示这条意图（写库成功后磁盘尚未改动时的形态）。
    #[must_use]
    pub fn prepared(&self) -> PutIntent {
        PutIntent {
            intent_id: self.intent_id,
            root: self.root,
            target_path: self.target_path.clone(),
            staging_path: self.staging_path.clone(),
            before: self.before.clone(),
            after: self.after,
            phase: PutPhase::Prepared,
        }
    }
}

/// 一条已持久化的 put 意图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutIntent {
    /// 意图身份。
    pub intent_id: IntentId,
    /// 所属 Root。
    pub root: RootId,
    /// 目标在 Root 内的相对路径。
    pub target_path: String,
    /// 暂存文件名。
    pub staging_path: String,
    /// 现场前提（已解码）。
    pub before: TargetBefore,
    /// 目标内容（已解码）。
    pub after: TargetAfter,
    /// 持久化阶段。
    pub phase: PutPhase,
}

impl PutIntent {
    /// 恢复判断用的一句话现场描述（日志与恢复报告）。
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "意图 {} 阶段 {} 目标 {} 期望 {}（{} 字节）",
            self.intent_id,
            self.phase.as_text(),
            self.target_path,
            self.after.digest,
            self.after.size
        )
    }
}

/// 一次成功发布的证据：意图已推进到 `published`，工作文件实测与 `desired_after` 一致。
#[derive(Clone, Debug)]
pub struct PublishedPut {
    /// 推进到 `published` 后的意图记录。
    pub intent: PutIntent,
    /// 发布后目标的实测现场。
    pub target: PublishedTarget,
}

/// put 失败的原因。
#[derive(Clone, Debug)]
pub enum PutFailureReason {
    /// 请求本身不成立（暂存名等），未做任何 I/O。
    InvalidRequest {
        /// 原因文本。
        detail: String,
    },
    /// 现场复核时目标不可读：故障不是“不存在”，也不是删除。
    TargetUnreadable(TargetError),
    /// 现场与 `expected_before` 不符：不写意图、不动磁盘（design §11.2 步骤 4）。
    BeforeMismatch {
        /// 意图前提。
        expected: TargetBefore,
        /// 实测现场。
        observed: TargetObservation,
    },
    /// 意图日志失败（写 prepared 或推进阶段）。
    Intent(IntentLogError),
    /// 发布动作失败（含 `ENOSPC` 等存储分类）。
    Publish(TargetError),
    /// 发布后的实测内容不等于 `desired_after`：不推进阶段。
    PublishedTargetMismatch {
        /// 意图目标。
        expected: TargetAfter,
        /// 发布后实测现场。
        observed: PublishedTarget,
    },
    /// Root epoch 与请求预期不符：服务器事实已重建，旧提交作废（`EPOCH_CHANGED`）。
    EpochChanged {
        /// 请求携带的 epoch。
        expected: ServerEpoch,
        /// Root 当前的 epoch。
        actual: ServerEpoch,
    },
    /// 条目 revision 已不是请求基线：过期基线必须重新规划（`BASE_MISMATCH`，I04）。
    BaseMismatch {
        /// 原因文本。
        detail: String,
    },
    /// 父目录或名称在计划后变化（`SUBTREE_CHANGED`）。
    SubtreeChanged {
        /// 原因文本。
        detail: String,
    },
    /// 工作目录现场与请求指纹不符（`RECONCILE_REQUIRED`）：先采集外部变化再重新计划。
    Reconcile {
        /// 请求的现场前提。
        expected: TargetBefore,
        /// 实测现场。
        observed: TargetObservation,
    },
    /// 条目不能承载内容（目录等）：标记该条目，不拖垮整个 Root（`UNSUPPORTED_ENTRY`）。
    UnsupportedEntry {
        /// 原因文本。
        detail: String,
    },
    /// 内容对象尚未持久化：没有可发布的内容，不得开始意图。
    ObjectNotPersisted {
        /// 缺失的内容地址。
        digest: ContentDigest,
    },
    /// 已登记对象的长度与请求内容不符：上传事实与对象事实冲突。
    ObjectSizeMismatch {
        /// 请求内容的字节数。
        expected: u64,
        /// 对象登记的字节数。
        actual: u64,
    },
    /// 同一 Root 已有提交在进行：不并发修改同一棵树。
    Gate(CommitGateError),
    /// 存储层失败（基线读取、对象查询或元数据事务）。
    Store(PutStoreError),
    /// 同一 `OpId` 已用不同请求摘要登记过：协议错误，停止并交给人排查（I06）。
    OpIdReused {
        /// 被重复使用的操作身份。
        op: OpId,
        /// 首次登记时记录的请求摘要。
        recorded: RequestDigest,
        /// 本次请求的摘要。
        requested: RequestDigest,
    },
    /// 该 `OpId` 已登记但尚未产生结果：`pending` 不是 committed，不得重做（I01、design §9.4）。
    OperationPending {
        /// 尚未收尾的操作身份。
        op: OpId,
    },
    /// head revision 仍是请求基线：本次提交没有过期，不构成冲突副本。
    ConflictNotStale {
        /// 请求基线。
        expected: Revision,
        /// 库里的当前值。
        actual: Revision,
    },
    /// head 内容已经等于请求内容：同结果只归并，不额外造副本（design §10.1 第二行）。
    ContentAlreadyCommitted {
        /// 已提交的内容摘要。
        digest: ContentDigest,
    },
    /// 冲突提交的存储层失败（源条目现状、幂等记录或冲突事务）。
    ConflictStore(ConflictStoreError),
    /// 存储层报告事务成功，但返回的证据与请求不一致：适配器缺陷，必须停写人工核对。
    StoreContract {
        /// 原因文本。
        detail: String,
    },
}

impl PutFailureReason {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            PutFailureReason::InvalidRequest { .. } => "invalid_request",
            PutFailureReason::TargetUnreadable(error) => error.code(),
            PutFailureReason::BeforeMismatch { .. } => "before_mismatch",
            PutFailureReason::Intent(error) => error.code(),
            PutFailureReason::Publish(error) => error.code(),
            PutFailureReason::PublishedTargetMismatch { .. } => "published_target_mismatch",
            PutFailureReason::EpochChanged { .. } => "epoch_changed",
            PutFailureReason::BaseMismatch { .. } => "base_mismatch",
            PutFailureReason::SubtreeChanged { .. } => "subtree_changed",
            PutFailureReason::Reconcile { .. } => "reconcile_required",
            PutFailureReason::UnsupportedEntry { .. } => "unsupported_entry",
            PutFailureReason::ObjectNotPersisted { .. } => "object_not_persisted",
            PutFailureReason::ObjectSizeMismatch { .. } => "object_size_mismatch",
            PutFailureReason::Gate(error) => error.code(),
            PutFailureReason::Store(error) => error.code(),
            PutFailureReason::StoreContract { .. } => "store_contract_violation",
            PutFailureReason::OpIdReused { .. } => "op_id_reused",
            PutFailureReason::OperationPending { .. } => "operation_pending",
            PutFailureReason::ConflictNotStale { .. } => "conflict_not_stale",
            PutFailureReason::ContentAlreadyCommitted { .. } => "content_already_committed",
            PutFailureReason::ConflictStore(error) => error.code(),
        }
    }

    /// wire 错误码（design §15.4）；服务端内部原因没有对应 wire 码时返回 `None`。
    ///
    /// 基线类拒绝必须映射到已经定义的码，客户端据此决定重新规划：
    /// 过期 revision → `BASE_MISMATCH`，父目录/名称变化 → `SUBTREE_CHANGED`，现场指纹变化 →
    /// `RECONCILE_REQUIRED`，epoch 变化 → `EPOCH_CHANGED`（I04）。
    #[must_use]
    pub fn error_code(&self) -> Option<ErrorCode> {
        match self {
            PutFailureReason::EpochChanged { .. } => Some(ErrorCode::EpochChanged),
            PutFailureReason::BaseMismatch { .. } => Some(ErrorCode::BaseMismatch),
            PutFailureReason::SubtreeChanged { .. } => Some(ErrorCode::SubtreeChanged),
            PutFailureReason::Reconcile { .. } => Some(ErrorCode::ReconcileRequired),
            PutFailureReason::UnsupportedEntry { .. } => Some(ErrorCode::UnsupportedEntry),
            PutFailureReason::StoreContract { .. } => Some(ErrorCode::NeedsRecovery),
            PutFailureReason::OpIdReused { .. } => Some(ErrorCode::OpIdReused),
            PutFailureReason::OperationPending { .. } => Some(ErrorCode::NeedsRecovery),
            PutFailureReason::ConflictNotStale { .. } => Some(ErrorCode::ReconcileRequired),
            PutFailureReason::ContentAlreadyCommitted { .. } => Some(ErrorCode::ReconcileRequired),
            PutFailureReason::InvalidRequest { .. }
            | PutFailureReason::TargetUnreadable(_)
            | PutFailureReason::BeforeMismatch { .. }
            | PutFailureReason::Intent(_)
            | PutFailureReason::Publish(_)
            | PutFailureReason::PublishedTargetMismatch { .. }
            | PutFailureReason::ObjectNotPersisted { .. }
            | PutFailureReason::ObjectSizeMismatch { .. }
            | PutFailureReason::Gate(_)
            | PutFailureReason::Store(_)
            | PutFailureReason::ConflictStore(_) => None,
        }
    }
}

impl fmt::Display for PutFailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PutFailureReason::InvalidRequest { detail } => write!(f, "请求不成立：{detail}"),
            PutFailureReason::TargetUnreadable(error) => write!(f, "{error}"),
            PutFailureReason::BeforeMismatch { expected, observed } => {
                write!(f, "现场 {observed} 与意图前提 {} 不符", expected.to_json())
            }
            PutFailureReason::Intent(error) => write!(f, "{error}"),
            PutFailureReason::Publish(error) => write!(f, "{error}"),
            PutFailureReason::PublishedTargetMismatch { expected, observed } => write!(
                f,
                "发布后实测 {}（{} 字节，设备 {} inode {}）与意图目标 {} 不符",
                observed.digest,
                observed.identity.bytes,
                observed.identity.device,
                observed.identity.inode,
                expected.to_json()
            ),
            PutFailureReason::EpochChanged { expected, actual } => write!(
                f,
                "Root epoch 已是 {actual}，请求预期是 {expected}：服务器事实已重建，旧提交作废"
            ),
            PutFailureReason::BaseMismatch { detail } => {
                write!(f, "基线已不是请求的 revision：{detail}")
            }
            PutFailureReason::SubtreeChanged { detail } => {
                write!(f, "命名空间基线已变化：{detail}")
            }
            PutFailureReason::Reconcile { expected, observed } => write!(
                f,
                "工作目录现场 {observed} 与请求指纹 {} 不符，需要先协调",
                expected.to_json()
            ),
            PutFailureReason::UnsupportedEntry { detail } => {
                write!(f, "条目不支持内容提交：{detail}")
            }
            PutFailureReason::ObjectNotPersisted { digest } => {
                write!(f, "内容对象 {digest} 尚未持久化")
            }
            PutFailureReason::ObjectSizeMismatch { expected, actual } => {
                write!(f, "对象登记 {actual} 字节，请求内容 {expected} 字节")
            }
            PutFailureReason::Gate(error) => write!(f, "{error}"),
            PutFailureReason::Store(error) => write!(f, "{error}"),
            PutFailureReason::StoreContract { detail } => write!(
                f,
                "存储层报告提交成功但证据不一致：{detail}（必须停写人工核对）"
            ),
            PutFailureReason::OpIdReused {
                op,
                recorded,
                requested,
            } => write!(
                f,
                "操作 {op} 已用请求摘要 {} 登记过，本次是 {}：同一 OpId 不同请求",
                recorded.to_hex(),
                requested.to_hex()
            ),
            PutFailureReason::OperationPending { op } => write!(
                f,
                "操作 {op} 已登记但尚未产生结果，不得重做，等恢复流程收尾"
            ),
            PutFailureReason::ConflictNotStale { expected, actual } => write!(
                f,
                "条目 revision 仍是请求基线 {expected}（当前 {actual}）：本次提交没有过期，不是冲突副本"
            ),
            PutFailureReason::ContentAlreadyCommitted { digest } => {
                write!(f, "head 内容已经是 {digest}：同结果归并，不再造副本")
            }
            PutFailureReason::ConflictStore(error) => write!(f, "{error}"),
        }
    }
}

/// put 失败：原因 + 已持久化的意图身份与阶段。
///
/// 已写入的意图行不会被删除或被回退：它就是恢复时要读的现场证据（design §11.3）。因此失败的
/// 返回值里同时给出 `intent_id` 与最近一次成功持久化的阶段，调用方可以直接查库。
#[derive(Clone, Debug)]
pub struct PutFailure {
    /// 失败原因。
    pub reason: PutFailureReason,
    /// 已持久化的意图（未写库时为 `None`）。
    pub intent_id: Option<IntentId>,
    /// 最近一次成功持久化的阶段。
    pub phase: Option<PutPhase>,
}

impl PutFailure {
    /// 用原因构造“尚未写库”的失败。
    #[must_use]
    pub fn new(reason: PutFailureReason) -> Self {
        PutFailure {
            reason,
            intent_id: None,
            phase: None,
        }
    }

    /// 用原因与该意图已持久化的阶段构造失败。
    #[must_use]
    pub fn after_intent(reason: PutFailureReason, intent: &PutIntent) -> Self {
        PutFailure {
            reason,
            intent_id: Some(intent.intent_id),
            phase: Some(intent.phase),
        }
    }

    /// 失败是否产生已提交版本：恒为 `false`（没有元数据事务就没有 committed）。
    #[must_use]
    pub fn is_committed(&self) -> bool {
        false
    }

    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.reason.code()
    }

    /// wire 错误码（design §15.4）；服务端内部原因没有对应 wire 码时返回 `None`。
    #[must_use]
    pub fn error_code(&self) -> Option<ErrorCode> {
        self.reason.error_code()
    }
}

impl fmt::Display for PutFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.intent_id, self.phase) {
            (Some(id), Some(phase)) => {
                write!(f, "{}（意图 {id} 停在 {}）", self.reason, phase.as_text())
            }
            _ => write!(f, "{}", self.reason),
        }
    }
}

impl std::error::Error for PutFailure {}

/// 单文件 put 的提交前半段：现场复核 → 持久意图 → 发布工作文件 → 记 `published`。
///
/// 顺序即不变量：写意图失败就不会发布；发布失败或发布后复核失败时意图停在 `prepared`（可重做，
/// design §11.3 第 3 行）；成功返回时目标是 `desired_after` 的内容、意图是 `published`。
///
/// 元数据事务（entries/versions/changes/operations）与 `finalized` 不在本函数内：S035 在一次数据库
/// 事务里写这些行并调用 [`finalize_put`]，让“最终逻辑状态”与阶段推进原子提交（design §11.2
/// 步骤 7）。
///
/// # Errors
///
/// 见 [`PutFailure`]：请求不成立、目标不可读、前提不符、意图日志失败、发布失败、发布后不符。
pub async fn publish_put<L, T>(
    log: &mut L,
    target: &mut T,
    at: UtcTimestamp,
    new: &NewPutIntent,
) -> Result<PublishedPut, Box<PutFailure>>
where
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
{
    // 暂存名必须是单个文件名：意图里的暂存路径会被发布端口直接使用，`../` 等于把发布指向别处。
    if let Err(error) = ObjectLayout::validate_staging_name(&new.staging_path) {
        return Err(Box::new(PutFailure::new(
            PutFailureReason::InvalidRequest {
                detail: error.to_string(),
            },
        )));
    }

    // 步骤 4：基线/现场复核。前提不成立就什么都不写、什么都不动。
    let observed = target
        .observe(&new.target_path)
        .map_err(|error| Box::new(PutFailure::new(PutFailureReason::TargetUnreadable(error))))?;
    if !new.before.satisfied_by(&observed) {
        return Err(Box::new(PutFailure::new(
            PutFailureReason::BeforeMismatch {
                expected: new.before.clone(),
                observed,
            },
        )));
    }

    // 步骤 5：先持久意图。
    let prepared = log
        .begin_put(new, at)
        .await
        .map_err(|error| Box::new(PutFailure::new(PutFailureReason::Intent(error))))?;

    // 步骤 6：发布工作文件。
    let request = PublishRequest {
        target_path: new.target_path.clone(),
        staging_path: new.staging_path.clone(),
        digest: new.after.digest,
        size: new.after.size,
    };
    let published = match target.publish(&request) {
        Ok(published) => published,
        Err(error) => {
            return Err(Box::new(PutFailure::after_intent(
                PutFailureReason::Publish(error),
                &prepared,
            )));
        }
    };

    // 发布后复核：短写、被外部改写都不算发布成功，也不推进阶段。
    if !new.after.satisfied_by_published(&published) {
        return Err(Box::new(PutFailure::after_intent(
            PutFailureReason::PublishedTargetMismatch {
                expected: new.after,
                observed: published,
            },
            &prepared,
        )));
    }

    let intent = log
        .advance_put(new.intent_id, PutPhase::Published, at)
        .await
        .map_err(|error| {
            Box::new(PutFailure::after_intent(
                PutFailureReason::Intent(error),
                &prepared,
            ))
        })?;
    Ok(PublishedPut {
        intent,
        target: published,
    })
}

/// 把已发布的意图推进到 `finalized`（终态）。
///
/// 只有元数据事务已经能提交时才调用：S035 在同一个数据库事务里写 entries/versions/changes 并调用
/// 本函数，事务失败即回滚，阶段不会停在 `finalized`。
///
/// # Errors
///
/// 见 [`PutFailure`]：意图日志失败（含非法转换：未发布就收尾会被拒绝）。
pub async fn finalize_put<L>(
    log: &mut L,
    id: IntentId,
    at: UtcTimestamp,
) -> Result<PutIntent, Box<PutFailure>>
where
    L: IntentLog + ?Sized,
{
    log.advance_put(id, PutPhase::Finalized, at)
        .await
        .map_err(|error| {
            Box::new(PutFailure {
                reason: PutFailureReason::Intent(error),
                intent_id: Some(id),
                phase: None,
            })
        })
}

/// 意图证据 JSON 的编解码失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateJsonError {
    /// 文本不是 JSON object（含语法错误、尾随内容、非 object 顶层值）。
    NotAnObject {
        /// 原因文本。
        detail: String,
    },
    /// 出现本模块编码之外的键。
    UnknownField {
        /// 键名。
        field: String,
    },
    /// 缺少本模块编码要求的键。
    MissingField {
        /// 键名。
        field: &'static str,
    },
    /// 键存在但类型不符（例如 `present` 不是布尔、计数不是十进制字符串）。
    TypeMismatch {
        /// 键名。
        field: &'static str,
        /// 期望的类型描述。
        expected: &'static str,
    },
    /// 键的类型对但值不合法（负号、溢出、非十六进制摘要等）。
    InvalidValue {
        /// 键名。
        field: &'static str,
        /// 原因文本。
        detail: String,
    },
}

impl StateJsonError {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            StateJsonError::NotAnObject { .. } => "state_not_object",
            StateJsonError::UnknownField { .. } => "state_unknown_field",
            StateJsonError::MissingField { .. } => "state_missing_field",
            StateJsonError::TypeMismatch { .. } => "state_type_mismatch",
            StateJsonError::InvalidValue { .. } => "state_invalid_value",
        }
    }
}

impl fmt::Display for StateJsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StateJsonError::NotAnObject { detail } => {
                write!(f, "意图证据不是 JSON object：{detail}")
            }
            StateJsonError::UnknownField { field } => {
                write!(f, "意图证据出现未知键 {field:?}")
            }
            StateJsonError::MissingField { field } => write!(f, "意图证据缺少键 {field:?}"),
            StateJsonError::TypeMismatch { field, expected } => {
                write!(f, "意图证据键 {field:?} 必须是{expected}")
            }
            StateJsonError::InvalidValue { field, detail } => {
                write!(f, "意图证据键 {field:?} 值不合法：{detail}")
            }
        }
    }
}

impl std::error::Error for StateJsonError {}

/// 解码时接受的 JSON 值：布尔与字符串。
///
/// 不接受数字与 `null`：本模块把所有计数与字节写成十进制字符串（避免 JSON number 丢精度），
/// “未知”由缺字段表达而不是 `null`。因此这里是一个**故意收窄**的解析器：写到库里的证据必然能
/// 读回来，别的写法一律拒绝而不是猜。
pub(crate) enum JsonValue {
    Bool(bool),
    Text(String),
}

pub(crate) fn parse_flat_object(text: &str) -> Result<BTreeMap<String, JsonValue>, StateJsonError> {
    let mut scanner = Scanner {
        bytes: text.as_bytes(),
        pos: 0,
    };
    let mut fields = BTreeMap::new();
    scanner.skip_whitespace();
    scanner.expect(b'{')?;
    scanner.skip_whitespace();
    if scanner.peek() == Some(b'}') {
        scanner.pos += 1;
    } else {
        loop {
            scanner.skip_whitespace();
            let key = scanner.string()?;
            scanner.skip_whitespace();
            scanner.expect(b':')?;
            scanner.skip_whitespace();
            let value = scanner.value()?;
            if fields.insert(key.clone(), value).is_some() {
                return Err(StateJsonError::InvalidValue {
                    field: "object",
                    detail: format!("键 {key:?} 重复"),
                });
            }
            scanner.skip_whitespace();
            match scanner.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(syntax("object 内缺少 ',' 或 '}'")),
            }
        }
    }
    scanner.skip_whitespace();
    if scanner.pos != scanner.bytes.len() {
        return Err(syntax("object 之后还有多余内容"));
    }
    Ok(fields)
}

fn syntax(detail: &str) -> StateJsonError {
    StateJsonError::NotAnObject {
        detail: detail.to_owned(),
    }
}

struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek();
        if byte.is_some() {
            self.pos += 1;
        }
        byte
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), StateJsonError> {
        if self.bump() == Some(byte) {
            Ok(())
        } else {
            Err(syntax(&format!("缺少 '{}'", char::from(byte))))
        }
    }

    fn literal(&mut self, word: &[u8]) -> Result<(), StateJsonError> {
        for expected in word {
            if self.bump() != Some(*expected) {
                return Err(syntax("字面量不完整"));
            }
        }
        Ok(())
    }

    /// 字符串字面量。本模块编码的键与值都是 ASCII 标识符/十六进制/十进制，因此不接受转义序列。
    fn string(&mut self) -> Result<String, StateJsonError> {
        self.expect(b'"')?;
        let start = self.pos;
        loop {
            match self.bump() {
                Some(b'"') => {
                    let raw = &self.bytes[start..self.pos - 1];
                    return std::str::from_utf8(raw)
                        .map(str::to_owned)
                        .map_err(|_| syntax("字符串不是合法 UTF-8"));
                }
                Some(b'\\') => return Err(syntax("不接受转义序列")),
                Some(byte) if byte < 0x20 => return Err(syntax("字符串含控制字符")),
                Some(_) => {}
                None => return Err(syntax("字符串没有结束引号")),
            }
        }
    }

    fn value(&mut self) -> Result<JsonValue, StateJsonError> {
        match self.peek() {
            Some(b't') => {
                self.literal(b"true")?;
                Ok(JsonValue::Bool(true))
            }
            Some(b'f') => {
                self.literal(b"false")?;
                Ok(JsonValue::Bool(false))
            }
            Some(b'"') => Ok(JsonValue::Text(self.string()?)),
            Some(b'n') => Err(syntax("不接受 null：未知必须由缺字段表达")),
            Some(b'0'..=b'9') | Some(b'-') => {
                Err(syntax("不接受 JSON number：计数以十进制字符串记录"))
            }
            _ => Err(syntax("值必须是 true/false 或字符串")),
        }
    }
}

pub(crate) fn take_bool(
    fields: &mut BTreeMap<String, JsonValue>,
    field: &'static str,
) -> Result<bool, StateJsonError> {
    match fields.remove(field) {
        Some(JsonValue::Bool(value)) => Ok(value),
        Some(JsonValue::Text(_)) => Err(StateJsonError::TypeMismatch {
            field,
            expected: "布尔值",
        }),
        None => Err(StateJsonError::MissingField { field }),
    }
}

pub(crate) fn take_text(
    fields: &mut BTreeMap<String, JsonValue>,
    field: &'static str,
) -> Result<String, StateJsonError> {
    match fields.remove(field) {
        Some(JsonValue::Text(value)) => Ok(value),
        Some(JsonValue::Bool(_)) => Err(StateJsonError::TypeMismatch {
            field,
            expected: "字符串",
        }),
        None => Err(StateJsonError::MissingField { field }),
    }
}

pub(crate) fn take_u64(
    fields: &mut BTreeMap<String, JsonValue>,
    field: &'static str,
) -> Result<u64, StateJsonError> {
    let text = take_text(fields, field)?;
    text.parse::<u64>()
        .map_err(|_| StateJsonError::InvalidValue {
            field,
            detail: format!("{text:?} 不是无符号十进制整数"),
        })
}

fn take_digest(
    fields: &mut BTreeMap<String, JsonValue>,
    field: &'static str,
) -> Result<ContentDigest, StateJsonError> {
    let text = take_text(fields, field)?;
    ContentDigest::from_hex(&text).map_err(|error: DigestParseError| StateJsonError::InvalidValue {
        field,
        detail: error.to_string(),
    })
}

pub(crate) fn take_entry(
    fields: &mut BTreeMap<String, JsonValue>,
    field: &'static str,
) -> Result<EntryId, StateJsonError> {
    let text = take_text(fields, field)?;
    EntryId::from_hex(&text).map_err(|error| StateJsonError::InvalidValue {
        field,
        detail: error.to_string(),
    })
}

/// 所有已知键都取完后，剩下的键就是本模块不认识的证据：拒绝，而不是忽略。
pub(crate) fn finish_object<T>(fields: &BTreeMap<String, JsonValue>, value: T) -> Result<T, StateJsonError> {
    match fields.keys().next() {
        Some(field) => Err(StateJsonError::UnknownField {
            field: field.clone(),
        }),
        None => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ports::object_store::StorageFailure;
    use crate::test_support::{
        FakeIntentLog, FakeTarget, intent_id, linked_ports, target_snapshot, user_time,
    };

    const HEX_A: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const HEX_B: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn digest(hex: &str) -> ContentDigest {
        ContentDigest::from_hex(hex).expect("测试用摘要合法")
    }

    fn after(hex: &str, size: u64) -> TargetAfter {
        TargetAfter {
            digest: digest(hex),
            size,
        }
    }

    fn plan(before: TargetBefore, after_value: TargetAfter) -> NewPutIntent {
        NewPutIntent {
            intent_id: intent_id(0x41),
            root: crate::test_support::root_id(0x21),
            target_path: "docs/note.txt".to_owned(),
            staging_path: "put-0041".to_owned(),
            before,
            after: after_value,
        }
    }

    #[tokio::test]
    async fn happy_path_persists_intent_before_publishing_and_records_each_phase() {
        let (mut log, mut target) = linked_ports();
        let new = plan(TargetBefore::Absent, after(HEX_A, 5));

        let published = publish_put(&mut log, &mut target, user_time(100), &new)
            .await
            .expect("发布成功");
        assert_eq!(published.intent.phase, PutPhase::Published);
        assert_eq!(published.target.digest, digest(HEX_A));
        assert_eq!(
            log.trace.entries(),
            vec![
                "target observe docs/note.txt".to_owned(),
                "intent begin prepared".to_owned(),
                "target publish docs/note.txt".to_owned(),
                "intent advance published".to_owned()
            ],
            "顺序固定：现场复核 → 写 prepared 意图 → 发布工作文件 → 记 published"
        );
        assert_eq!(target.publishes.len(), 1);
        assert_eq!(target.current.digest(), Some(digest(HEX_A)));

        let finalized = finalize_put(&mut log, new.intent_id, user_time(110))
            .await
            .expect("收尾成功");
        assert_eq!(finalized.phase, PutPhase::Finalized);
        assert!(finalized.phase.is_finished());
        assert!(!finalized.phase.is_open());
    }

    #[tokio::test]
    async fn before_mismatch_writes_nothing_and_touches_nothing() {
        let mut log = FakeIntentLog::default();
        let mut target = FakeTarget::absent();
        // 意图说目标本来存在，实测却不存在：这是“新建 vs 覆盖”的前提冲突。
        let new = plan(
            TargetBefore::Present(target_snapshot(HEX_B, 9)),
            after(HEX_A, 5),
        );
        let failure = publish_put(&mut log, &mut target, user_time(100), &new)
            .await
            .expect_err("前提不符");
        assert_eq!(failure.code(), "before_mismatch");
        assert!(!failure.is_committed());
        assert_eq!(failure.intent_id, None, "前提不符不得写意图");
        assert!(log.intents.is_empty());
        assert!(target.publishes.is_empty());
    }

    #[tokio::test]
    async fn unreadable_target_is_not_treated_as_absent() {
        let mut log = FakeIntentLog::default();
        let mut target = FakeTarget::absent();
        target.observe_error = Some(TargetError::Unreadable {
            detail: "EACCES".to_owned(),
        });
        let new = plan(TargetBefore::Absent, after(HEX_A, 5));
        let failure = publish_put(&mut log, &mut target, user_time(100), &new)
            .await
            .expect_err("不可读");
        assert_eq!(failure.code(), "target_unreadable");
        assert!(!failure.is_committed());
        assert!(
            log.intents.is_empty(),
            "读不到现场时不能把故障当成“目标不存在”写意图"
        );
    }

    #[tokio::test]
    async fn begin_failure_never_publishes() {
        let mut log = FakeIntentLog {
            begin_error: Some(IntentLogError::Storage {
                detail: "SQLITE_BUSY".to_owned(),
            }),
            ..FakeIntentLog::default()
        };
        let mut target = FakeTarget::absent();
        let new = plan(TargetBefore::Absent, after(HEX_A, 5));
        let failure = publish_put(&mut log, &mut target, user_time(100), &new)
            .await
            .expect_err("意图写不进去");
        assert_eq!(failure.code(), "intent_storage_failed");
        assert!(!failure.is_committed());
        assert!(
            target.publishes.is_empty(),
            "意图先于发布：写不进去就不得改工作文件"
        );
    }

    #[tokio::test]
    async fn publish_failure_leaves_intent_prepared_and_uncommitted() {
        let mut log = FakeIntentLog::default();
        let mut target = FakeTarget::absent();
        target.publish_error = Some(TargetError::Storage {
            stage: "stage_publish",
            failure: StorageFailure::NoSpace,
            detail: "No space left on device".to_owned(),
        });
        let new = plan(TargetBefore::Absent, after(HEX_A, 5));
        let failure = publish_put(&mut log, &mut target, user_time(100), &new)
            .await
            .expect_err("满盘");
        assert_eq!(failure.code(), "no_space");
        assert!(!failure.is_committed());
        assert_eq!(failure.intent_id, Some(new.intent_id));
        assert_eq!(failure.phase, Some(PutPhase::Prepared));
        let stored = log.intents.first().expect("意图行仍在");
        assert_eq!(stored.phase, PutPhase::Prepared);
        assert_eq!(target.current, TargetObservation::Absent);
    }

    #[tokio::test]
    async fn published_content_mismatch_does_not_advance_the_phase() {
        let (mut log, mut target) = linked_ports();
        // 端口报告发布成功，但实测内容长度被截断：不能记 published。
        target.publish_reports = Some(PublishedTarget {
            identity: FileKey {
                device: 7,
                inode: 91,
                links: 1,
                bytes: 3,
            },
            digest: digest(HEX_A),
        });
        let new = plan(TargetBefore::Absent, after(HEX_A, 5));
        let failure = publish_put(&mut log, &mut target, user_time(100), &new)
            .await
            .expect_err("短写");
        assert_eq!(failure.code(), "published_target_mismatch");
        assert_eq!(failure.phase, Some(PutPhase::Prepared));
        assert!(!failure.is_committed());
        assert!(
            log.intents
                .iter()
                .all(|intent| intent.phase == PutPhase::Prepared)
        );
    }

    #[tokio::test]
    async fn finalize_without_publish_is_rejected_by_the_phase_rules() {
        let mut log = FakeIntentLog::default();
        let new = plan(TargetBefore::Absent, after(HEX_A, 5));
        log.begin_put(&new, user_time(100)).await.expect("写意图");
        let failure = finalize_put(&mut log, new.intent_id, user_time(110))
            .await
            .expect_err("未发布就收尾");
        assert_eq!(failure.code(), "illegal_intent_transition");
        assert!(!failure.is_committed());
        assert_eq!(log.intents[0].phase, PutPhase::Prepared);
    }

    #[tokio::test]
    async fn overwrite_publishes_when_the_expected_before_is_still_present() {
        let snapshot = target_snapshot(HEX_B, 9);
        let (mut log, _) = linked_ports();
        let mut target = FakeTarget::present(&snapshot);
        let new = plan(TargetBefore::Present(snapshot), after(HEX_A, 5));
        let published = publish_put(&mut log, &mut target, user_time(100), &new)
            .await
            .expect("覆盖发布成功");
        assert_eq!(published.intent.phase, PutPhase::Published);
        assert_eq!(published.intent.before.to_json(), new.before.to_json());
        assert_eq!(target.publishes.len(), 1);
        assert_eq!(
            target.publishes[0].digest,
            digest(HEX_A),
            "发布的是意图目标内容，不是旧内容"
        );
        assert_eq!(target.current.digest(), Some(digest(HEX_A)));
    }

    #[test]
    fn evidence_json_round_trips_through_the_canonical_encoding() {
        let absent = TargetBefore::Absent;
        assert_eq!(absent.to_json(), r#"{"present":false}"#);
        assert_eq!(TargetBefore::from_json(&absent.to_json()), Ok(absent));

        let present = TargetBefore::Present(target_snapshot(HEX_A, 5));
        let text = present.to_json();
        let expected = format!(
            r#"{{"present":true,"entry":"{}","revision":"7","digest":"{HEX_A}","size":"5","device":"7","inode":"41","links":"1"}}"#,
            crate::test_support::entry_id(0x51).to_hex()
        );
        assert_eq!(text, expected, "键顺序固定，便于比对与取证");
        assert_eq!(TargetBefore::from_json(&text), Ok(present));

        let target_after = after(HEX_B, 12);
        assert_eq!(
            target_after.to_json(),
            format!(r#"{{"present":true,"digest":"{HEX_B}","size":"12"}}"#)
        );
        assert_eq!(
            TargetAfter::from_json(&target_after.to_json()),
            Ok(target_after)
        );
    }

    #[test]
    fn evidence_json_rejects_anything_outside_the_encoding() {
        // 顶层不是 object、尾随内容、缺字段、未知键、类型错、非法值都必须报错。
        for (text, code) in [
            (r#"[]"#, "state_not_object"),
            (r#"{"present":false} x"#, "state_not_object"),
            (r#"{"present":false,}"#, "state_not_object"),
            (r#"{"present":null}"#, "state_not_object"),
            (r#"{"present":1}"#, "state_not_object"),
            (r#"{"present":true}"#, "state_missing_field"),
            (
                r#"{"present":false,"digest":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
                "state_unknown_field",
            ),
            (r#"{"present":"yes"}"#, "state_type_mismatch"),
            (
                r#"{"present":true,"entry":"zz","revision":"7","digest":"1111111111111111111111111111111111111111111111111111111111111111","size":"5","device":"7","inode":"41","links":"1"}"#,
                "state_invalid_value",
            ),
            (
                r#"{"present":true,"entry":"51515151515151515151515151515151","revision":"-7","digest":"1111111111111111111111111111111111111111111111111111111111111111","size":"5","device":"7","inode":"41","links":"1"}"#,
                "state_invalid_value",
            ),
        ] {
            let error = TargetBefore::from_json(text).expect_err("必须拒绝");
            assert_eq!(error.code(), code, "文本 {text}");
        }
        // put 的目标不能是“不存在”：那会让恢复以为删掉就算完成。
        assert_eq!(
            TargetAfter::from_json(r#"{"present":false}"#)
                .expect_err("目标不能为空")
                .code(),
            "state_invalid_value"
        );
    }

    #[test]
    fn phases_only_advance_along_the_declared_transitions() {
        assert!(PutPhase::Prepared.allows_transition_to(PutPhase::Published));
        assert!(PutPhase::Published.allows_transition_to(PutPhase::Finalized));
        assert!(PutPhase::Prepared.allows_transition_to(PutPhase::NeedsRecovery));
        assert!(PutPhase::Published.allows_transition_to(PutPhase::NeedsRecovery));
        assert!(PutPhase::NeedsRecovery.allows_transition_to(PutPhase::Finalized));
        assert!(!PutPhase::Prepared.allows_transition_to(PutPhase::Finalized));
        assert!(!PutPhase::Published.allows_transition_to(PutPhase::Prepared));
        assert!(!PutPhase::Finalized.allows_transition_to(PutPhase::NeedsRecovery));
        assert!(!PutPhase::NeedsRecovery.allows_transition_to(PutPhase::Published));
        for phase in [
            PutPhase::Prepared,
            PutPhase::Published,
            PutPhase::Finalized,
            PutPhase::NeedsRecovery,
        ] {
            assert_eq!(PutPhase::from_text(phase.as_text()), Some(phase));
        }
        assert_eq!(PutPhase::from_text("published_ish"), None);
    }
}
