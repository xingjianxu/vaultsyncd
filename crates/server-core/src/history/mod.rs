//! 历史与回收站恢复（design §10.3、I12、T13）：按旧不可变版本创建**新的**当前版本，以及把回收站
//! 清单里的文件/目录子树放回命名空间（原位置被占用时另名并回报实际位置）。
//!
//! 两类恢复都复用 put 的提交协调（[`crate::commit::put::RootCommitGate`]、意图状态机、受控发布、
//! 单一元数据事务），差别只在“内容从哪来”和“事务写什么”：
//!
//! - [`version`]：内容取自已提交的旧版本对象。发布前先建立**内容租约**（[`ContentLeasePort`]），
//!   把“旧版本还在保留期内”这个判断与“新版本已引用同一对象”之间的窗口保护起来，避免 GC 在中间
//!   回收对象（design §8.4、§11）。事务只追加新版本行，旧版本行一字不改（I12）。
//! - [`trash`]：内容取自回收站清单里的内容版本。恢复先按清单决定**实际名称**——原位置被活跃条目
//!   占用时另名，绝不覆盖现在的新文件（T13）；父目录不在或不是目录时先报 `SUBTREE_CHANGED`，不
//!   悄悄把子树丢到别处。
//!
//! 两条约束在两层都成立：同一 `OpId` 与同一规范化请求只有一个业务结果（I06），重复恢复返回首次的
//! 实际路径与身份；任何失败都不是 committed，已持久化的意图与 `pending` 记录保留为恢复证据。

use std::fmt;

use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{IntentId, OpId};
use vaultsyncd_domain::version::{ServerEpoch, UtcTimestamp};

use crate::commit::intent::{PutFailure, PutFailureReason, PutIntent, PutPhase, TargetAfter};
use crate::commit::put::CommitGateError;
use crate::ports::conflict_store::RequestDigest;
use crate::ports::content_lease::ContentLeaseError;
use crate::ports::intent_log::IntentLogError;
use crate::ports::namespace::NamespaceError;
use crate::ports::restore_store::RestoreStoreError;
use crate::ports::target::{PublishedTarget, TargetError, TargetObservation};

pub mod trash;
pub mod version;

/// 恢复事务提交后，为保护旧内容而建立的租约最后怎么处理。
///
/// 提交成功之后租约只是“提前解除的优化”：到期时间本身就是兜底，GC 在到期前不会回收该对象。因此
/// 解除失败时如实报告“仍在保护到某时刻”，不把已经提交的业务结果改判为失败（I01：committed 不因
/// 后续清理失败而回滚），也不假装已经解除。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseDisposition {
    /// 租约已解除。
    Released,
    /// 租约解除失败，仍将保护内容到该时刻（到期后 GC 按引用标记回收）。
    Expires {
        /// 到期时间。
        at: UtcTimestamp,
    },
}

/// 一次恢复的失败：`is_committed()` 恒为 `false`，已持久化的意图/`pending` 记录留作恢复证据。
#[derive(Clone, Debug)]
pub struct RestoreFailure {
    /// 失败原因。
    pub reason: RestoreFailureReason,
    /// 已持久化的意图（未写库时为 `None`）：恢复流程靠它继续判断该重做、收尾还是停写。
    pub intent_id: Option<IntentId>,
    /// 最近一次成功持久化的阶段。
    pub phase: Option<PutPhase>,
    /// 失败路径上没能解除的内容租约到期时间。
    ///
    /// 有值时表示“这个对象仍被保护到该时刻，到期后 GC 才会按引用标记回收”。它是清理状态而不是主因，
    /// 因此不改写失败原因；`None` 表示没有遗留租约。
    pub lease_expires_at: Option<UtcTimestamp>,
}

impl RestoreFailure {
    /// 用原因构造“尚未写库”的失败。
    #[must_use]
    pub fn new(reason: RestoreFailureReason) -> Self {
        Self {
            reason,
            intent_id: None,
            phase: None,
            lease_expires_at: None,
        }
    }

    /// 用原因与该意图已持久化的阶段构造失败。
    #[must_use]
    pub fn after_intent(reason: RestoreFailureReason, intent: &PutIntent) -> Self {
        Self {
            reason,
            intent_id: Some(intent.intent_id),
            phase: Some(intent.phase),
            lease_expires_at: None,
        }
    }

    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.reason.code()
    }

    /// wire 错误码（design §15.4）；服务端内部原因没有对应码时返回 `None`，由 API 层按端点决定
    /// 状态码。
    #[must_use]
    pub fn error_code(&self) -> Option<ErrorCode> {
        self.reason.error_code()
    }

    /// 是否 committed。恢复永远不返回 committed 的失败：事务没提交就没有成功结果。
    #[must_use]
    pub const fn is_committed(&self) -> bool {
        false
    }
}

