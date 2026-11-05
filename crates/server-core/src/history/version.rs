//! 历史版本恢复：按一个旧不可变版本创建**新的**当前版本（design §10.3、I12、§15.2
//! `GET .../versions`）。
//!
//! 顺序与 put 的提交步骤一致（§11.2），差别只在于内容来源与事务内容：
//!
//! 1. **请求形状 → Root 串行化 → epoch**：与 put 同一套 Gate 与 epoch 判定，旧 epoch 的恢复作废；
//! 2. **`OpId` 记录**（I06）：先查该幂等键。已提交 → 原样重放首次的实际结果与路径；已登记但
//!    `pending` → `NEEDS_RECOVERY` 等恢复，不重做；摘要不同 → `OP_ID_REUSED`；
//! 3. **版本事实**：版本必须属于该条目、条目必须是普通文件、revision 仍是请求基线，且版本仍在历史
//!    保留期内；过期返回 `VERSION_GONE`，不用同路径的新内容冒充旧版本；
//! 4. **内容租约**：在“读到旧版本”和“新版本引用同一对象”之间建立短期租约，GC 在这段时间不得回收
//!    该对象（design §8.4、§11、§19.4）；
//! 5. **现场指纹 → 意图 → 受控发布**：工作目录现场必须还是请求指纹，随后复用 [`publish_put`]，
//!    把对象发布成目标路径上的新内容；
//! 6. **单一事务**：追加新版本行（引用**同一个**内容对象）、推进条目 revision/当前内容、事件批次、
//!    操作结果、意图收尾。旧版本行一字不改（I12）；返回前核对事务证据，不一致就停写人工核对。
//!
//! 恢复不是回滚：它产生新的当前版本，历史里仍然能看到被恢复的旧版本本身，客户端按普通事件收敛。

use vaultsyncd_domain::ids::{EntryId, IntentId, OpId, RootId, VersionId};
use vaultsyncd_domain::version::{Revision, RootSeq, ServerEpoch, UtcTimestamp};

use crate::commit::intent::{NewPutIntent, TargetAfter, TargetBefore, publish_put};
use crate::commit::put::{RootCommitGate, target_name, validate_name, validate_target_path};
use crate::history::{
    LeaseDisposition, RestoreFailure, RestoreFailureReason, reclassify_publish_failure,
    release_after_failure,
};
use crate::objects::{ExpectedContent, ObjectLayout};
use crate::ports::conflict_store::{OperationKey, RequestDigest};
use crate::ports::content_lease::{ContentLeasePort, ContentLeasePurpose, ContentLeaseRequest};
use crate::ports::intent_log::IntentLog;
use crate::ports::restore_store::{
    RecordedVersionRestore, RestoreBegin, RestoreStore, RestoreStoreError, VersionRestoreWrite,
};
use crate::ports::target::TargetPort;

/// 恢复期间的内容租约时长（秒）。
///
/// 只覆盖“核对旧版本 → 发布 → 事务提交”这一小段；租约不是长期引用，恢复产生的新版本本身就是之后
/// 的持久引用。取 15 分钟是为了容忍发布慢（大文件跨卷复制）而不至于长期压住 GC。
pub const RESTORE_LEASE_TTL_SECONDS: i64 = 900;

/// 一次历史版本恢复的请求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionRestoreRequest {
    /// 意图身份（由调用方生成，重试必须复用同一个身份）。
    pub intent_id: IntentId,
    /// 幂等键：`OpId` 的业务结果按 `(RootId, DeviceId, OpId)` 唯一（design §7.2、I06）。
    pub op: OperationKey,
    /// 本次请求的规范化摘要，由调用方用存储层的 `operations::request_digest` 算出。
    ///
    /// 必须覆盖本次请求的全部语义字段（条目、基线 revision、被恢复版本、目标路径），否则摘要相同
    /// 的两个不同请求会被当成重试。
    pub digest: RequestDigest,
    /// 被恢复内容的条目。
    pub entry: EntryId,
    /// 要恢复的旧版本；它必须属于 `entry` 且仍在保留期内。
    pub version: VersionId,
    /// 客户端事实基础所属的服务器 epoch。
    pub expected_epoch: ServerEpoch,
    /// 客户端观察到的条目 revision（服务器当前 revision 已前进时不覆盖）。
    pub expected_revision: Revision,
    /// 目标在 Root 内的相对路径（最后一段必须等于条目当前名称）。
    pub target_path: String,
    /// 目标所在子卷内的暂存文件名（单个文件名）。
    pub staging_path: String,
    /// 客户端观察到的现场指纹（发布前必须仍然相符）。
    pub before: TargetBefore,
}

