//! 外部内容的采集与采纳（design §12.1）：稳定指纹 → 私有候选 → 候选校验 → 采集前后核对 →
//! `ExternalProposal` → 同一提交事件管线。
//!
//! 重要的诚实边界（design §12.2、C33）：
//!
//! - 整文件克隆让候选字节相对并发写原子，但它**可能恰好捕获应用多次写入之间的状态**，不是“应用
//!   已保存完成”的证据；采集前后的指纹核对只降低风险，不消除外部写入的竞态。
//! - 两次观察之间被改写又改回、或在服务停止期间被连续覆盖的中间内容，从未被观察，不会有版本。
//!   本模块不声称保留它们。
//! - 候选对象一旦发布到内容地址就是不可变的；若随后发现源已变化而放弃采纳，对象只是无引用的
//!   内容地址文件，由对象 GC 回收，不会成为任何版本。

use std::fmt;
use std::path::Path;

use vaultsyncd_domain::ids::{EntryId, VaultId};
use vaultsyncd_domain::version::{Revision, UtcTimestamp};

use crate::commit::put::VaultCommitGate;
use crate::objects::{
    CaptureFailure, CaptureFailureReason, CapturedObject, ExpectedContent, ObjectLayout,
    capture_observed_object,
};
use crate::ports::external_store::{
    AdoptKind, ExternalAdoptStore, ExternalRecord, ExternalStoreError, ExternalWrite,
};
use crate::ports::object_store::ObjectBlobPort;
use crate::ports::put_store::EntryKind;
use crate::ports::work_tree::{
    FileStamp, WorkEntryKind, WorkObservation, WorkTreeError, WorkTreePort,
};

/// 为什么要重新排队（尚未采集，也不得标为已保护）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequeueReason {
    /// 采集开始前，现场指纹已不是稳定判断时的指纹。
    ChangedBeforeCapture {
        /// 变化的字段。
        fields: Vec<&'static str>,
    },
    /// 候选建立期间源在增长/被改写（候选长度与源长度不符）。
    SourceChangedDuringCopy,
    /// 采集之后指纹与采集之前不同：候选可能不是当前内容。
    ChangedDuringCapture {
        /// 变化的字段。
        fields: Vec<&'static str>,
    },
    /// 采纳之前现场指纹已不同于候选对应的指纹。
    ChangedBeforeCommit {
        /// 变化的字段。
        fields: Vec<&'static str>,
    },
    /// 事务内发现条目已被他人推进（比较并写失败）：基于新状态重新规划。
    BaselineMoved,
}

impl RequeueReason {
    /// 稳定短名。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            RequeueReason::ChangedBeforeCapture { .. } => "changed_before_capture",
            RequeueReason::SourceChangedDuringCopy => "source_changed_during_copy",
            RequeueReason::ChangedDuringCapture { .. } => "changed_during_capture",
            RequeueReason::ChangedBeforeCommit { .. } => "changed_before_commit",
            RequeueReason::BaselineMoved => "baseline_moved",
        }
    }
}

impl fmt::Display for RequeueReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequeueReason::ChangedBeforeCapture { fields }
            | RequeueReason::ChangedDuringCapture { fields }
            | RequeueReason::ChangedBeforeCommit { fields } => {
                write!(f, "{}（变化字段：{}）", self.code(), fields.join(","))
            }
            _ => f.write_str(self.code()),
        }
    }
}

/// 一份已固定成不可变对象的外部内容提案（design §12.1 的 ExternalProposal）。
///
/// 持有它**不等于**已提交：只有 [`adopt_external`] 返回 [`AdoptOutcome::Adopted`] 才算有了版本。
#[derive(Clone, Debug)]
pub struct ExternalProposal {
    /// 所属 Vault。
    pub vault: VaultId,
    /// 文件在 Vault 内的相对路径。
    pub path: String,
    /// 候选内容的长度与摘要。
    pub content: ExpectedContent,
    /// 已发布（或复用）的不可变对象。
    pub object: CapturedObject,
    /// 采集前后核对一致的指纹：采纳前再次与现场比对。
    pub stamp: FileStamp,
}