impl fmt::Display for RestoreFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.intent_id, self.phase) {
            (Some(id), Some(phase)) => {
                write!(f, "{}（意图 {id} 停在 {}）", self.reason, phase.as_text())?
            }
            _ => write!(f, "{}", self.reason)?,
        }
        if let Some(at) = self.lease_expires_at {
            write!(f, "（内容租约未能解除，仍保护到 {at}）")?;
        }
        Ok(())
    }
}

impl std::error::Error for RestoreFailure {}

/// 恢复失败的原因。
#[derive(Clone, Debug)]
pub enum RestoreFailureReason {
    /// 请求本身不成立（暂存名、路径、现场指纹与基线不一致），未做任何 I/O。
    InvalidRequest {
        /// 原因文本。
        detail: String,
    },
    /// 同一 Root 已有提交在进行：不并发修改同一棵树。
    Gate(CommitGateError),
    /// Root epoch 与请求预期不符：服务器事实已重建，旧恢复作废（`EPOCH_CHANGED`）。
    EpochChanged {
        /// 请求携带的 epoch。
        expected: ServerEpoch,
        /// Root 当前的 epoch。
        actual: ServerEpoch,
    },
    /// 条目 revision 已不是请求基线（`BASE_MISMATCH`）。
    BaseMismatch {
        /// 原因文本。
        detail: String,
    },
    /// 父目录/名称在计划后变化，或恢复目标不可用（`SUBTREE_CHANGED`）。
    SubtreeChanged {
        /// 原因文本。
        detail: String,
    },
    /// 要恢复的版本不存在、不属于该条目，或已过历史保留期（`VERSION_GONE`）。
    VersionGone {
        /// 原因文本。
        detail: String,
    },
    /// 条目不能承载内容（目录等，`UNSUPPORTED_ENTRY`）。
    UnsupportedEntry {
        /// 原因文本。
        detail: String,
    },
    /// 现场复核时目标不可读：故障不是“不存在”。
    TargetUnreadable(TargetError),
    /// 工作目录现场与请求指纹不符（`RECONCILE_REQUIRED`）：先采集外部变化再重新计划。
    Reconcile {
        /// 请求的现场前提（用文本表示，避免为一个失败原因暴露意图编码）。
        expected: String,
        /// 实测现场。
        observed: TargetObservation,
    },
    /// 同一 `OpId` 已用不同请求摘要登记过：协议错误，停止并交给人排查（`OP_ID_REUSED`）。
    OperationReused {
        /// 被重复使用的操作身份。
        op: OpId,
        /// 首次登记时记录的请求摘要。
        recorded: RequestDigest,
        /// 本次请求的摘要。
        requested: RequestDigest,
    },
    /// 该 `OpId` 已登记但尚未产生结果：`pending` 不是 committed，不得重做（`NEEDS_RECOVERY`）。
    OperationPending {
        /// 尚未收尾的操作身份。
        op: OpId,
    },
    /// 回收站清单里不再有该条目（已被恢复或内容已过期回收）。
    NotInTrash {
        /// 原因文本。
        detail: String,
    },
    /// 带后代的大目录子树恢复尚未接入（`restore_subtree_unsupported`）：不做部分恢复。
    UnsupportedSubtree {
        /// 原因文本。
        detail: String,
    },
    /// 工作目录命名空间动作失败（建目录等）。
    Namespace(NamespaceError),
    /// 同名目标已被占用且没有可用的另名（`restore_name_unavailable`）：不覆盖现有内容。
    NameUnavailable {
        /// 原因文本。
        detail: String,
    },
    /// 内容租约失败：没有保护就不能在“读到旧版本”之后继续发布。
    Lease(ContentLeaseError),
    /// 意图日志失败（写 prepared 或推进阶段）。
    Intent(IntentLogError),
    /// 发布动作失败（含 `ENOSPC` 等存储分类）。
    Publish(TargetError),
    /// 发布后的实测内容不等于目标：不推进阶段。
    PublishedTargetMismatch {
        /// 意图目标。
        expected: TargetAfter,
        /// 发布后实测现场。
        observed: PublishedTarget,
    },
    /// 恢复存储层失败（事实读取、幂等记录或元数据事务）。
    Store(RestoreStoreError),
    /// 存储层报告事务成功，但返回的证据与请求不一致：适配器缺陷，必须停写人工核对。
    StoreContract {
        /// 原因文本。
        detail: String,
    },
}