impl VersionRestoreRequest {
    /// 在调用任何端口之前检查请求自身的形状。
    fn validate(&self) -> Result<(), Box<RestoreFailure>> {
        ObjectLayout::validate_staging_name(&self.staging_path)
            .map_err(|error| invalid_request(error.to_string()))?;
        validate_target_path(&self.target_path).map_err(invalid_request)?;
        let name = target_name(&self.target_path)
            .ok_or_else(|| invalid_request("目标路径没有名称分量".to_owned()))?;
        validate_name(name).map_err(invalid_request)?;
        if let TargetBefore::Present(snapshot) = &self.before {
            if snapshot.entry != self.entry {
                return Err(invalid_request(format!(
                    "现场指纹记录的条目 {} 不是请求条目 {}",
                    snapshot.entry, self.entry
                )));
            }
            if snapshot.revision != self.expected_revision.get() {
                return Err(invalid_request(format!(
                    "现场指纹记录的 revision {} 不是请求基线 {}",
                    snapshot.revision, self.expected_revision
                )));
            }
        }
        Ok(())
    }
}

/// 一次历史版本恢复的业务结果（首次提交与重放共用，也是 `operations` 里的终态结果）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedVersionRestore {
    /// 产生该结果的逻辑操作身份。
    pub op: OpId,
    /// 所属 Root。
    pub root: RootId,
    /// 被恢复内容的条目。
    pub entry: EntryId,
    /// 新的当前版本身份（服务器分配，与被恢复的旧版本不同）。
    pub version: VersionId,
    /// 内容来源的旧版本：它的行与字节都不因本次恢复改变（I12）。
    pub restored_from: VersionId,
    /// 恢复后的内容（与新版本同一份对象）。
    pub content: ExpectedContent,
    /// 条目新 revision（严格大于请求基线）。
    pub revision: Revision,
    /// 该批次占用的 RootSeq。
    pub seq: RootSeq,
    /// 目标的 Root 内相对路径。
    pub path: String,
}

/// 一次成功的恢复：业务结果 + 租约的处理方式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredVersion {
    /// 业务结果。
    pub result: CommittedVersionRestore,
    /// 提交之后旧内容租约怎么处理的（解除失败时如实报告到期时间，不改判结果）。
    pub lease: LeaseDisposition,
}

/// 恢复一个历史版本。
///
/// 顺序见模块文档：请求形状 → Root 票据 → epoch → `OpId` 记录 → 版本事实 → 内容租约 → 现场指纹 →
/// 意图 → 发布 → 元数据事务 → 核对证据 → 解除租约 → ACK。无论成功还是失败，退出前都会释放 Root
/// 票据。
///
/// # Errors
///
/// 见 [`RestoreFailure`]：请求不成立、Root 忙、epoch 变化、同一 `OpId` 不同请求、记录仍是 pending、
/// 基线过期、版本不可恢复（`VERSION_GONE`）、条目不是普通文件、现场不符、租约/意图/发布/存储失败、
/// 存储契约不一致。失败都不是 committed，已持久化的意图与 `pending` 记录保留为恢复证据。
pub async fn restore_version<S, L, T, C>(
    gate: &mut RootCommitGate,
    store: &mut S,
    log: &mut L,
    target: &mut T,
    leases: &mut C,
    at: UtcTimestamp,
    request: &VersionRestoreRequest,
) -> Result<RestoredVersion, Box<RestoreFailure>>
where
    S: RestoreStore + ?Sized,
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
    C: ContentLeasePort + ?Sized,
{
    request.validate()?;
    let ticket = gate
        .acquire(request.op.root)
        .map_err(|error| Box::new(RestoreFailure::new(RestoreFailureReason::Gate(error))))?;
    let outcome = restore_under_ticket(store, log, target, leases, at, request).await;
    gate.release(ticket);
    outcome
}