/// 采集失败（不含“需要重排队”这类正常结果）。
#[derive(Clone, Debug)]
pub enum ExternalCaptureError {
    /// 观察工作目录失败（含 Vault 不可用）：不得解释成删除。
    Work(WorkTreeError),
    /// 候选建立或校验失败（源不可读、`ENOSPC`、校验失败等）。
    Capture(CaptureFailure),
}

impl ExternalCaptureError {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ExternalCaptureError::Work(error) => error.code(),
            ExternalCaptureError::Capture(failure) => failure.code(),
        }
    }

    /// 是否 Vault 不可用（应停止整个 Vault 的本轮工作）。
    #[must_use]
    pub fn is_vault_unavailable(&self) -> bool {
        matches!(
            self,
            ExternalCaptureError::Work(WorkTreeError::VaultUnavailable { .. })
        )
    }
}

impl fmt::Display for ExternalCaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExternalCaptureError::Work(error) => write!(f, "{error}"),
            ExternalCaptureError::Capture(failure) => write!(f, "{failure}"),
        }
    }
}

impl std::error::Error for ExternalCaptureError {}

/// 一次采集的结果。
#[derive(Debug)]
pub enum CaptureOutcome {
    /// 采集成功：前后指纹一致，候选已固定成对象。
    Proposal(Box<ExternalProposal>),
    /// 需要重新排队：没有产生可采纳的提案。
    Requeue(RequeueReason),
    /// 路径已不存在：它从未被采纳，没有历史可保护，也没有可传播的删除。
    Vanished,
    /// 路径现在不是普通文件（被替换成目录/符号链接等）。
    NotRegular(WorkEntryKind),
}

/// 对一个已稳定的文件做采集。
///
/// 顺序：核对现场仍是稳定时的指纹 → 私有候选（优先 reflink，跨卷校验复制）并读回校验 → 再次观察
/// 并要求与采集前**完全相同**的指纹。任何一步不满足都不产生提案。
///
/// # Errors
///
/// 见 [`ExternalCaptureError`]：观察失败与候选建立/校验失败。读不到的文件不会被当成不存在。
pub fn capture_external(
    work: &mut dyn WorkTreePort,
    blobs: &mut dyn ObjectBlobPort,
    layout: &ObjectLayout,
    vault: VaultId,
    path: &str,
    stable: &FileStamp,
    staging_name: &str,
) -> Result<CaptureOutcome, ExternalCaptureError> {
    let before = match work.observe(path).map_err(ExternalCaptureError::Work)? {
        WorkObservation::Absent => return Ok(CaptureOutcome::Vanished),
        WorkObservation::NotRegular(kind) => return Ok(CaptureOutcome::NotRegular(kind)),
        WorkObservation::File(stamp) => stamp,
    };
    let drift = stable.changed_fields(&before);
    if !drift.is_empty() {
        return Ok(CaptureOutcome::Requeue(
            RequeueReason::ChangedBeforeCapture { fields: drift },
        ));
    }

    let object = match capture_observed_object(blobs, layout, Path::new(path), staging_name) {
        Ok(object) => object,
        Err(failure) if matches!(failure.reason, CaptureFailureReason::LengthMismatch { .. }) => {
            return Ok(CaptureOutcome::Requeue(
                RequeueReason::SourceChangedDuringCopy,
            ));
        }
        Err(failure) => return Err(ExternalCaptureError::Capture(failure)),
    };

    let after = match work.observe(path).map_err(ExternalCaptureError::Work)? {
        WorkObservation::File(stamp) => stamp,
        // 采集期间被删除/替换：候选仍是某个时刻的真实内容，但没有当前路径与之对应。
        WorkObservation::Absent => return Ok(CaptureOutcome::Vanished),
        WorkObservation::NotRegular(kind) => return Ok(CaptureOutcome::NotRegular(kind)),
    };
    let drift = before.changed_fields(&after);
    if !drift.is_empty() {
        return Ok(CaptureOutcome::Requeue(
            RequeueReason::ChangedDuringCapture { fields: drift },
        ));
    }

    Ok(CaptureOutcome::Proposal(Box::new(ExternalProposal {
        vault,
        path: path.to_owned(),
        content: ExpectedContent::new(object.digest, object.bytes),
        object,
        stamp: after,
    })))
}