impl RestoreFailureReason {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            RestoreFailureReason::InvalidRequest { .. } => "invalid_request",
            RestoreFailureReason::Gate(error) => error.code(),
            RestoreFailureReason::EpochChanged { .. } => "epoch_changed",
            RestoreFailureReason::BaseMismatch { .. } => "base_mismatch",
            RestoreFailureReason::SubtreeChanged { .. } => "subtree_changed",
            RestoreFailureReason::VersionGone { .. } => "version_gone",
            RestoreFailureReason::UnsupportedEntry { .. } => "unsupported_entry",
            RestoreFailureReason::TargetUnreadable(error) => error.code(),
            RestoreFailureReason::Reconcile { .. } => "reconcile_required",
            RestoreFailureReason::OperationReused { .. } => "op_id_reused",
            RestoreFailureReason::OperationPending { .. } => "operation_pending",
            RestoreFailureReason::NotInTrash { .. } => "restore_not_in_trash",
            RestoreFailureReason::NameUnavailable { .. } => "restore_name_unavailable",
            RestoreFailureReason::UnsupportedSubtree { .. } => "restore_subtree_unsupported",
            RestoreFailureReason::Namespace(error) => error.code(),
            RestoreFailureReason::Lease(error) => error.code(),
            RestoreFailureReason::Intent(error) => error.code(),
            RestoreFailureReason::Publish(error) => error.code(),
            RestoreFailureReason::PublishedTargetMismatch { .. } => "published_target_mismatch",
            RestoreFailureReason::Store(error) => error.code(),
            RestoreFailureReason::StoreContract { .. } => "store_contract_violation",
        }
    }

    /// wire 错误码（design §15.4）；没有对应码的内部原因返回 `None`。
    #[must_use]
    pub fn error_code(&self) -> Option<ErrorCode> {
        match self {
            RestoreFailureReason::EpochChanged { .. } => Some(ErrorCode::EpochChanged),
            RestoreFailureReason::BaseMismatch { .. } => Some(ErrorCode::BaseMismatch),
            RestoreFailureReason::SubtreeChanged { .. } => Some(ErrorCode::SubtreeChanged),
            RestoreFailureReason::VersionGone { .. } => Some(ErrorCode::VersionGone),
            RestoreFailureReason::UnsupportedEntry { .. } => Some(ErrorCode::UnsupportedEntry),
            RestoreFailureReason::Reconcile { .. } => Some(ErrorCode::ReconcileRequired),
            RestoreFailureReason::OperationReused { .. } => Some(ErrorCode::OpIdReused),
            RestoreFailureReason::OperationPending { .. }
            | RestoreFailureReason::StoreContract { .. } => Some(ErrorCode::NeedsRecovery),
            // 同名目标被占用是“重新规划”的情况，但恢复的另名规则已经把可自动解决的部分吃掉了；
            // 走到这里说明连备用名都没有，客户端刷新回收站与目录事实后重新发起（design §10.1 末行）。
            RestoreFailureReason::NameUnavailable { .. } => Some(ErrorCode::SubtreeChanged),
            // 清单里不再有该条目：客户端刷新回收站清单即可，服务端无更精确的 wire 码。
            RestoreFailureReason::NotInTrash { .. } => Some(ErrorCode::BaseMismatch),
            RestoreFailureReason::UnsupportedSubtree { .. } => Some(ErrorCode::UnsupportedEntry),
            RestoreFailureReason::InvalidRequest { .. }
            | RestoreFailureReason::Namespace(_)
            | RestoreFailureReason::TargetUnreadable(_)
            | RestoreFailureReason::Publish(_)
            | RestoreFailureReason::Gate(_)
            | RestoreFailureReason::PublishedTargetMismatch { .. }
            | RestoreFailureReason::Lease(_)
            | RestoreFailureReason::Intent(_)
            | RestoreFailureReason::Store(_) => None,
        }
    }
}