async fn restore_under_ticket<S, L, T, C>(
    store: &mut S,
    log: &mut L,
    target: &mut T,
    leases: &mut C,
    at: UtcTimestamp,
    request: &VersionRestoreRequest,
) -> Result<RestoredVersion, Box<RestoreFailure>>
where
    S: RestoreStore + ?Sized,
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
    C: ContentLeasePort + ?Sized,
{
    // 1. epoch：恢复过的服务器不是同一套事实，旧预期的恢复直接作废。
    let facts = store
        .root_facts(request.op.root)
        .await
        .map_err(store_failure)?;
    if facts.epoch != request.expected_epoch {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::EpochChanged {
                expected: request.expected_epoch,
                actual: facts.epoch,
            },
        )));
    }

    // 2. 幂等记录先于“基线是否过期”的判定：重放自己已提交的结果时基线早已前进。
    if let Some(recorded) = store.operation(request.op).await.map_err(store_failure)? {
        return resolve_recorded(recorded, request);
    }

    // 3. 版本事实：属于该条目、仍在保留期、条目仍是普通文件且基线没前进。
    let version = store
        .version_facts(request.op.root, request.entry, request.version)
        .await
        .map_err(version_failure)?;
    check_version_facts(&version, at, request)?;

    // 4. 先登记 pending：并发的同一 OpId 只有一个能继续，崩溃重发得到的也是 pending。
    match store
        .begin_restore(request.op, request.digest)
        .await
        .map_err(store_failure)?
    {
        RestoreBegin::Started => {}
        RestoreBegin::Recorded => {
            let recorded = store
                .operation(request.op)
                .await
                .map_err(store_failure)?
                .ok_or_else(|| pending(request.op.op))?;
            return resolve_recorded(recorded, request);
        }
    }

    // 5. 内容租约：把“读到旧版本”与“新版本引用同一对象”之间的窗口保护起来。
    let lease = leases
        .acquire(&ContentLeaseRequest {
            root: request.op.root,
            digest: version.content.digest,
            purpose: ContentLeasePurpose::Commit,
            expires_at: lease_expiry(at)?,
        })
        .await
        .map_err(|error| Box::new(RestoreFailure::new(RestoreFailureReason::Lease(error))))?;

    // 6. 现场指纹 → 意图 → 发布：外部写入不是可覆盖的东西（design §12.2）。
    let new = NewPutIntent {
        intent_id: request.intent_id,
        root: request.op.root,
        target_path: request.target_path.clone(),
        staging_path: request.staging_path.clone(),
        before: request.before.clone(),
        after: TargetAfter {
            digest: version.content.digest,
            size: version.content.bytes,
        },
    };
    let published = match publish_put(log, target, at, &new).await {
        Ok(published) => published,
        Err(failure) => {
            let failure = reclassify_publish_failure(*failure);
            return Err(Box::new(
                release_after_failure(leases, std::slice::from_ref(&lease), failure).await,
            ));
        }
    };

    // 7. 单一事务：新版本 + 条目 + 事件批次 + 操作结果 + 意图收尾；旧版本行不动（I12）。
    let write = VersionRestoreWrite {
        intent_id: request.intent_id,
        key: request.op,
        digest: request.digest,
        entry: request.entry,
        expected_revision: request.expected_revision,
        restored_from: request.version,
        content: version.content,
        path: request.target_path.clone(),
        at,
    };
    let record = match store.apply_version_restore(&write).await {
        Ok(record) => record,
        Err(error) => {
            let failure = RestoreFailure::after_intent(
                restore_failure_after_intent(error),
                &published.intent,
            );
            return Err(Box::new(
                release_after_failure(leases, std::slice::from_ref(&lease), failure).await,
            ));
        }
    };
    if let Err(failure) = check_record(&record, request, &version, &published) {
        // 证据不符同样不能留着租约：停写人工核对，但保护也要释放。
        let failure = release_after_failure(leases, std::slice::from_ref(&lease), *failure).await;
        return Err(Box::new(failure));
    }

    Ok(RestoredVersion {
        result: record.result,
        lease: release_lease(leases, &lease).await,
    })
}