/// 采纳被阻止的原因：需要人工协调，不覆盖、不改写。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockReason {
    /// 库里该路径是目录（或父路径是文件）等：类型冲突。
    PathConflict {
        /// 原因文本。
        detail: String,
    },
}

/// 一次采纳的结果。
#[derive(Debug)]
pub enum AdoptOutcome {
    /// 已提交：新版本与事件批次在同一事务里落库。
    Adopted {
        /// 事务证据。
        record: ExternalRecord,
        /// 是否新建条目（否则为已有条目的新版本）。
        created: bool,
    },
    /// 库里该路径的当前版本内容与候选相同：没有新版本（保留期内的 `mtime` 变化不产生版本）。
    UnchangedContent {
        /// 已知条目当前的内容。
        content: ExpectedContent,
        /// 已知条目身份（写入采纳基线，供外部移动/删除核对）。
        entry: EntryId,
        /// 已知条目当前 revision。
        revision: Revision,
    },
    /// 需要重新排队。
    Requeue(RequeueReason),
    /// 路径在采纳前消失：从未被采纳，无需传播。
    Vanished,
    /// 路径在采纳前变成了非普通文件。
    NotRegular(WorkEntryKind),
    /// 同一 Vault 正在提交别的内容：下一轮再试，不排队、不阻塞。
    VaultBusy,
    /// 被阻止，需要人工协调。
    Blocked(BlockReason),
}

/// 采纳失败。
#[derive(Clone, Debug)]
pub enum AdoptFailure {
    /// 观察工作目录失败（含 Vault 不可用）。
    Work(WorkTreeError),
    /// 存储层失败：没有提交。
    Store(ExternalStoreError),
    /// 存储端口声称成功但证据与请求不符：按契约违反处理，不当成已采纳。
    StoreContract {
        /// 原因文本。
        detail: String,
    },
}

impl AdoptFailure {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            AdoptFailure::Work(error) => error.code(),
            AdoptFailure::Store(error) => error.code(),
            AdoptFailure::StoreContract { .. } => "store_contract_violation",
        }
    }
}

impl fmt::Display for AdoptFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdoptFailure::Work(error) => write!(f, "{error}"),
            AdoptFailure::Store(error) => write!(f, "{error}"),
            AdoptFailure::StoreContract { detail } => write!(f, "采纳证据与请求不符：{detail}"),
        }
    }
}

impl std::error::Error for AdoptFailure {}

/// 通过与 put 相同的 Vault 内串行化与元数据事务采纳一份提案。
///
/// 顺序：取 Vault 票据（忙则 [`AdoptOutcome::VaultBusy`]）→ 再次观察现场并要求指纹仍等于提案指纹 →
/// 按路径查已知条目（内容相同则不产生版本）→ 一个事务写对象登记/版本/revision/事件批次 →
/// 核对证据。**不写工作目录**：内容已经在那里，服务端不会（也不得）把它再发布一次。无论结果如何，
/// 退出前都释放 Vault 票据。
///
/// 事务提交之后现场仍可能又被改写；这没有被阻止，也不被宣称阻止：新内容会在下一轮按同样流程成为
/// 下一个版本。
///
/// # Errors
///
/// 见 [`AdoptFailure`]：观察失败、存储失败、存储证据与请求不符。失败时没有提交。
pub async fn adopt_external<S: ExternalAdoptStore>(
    gate: &mut VaultCommitGate,
    store: &mut S,
    work: &mut dyn WorkTreePort,
    proposal: &ExternalProposal,
    at: UtcTimestamp,
) -> Result<AdoptOutcome, AdoptFailure> {
    let Ok(ticket) = gate.acquire(proposal.vault) else {
        return Ok(AdoptOutcome::VaultBusy);
    };
    let result = adopt_locked(store, work, proposal, at).await;
    gate.release(ticket);
    result
}