impl fmt::Display for RestoreFailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreFailureReason::InvalidRequest { detail } => write!(f, "请求不成立：{detail}"),
            RestoreFailureReason::Gate(error) => write!(f, "{error}"),
            RestoreFailureReason::EpochChanged { expected, actual } => write!(
                f,
                "Root epoch 已从请求的 {expected} 变为 {actual}，旧恢复作废"
            ),
            RestoreFailureReason::BaseMismatch { detail } => write!(f, "基线过期：{detail}"),
            RestoreFailureReason::SubtreeChanged { detail } => {
                write!(f, "子树已变化：{detail}")
            }
            RestoreFailureReason::VersionGone { detail } => write!(f, "版本不可恢复：{detail}"),
            RestoreFailureReason::UnsupportedEntry { detail } => {
                write!(f, "条目不能承载内容：{detail}")
            }
            RestoreFailureReason::TargetUnreadable(error) => write!(f, "{error}"),
            RestoreFailureReason::Reconcile { expected, observed } => {
                write!(f, "现场与请求指纹不符：期望 {expected}，实测 {observed}")
            }
            RestoreFailureReason::OperationReused {
                op,
                recorded,
                requested,
            } => write!(
                f,
                "同一 OpId {op} 已登记摘要 {}，本次请求摘要 {}",
                recorded.to_hex(),
                requested.to_hex()
            ),
            RestoreFailureReason::OperationPending { op } => {
                write!(f, "OpId {op} 仍是 pending，不能重做")
            }
            RestoreFailureReason::NotInTrash { detail } => {
                write!(f, "回收站里没有可恢复的内容：{detail}")
            }
            RestoreFailureReason::NameUnavailable { detail } => {
                write!(f, "没有可用的恢复名称：{detail}")
            }
            RestoreFailureReason::UnsupportedSubtree { detail } => {
                write!(f, "带后代的目录子树恢复尚未接入：{detail}")
            }
            RestoreFailureReason::Namespace(error) => write!(f, "{error}"),
            RestoreFailureReason::Lease(error) => write!(f, "{error}"),
            RestoreFailureReason::Intent(error) => write!(f, "{error}"),
            RestoreFailureReason::Publish(error) => write!(f, "{error}"),
            RestoreFailureReason::PublishedTargetMismatch { expected, observed } => write!(
                f,
                "发布后实测 {} 字节 / {}，不是目标 {} 字节 / {}",
                observed.identity.bytes, observed.digest, expected.size, expected.digest
            ),
            RestoreFailureReason::Store(error) => write!(f, "{error}"),
            RestoreFailureReason::StoreContract { detail } => {
                write!(f, "恢复事务返回的证据与请求不一致：{detail}")
            }
        }
    }
}

/// 失败路径上的租约清理：解除失败不覆盖主因，只把“仍保护到某时刻”记到失败证据里。
///
/// 租约到期时间本身就是兜底（GC 到期后才按引用标记回收），因此解除失败不能让已经失败的操作再换一个
/// 原因，更不能把已提交的结果改判为失败；但也不能假装解除成功，所以留下 `lease_expires_at`。
pub(crate) async fn release_after_failure<C>(
    leases: &mut C,
    lease: &crate::ports::content_lease::ContentLease,
    mut failure: RestoreFailure,
) -> RestoreFailure
where
    C: crate::ports::content_lease::ContentLeasePort + ?Sized,
{
    if leases.release(lease).await.is_err() {
        failure.lease_expires_at = Some(lease.expires_at);
    }
    failure
}

/// 把 put 状态机的失败改判成恢复失败：意图与发布两个阶段共用同一套错误词汇。
pub(crate) fn reclassify_publish_failure(failure: PutFailure) -> RestoreFailure {
    use RestoreFailureReason as Reason;

    let reason = match failure.reason {
        PutFailureReason::InvalidRequest { detail } => Reason::InvalidRequest { detail },
        PutFailureReason::TargetUnreadable(error) => Reason::TargetUnreadable(error),
        PutFailureReason::BeforeMismatch { expected, observed } => Reason::Reconcile {
            expected: expected.to_json(),
            observed,
        },
        PutFailureReason::Intent(error) => Reason::Intent(error),
        PutFailureReason::Publish(error) => Reason::Publish(error),
        PutFailureReason::PublishedTargetMismatch { expected, observed } => {
            Reason::PublishedTargetMismatch { expected, observed }
        }
        other => Reason::StoreContract {
            detail: format!("发布阶段返回了与恢复无关的失败：{other}"),
        },
    };
    RestoreFailure::new(reason)
}