/// 版本不可恢复时的映射：不存在/不属于该条目/已过期都归到 `VERSION_GONE`，让客户端重列历史。
fn version_failure(error: RestoreStoreError) -> Box<RestoreFailure> {
    Box::new(RestoreFailure::new(match error {
        RestoreStoreError::VersionMissing { version } => RestoreFailureReason::VersionGone {
            detail: format!("版本 {version} 不可恢复"),
        },
        RestoreStoreError::EntryMissing { entry } => RestoreFailureReason::BaseMismatch {
            detail: format!("条目 {entry} 已不存在"),
        },
        error => RestoreFailureReason::Store(error),
    }))
}

fn store_failure(error: RestoreStoreError) -> Box<RestoreFailure> {
    Box::new(RestoreFailure::new(match error {
        RestoreStoreError::EntryMissing { entry } => RestoreFailureReason::BaseMismatch {
            detail: format!("条目 {entry} 已不存在"),
        },
        RestoreStoreError::UnknownRoot { root } => RestoreFailureReason::BaseMismatch {
            detail: format!("Root {root} 已不存在"),
        },
        error => RestoreFailureReason::Store(error),
    }))
}

fn check_version_facts(
    facts: &crate::ports::restore_store::VersionFacts,
    at: UtcTimestamp,
    request: &VersionRestoreRequest,
) -> Result<(), Box<RestoreFailure>> {
    if facts.entry.entry != request.entry || facts.entry.root != request.op.root {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::BaseMismatch {
                detail: format!(
                    "库里的条目 {}（Root {}）不是请求条目 {}",
                    facts.entry.entry, facts.entry.root, request.entry
                ),
            },
        )));
    }
    if !facts.entry.kind.is_file() {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::UnsupportedEntry {
                detail: format!(
                    "条目 {} 是{}，历史版本恢复只用于普通文件",
                    facts.entry.entry,
                    facts.entry.kind.as_text()
                ),
            },
        )));
    }
    if facts.entry.revision != request.expected_revision {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::BaseMismatch {
                detail: format!(
                    "条目 revision 已是 {}，请求基线是 {}",
                    facts.entry.revision, request.expected_revision
                ),
            },
        )));
    }
    if target_name(&request.target_path) != Some(facts.entry.name.as_str()) {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::SubtreeChanged {
                detail: format!(
                    "条目现在的名称是 {:?}，目标路径 {:?} 的最后一段不是它",
                    facts.entry.name, request.target_path
                ),
            },
        )));
    }
    if facts.retained_until < at {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::VersionGone {
                detail: format!(
                    "版本 {} 的历史保留期已在 {} 结束",
                    request.version, facts.retained_until
                ),
            },
        )));
    }
    Ok(())
}