async fn adopt_locked<S: ExternalAdoptStore>(
    store: &mut S,
    work: &mut dyn WorkTreePort,
    proposal: &ExternalProposal,
    at: UtcTimestamp,
) -> Result<AdoptOutcome, AdoptFailure> {
    match work.observe(&proposal.path).map_err(AdoptFailure::Work)? {
        WorkObservation::Absent => return Ok(AdoptOutcome::Vanished),
        WorkObservation::NotRegular(kind) => return Ok(AdoptOutcome::NotRegular(kind)),
        WorkObservation::File(now) => {
            let drift = proposal.stamp.changed_fields(&now);
            if !drift.is_empty() {
                return Ok(AdoptOutcome::Requeue(RequeueReason::ChangedBeforeCommit {
                    fields: drift,
                }));
            }
        }
    }

    let known = store
        .lookup_path(proposal.vault, &proposal.path)
        .await
        .map_err(AdoptFailure::Store)?;
    let kind = match known {
        None => AdoptKind::Create,
        Some(entry) => {
            if entry.kind != EntryKind::File {
                return Ok(AdoptOutcome::Blocked(BlockReason::PathConflict {
                    detail: format!(
                        "{} 在库里是{}，现场是普通文件",
                        proposal.path,
                        entry.kind.as_text()
                    ),
                }));
            }
            if entry.content == Some(proposal.content) {
                return Ok(AdoptOutcome::UnchangedContent {
                    content: proposal.content,
                    entry: entry.entry,
                    revision: entry.revision,
                });
            }
            AdoptKind::Update {
                entry: entry.entry,
                expected_revision: entry.revision,
            }
        }
    };

    let write = ExternalWrite {
        vault: proposal.vault,
        path: proposal.path.clone(),
        kind,
        content: proposal.content,
        storage_path: proposal.object.storage_path.to_string_lossy().into_owned(),
        at,
    };
    let record = match store.apply_external(&write).await {
        Ok(record) => record,
        Err(ExternalStoreError::RevisionChanged { .. }) => {
            return Ok(AdoptOutcome::Requeue(RequeueReason::BaselineMoved));
        }
        Err(ExternalStoreError::PathConflict { detail }) => {
            return Ok(AdoptOutcome::Blocked(BlockReason::PathConflict { detail }));
        }
        Err(error) => return Err(AdoptFailure::Store(error)),
    };
    verify_record(&write, &record)?;
    Ok(AdoptOutcome::Adopted {
        record,
        created: matches!(write.kind, AdoptKind::Create),
    })
}

fn verify_record(write: &ExternalWrite, record: &ExternalRecord) -> Result<(), AdoptFailure> {
    let violation = |detail: String| Err(AdoptFailure::StoreContract { detail });
    if record.digest != write.content.digest || record.size != write.content.bytes {
        return violation(format!(
            "事务证据内容为 {}（{} 字节），请求为 {}（{} 字节）",
            record.digest, record.size, write.content.digest, write.content.bytes
        ));
    }
    if let AdoptKind::Update {
        entry,
        expected_revision,
    } = write.kind
    {
        if record.entry != entry {
            return violation(format!(
                "事务证据条目 {} 不是请求条目 {entry}",
                record.entry
            ));
        }
        if record.revision <= expected_revision {
            return violation(format!(
                "revision 没有推进：请求基线 {expected_revision}，证据 {}",
                record.revision
            ));
        }
    }
    Ok(())
}