/// 事务声称成功时核对返回的证据：不一致说明适配器违反了端口契约，必须停写人工核对，不能 ACK。
fn check_record(
    record: &crate::ports::restore_store::VersionRestoreRecord,
    request: &VersionRestoreRequest,
    version: &crate::ports::restore_store::VersionFacts,
    published: &crate::commit::intent::PublishedPut,
) -> Result<(), Box<RestoreFailure>> {
    let problem = if record.intent_id != request.intent_id {
        Some(format!(
            "事务收尾的意图 {} 不是请求意图 {}",
            record.intent_id, request.intent_id
        ))
    } else if !record.phase.is_finished() {
        Some(format!("意图阶段是 {} 而不是 finalized", record.phase))
    } else if record.result.op != request.op.op {
        Some(format!(
            "事务记录的操作 {} 不是请求操作 {}",
            record.result.op, request.op.op
        ))
    } else if record.result.entry != request.entry {
        Some(format!(
            "事务恢复的条目 {} 不是请求条目 {}",
            record.result.entry, request.entry
        ))
    } else if record.result.restored_from != request.version {
        Some(format!(
            "事务记录的来源版本 {} 不是请求版本 {}",
            record.result.restored_from, request.version
        ))
    } else if record.result.version == request.version {
        Some(
            "事务把旧版本本身当成新的当前版本：恢复必须创建新版本，不能改写旧版本（I12）"
                .to_owned(),
        )
    } else if record.result.content != version.content {
        Some(format!(
            "恢复后的内容是 {}（{} 字节），不是旧版本的内容 {}（{} 字节）",
            record.result.content.digest,
            record.result.content.bytes,
            version.content.digest,
            version.content.bytes
        ))
    } else if record.result.revision <= request.expected_revision {
        Some(format!(
            "条目 revision 没有推进：{} 不大于基线 {}",
            record.result.revision, request.expected_revision
        ))
    } else if record.result.path != request.target_path {
        Some(format!(
            "事务记录的路径 {:?} 不是请求路径 {:?}",
            record.result.path, request.target_path
        ))
    } else {
        None
    };
    match problem {
        Some(detail) => Err(Box::new(RestoreFailure::after_intent(
            RestoreFailureReason::StoreContract { detail },
            &published.intent,
        ))),
        None => Ok(()),
    }
}

/// 按已记录的 `OpId` 状态回答：重放原结果、拒绝不同请求，或拒绝重做未完成的记录。
fn resolve_recorded(
    recorded: RecordedVersionRestore,
    request: &VersionRestoreRequest,
) -> Result<RestoredVersion, Box<RestoreFailure>> {
    if recorded.digest != request.digest {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::OperationReused {
                op: request.op.op,
                recorded: recorded.digest,
                requested: request.digest,
            },
        )));
    }
    let Some(result) = recorded.result else {
        return Err(pending(request.op.op));
    };
    if result.entry != request.entry || result.restored_from != request.version {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::StoreContract {
                detail: format!(
                    "同一 OpId {} 记录的是条目 {} 版本 {} 的恢复，本次请求是条目 {} 版本 {}",
                    request.op.op,
                    result.entry,
                    result.restored_from,
                    request.entry,
                    request.version
                ),
            },
        )));
    }
    Ok(RestoredVersion {
        result,
        // 重放不新建租约：首次提交已经产生了新版本这个持久引用。
        lease: LeaseDisposition::Released,
    })
}

fn pending(op: OpId) -> Box<RestoreFailure> {
    Box::new(RestoreFailure::new(
        RestoreFailureReason::OperationPending { op },
    ))
}

fn invalid_request(detail: String) -> Box<RestoreFailure> {
    Box::new(RestoreFailure::new(RestoreFailureReason::InvalidRequest {
        detail,
    }))
}

fn lease_expiry(at: UtcTimestamp) -> Result<UtcTimestamp, Box<RestoreFailure>> {
    let seconds = at.as_unix_seconds().checked_add(RESTORE_LEASE_TTL_SECONDS);
    seconds
        .and_then(|value| UtcTimestamp::from_unix_seconds(value).ok())
        .ok_or_else(|| {
            invalid_request(format!(
                "租约到期时间超出可表示范围（基准 {}）",
                at.as_unix_seconds()
            ))
        })
}

/// 提交成功后解除租约：解除失败如实报告“仍保护到到期时间”，不改判已提交的业务结果。
async fn release_lease<C>(
    leases: &mut C,
    lease: &crate::ports::content_lease::ContentLease,
) -> LeaseDisposition
where
    C: ContentLeasePort + ?Sized,
{
    match leases.release(lease).await {
        Ok(()) => LeaseDisposition::Released,
        Err(_) => LeaseDisposition::Expires {
            at: lease.expires_at,
        },
    }
}

/// 事务失败后的原因映射：把“事务里才发现基线变了”归到重新规划一类，其余保留存储原因。
fn restore_failure_after_intent(error: RestoreStoreError) -> RestoreFailureReason {
    match error {
        RestoreStoreError::RevisionChanged { expected, actual } => {
            RestoreFailureReason::BaseMismatch {
                detail: format!("提交时条目 revision 已从 {expected} 变为 {actual}"),
            }
        }
        RestoreStoreError::EntryMissing { entry } => RestoreFailureReason::BaseMismatch {
            detail: format!("提交时条目 {entry} 已不存在"),
        },
        RestoreStoreError::VersionMissing { version } => RestoreFailureReason::VersionGone {
            detail: format!("提交时版本 {version} 已不可恢复"),
        },
        error => RestoreFailureReason::Store(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use vaultsyncd_domain::error::ErrorCode;

    use crate::commit::intent::PutPhase;
    use crate::ports::content_lease::ContentLeaseError;
    use crate::ports::target::TargetError;

    use crate::ports::put_store::{EntryBaseline, EntryKind};
    use crate::ports::restore_store::VersionFacts;
    use crate::test_support::{
        FakeContentLease, FakeIntentLog, FakeRestoreStore, FakeTarget, Trace, device_id, digest,
        entry_id, epoch, op_id, request_digest, revision, root_id, target_snapshot, user_time,
        version_id,
    };

    /// 当前工作文件的内容（与要被恢复的旧版本不同）。
    const CURRENT_HEX: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    struct Fixture {
        gate: RootCommitGate,
        store: FakeRestoreStore,
        log: FakeIntentLog,
        target: FakeTarget,
        leases: FakeContentLease,
        trace: Trace,
    }

    fn facts() -> VersionFacts {
        VersionFacts {
            entry: EntryBaseline {
                entry: entry_id(0x51),
                root: root_id(0x21),
                parent: entry_id(0x31),
                name: "note.txt".to_owned(),
                revision: revision(7),
                kind: EntryKind::File,
            },
            content: ExpectedContent::new(digest(0x22), 5),
            retained_until: user_time(2_000_000),
        }
    }

    fn request() -> VersionRestoreRequest {
        VersionRestoreRequest {
            intent_id: crate::test_support::intent_id(0x41),
            op: OperationKey {
                root: root_id(0x21),
                device: device_id(0x61),
                op: op_id(0x71),
            },
            digest: request_digest(0x01),
            entry: entry_id(0x51),
            version: version_id(0x81),
            expected_epoch: epoch(3),
            expected_revision: revision(7),
            target_path: "docs/note.txt".to_owned(),
            staging_path: ".staging-note".to_owned(),
            before: TargetBefore::Present(target_snapshot(CURRENT_HEX, 6)),
        }
    }

    fn fixture() -> Fixture {
        let trace = Trace::default();
        let snapshot = target_snapshot(CURRENT_HEX, 6);
        let mut store = FakeRestoreStore::new(facts());
        store.trace = trace.clone();
        Fixture {
            gate: RootCommitGate::new(),
            store,
            log: FakeIntentLog {
                trace: trace.clone(),
                ..FakeIntentLog::default()
            },
            target: FakeTarget::present(&snapshot).with_trace(trace.clone()),
            leases: FakeContentLease::new().with_trace(trace.clone()),
            trace,
        }
    }

    async fn run(
        fixture: &mut Fixture,
        request: &VersionRestoreRequest,
    ) -> Result<RestoredVersion, Box<RestoreFailure>> {
        restore_version(
            &mut fixture.gate,
            &mut fixture.store,
            &mut fixture.log,
            &mut fixture.target,
            &mut fixture.leases,
            user_time(1_000),
            request,
        )
        .await
    }

    #[tokio::test]
    async fn restore_creates_a_new_current_version_from_the_old_object() {
        let mut fixture = fixture();
        let request = request();
        let restored = run(&mut fixture, &request).await.expect("恢复应当成功");

        // 新版本不是旧版本本身：恢复创建新的当前版本，而不是改写旧版本（I12）。
        assert_ne!(restored.result.version, request.version);
        assert_eq!(restored.result.restored_from, request.version);
        assert_eq!(
            restored.result.content,
            ExpectedContent::new(digest(0x22), 5)
        );
        assert_eq!(restored.result.revision, revision(8));
        assert_eq!(restored.result.path, "docs/note.txt");
        assert_eq!(restored.lease, LeaseDisposition::Released);

        // 事务输入：来源版本与内容都是旧版本的同一份对象，没有第二份内容。
        let write = &fixture.store.writes[0];
        assert_eq!(write.restored_from, request.version);
        assert_eq!(write.content, ExpectedContent::new(digest(0x22), 5));

        // 内容租约保护的是旧版本对象，提交后解除。
        assert_eq!(fixture.leases.acquired.len(), 1);
        assert_eq!(fixture.leases.acquired[0].digest, digest(0x22));
        assert_eq!(
            fixture.leases.acquired[0].purpose,
            ContentLeasePurpose::Commit
        );
        assert_eq!(fixture.leases.acquired[0].expires_at, user_time(1_900));
        assert_eq!(fixture.leases.released.len(), 1);

        // 顺序：意图 → 发布 → 事务；租约在发布之前。
        let trace = fixture.trace.entries();
        let position = |needle: &str| {
            trace
                .iter()
                .position(|line| line == needle)
                .unwrap_or_else(|| panic!("轨迹里没有 {needle}：{trace:?}"))
        };
        assert!(position("lease acquire") < position("intent begin prepared"));
        assert!(position("intent begin prepared") < position("target publish docs/note.txt"));
        assert!(position("target publish docs/note.txt") < position("store apply_version_restore"));
        assert!(position("store apply_version_restore") < position("lease release"));
    }

    #[tokio::test]
    async fn repeated_op_id_replays_the_first_result() {
        let mut fixture = fixture();
        let request = request();
        let first = run(&mut fixture, &request).await.expect("首次恢复成功");
        let second = run(&mut fixture, &request).await.expect("重放应当成功");

        assert_eq!(first.result, second.result);
        assert_eq!(fixture.store.writes.len(), 1, "重放不得再做一次事务");
        assert_eq!(fixture.leases.acquired.len(), 1, "重放不得再建租约");
        assert_eq!(second.lease, LeaseDisposition::Released);
    }

    #[tokio::test]
    async fn same_op_id_with_another_request_is_rejected() {
        let mut fixture = fixture();
        let request = request();
        run(&mut fixture, &request).await.expect("首次恢复成功");
        let mut other = request.clone();
        other.digest = request_digest(0x02);
        let failure = run(&mut fixture, &other)
            .await
            .expect_err("不同请求必须被拒");
        assert_eq!(failure.code(), "op_id_reused");
        assert_eq!(failure.error_code(), Some(ErrorCode::OpIdReused));
        assert!(!failure.is_committed());
    }

    #[tokio::test]
    async fn pending_record_is_not_redone() {
        let mut fixture = fixture();
        let request = request();
        fixture.store.recorded.push((
            request.op,
            RecordedVersionRestore {
                digest: request.digest,
                result: None,
            },
        ));
        let failure = run(&mut fixture, &request)
            .await
            .expect_err("pending 不是结果");
        assert_eq!(failure.code(), "operation_pending");
        assert_eq!(failure.error_code(), Some(ErrorCode::NeedsRecovery));
        assert!(fixture.store.writes.is_empty());
    }

    #[tokio::test]
    async fn expired_history_reports_version_gone() {
        let mut fixture = fixture();
        let mut old = facts();
        old.retained_until = user_time(900);
        fixture.store.version = Some(old);
        let failure = run(&mut fixture, &request())
            .await
            .expect_err("过期历史不可恢复");
        assert_eq!(failure.code(), "version_gone");
        assert_eq!(failure.error_code(), Some(ErrorCode::VersionGone));
        assert!(fixture.store.writes.is_empty());
        assert!(fixture.leases.acquired.is_empty());
    }

    #[tokio::test]
    async fn directory_entry_cannot_restore_content() {
        let mut fixture = fixture();
        let mut directory = facts();
        directory.entry.kind = EntryKind::Directory;
        fixture.store.version = Some(directory);
        let failure = run(&mut fixture, &request())
            .await
            .expect_err("目录没有内容版本");
        assert_eq!(failure.code(), "unsupported_entry");
        assert_eq!(failure.error_code(), Some(ErrorCode::UnsupportedEntry));
    }

    #[tokio::test]
    async fn stale_revision_is_a_base_mismatch() {
        let mut fixture = fixture();
        let mut advanced = facts();
        // 服务器上的条目已经前进到 revision 8，而请求还按 7 计划。
        advanced.entry.revision = revision(8);
        fixture.store.version = Some(advanced);
        let failure = run(&mut fixture, &request())
            .await
            .expect_err("过期基线不能恢复");
        assert_eq!(failure.code(), "base_mismatch");
        assert_eq!(failure.error_code(), Some(ErrorCode::BaseMismatch));
    }

    #[tokio::test]
    async fn changed_work_file_needs_reconcile() {
        let mut fixture = fixture();
        fixture.target = FakeTarget::absent().with_trace(fixture.trace.clone());
        let failure = run(&mut fixture, &request())
            .await
            .expect_err("现场不符不能发布");
        assert_eq!(failure.code(), "reconcile_required");
        assert_eq!(failure.error_code(), Some(ErrorCode::ReconcileRequired));
        assert!(fixture.store.writes.is_empty(), "现场不符不得写意图");
    }

    #[tokio::test]
    async fn store_contract_violation_stops_the_ack() {
        let mut fixture = fixture();
        let request = request();
        let mut record = FakeRestoreStore::default_record(&VersionRestoreWrite {
            intent_id: request.intent_id,
            key: request.op,
            digest: request.digest,
            entry: request.entry,
            expected_revision: request.expected_revision,
            restored_from: request.version,
            content: facts().content,
            path: request.target_path.clone(),
            at: user_time(1_000),
        });
        // 适配器把旧版本本身当成新的当前版本：违反 I12，必须停写而不是 ACK。
        record.result.version = request.version;
        fixture.store.record = Some(record);
        let failure = run(&mut fixture, &request)
            .await
            .expect_err("证据不一致不得 ACK");
        assert_eq!(failure.code(), "store_contract_violation");
        assert_eq!(failure.error_code(), Some(ErrorCode::NeedsRecovery));
        assert!(!failure.is_committed());
        assert_eq!(failure.phase, Some(PutPhase::Published));
    }

    #[tokio::test]
    async fn lease_failure_stops_before_any_intent() {
        let mut fixture = fixture();
        fixture.leases.acquire_error = Some(ContentLeaseError::ObjectNotRegistered {
            digest: digest(0x22),
        });
        let failure = run(&mut fixture, &request())
            .await
            .expect_err("没有保护就不发布");
        assert_eq!(failure.code(), "object_not_registered");
        assert!(fixture.store.writes.is_empty());
        assert!(fixture.log.intents.is_empty());
    }

    #[tokio::test]
    async fn publish_failure_releases_the_lease() {
        let mut fixture = fixture();
        fixture.target.publish_error = Some(TargetError::Storage {
            stage: "rename",
            failure: crate::ports::object_store::StorageFailure::Other,
            detail: "模拟替换失败".to_owned(),
        });
        let failure = run(&mut fixture, &request()).await.expect_err("发布失败");
        assert!(!failure.is_committed());
        assert_eq!(failure.code(), "other");
        assert_eq!(fixture.leases.released.len(), 1);
        assert!(fixture.store.writes.is_empty());
    }
}
