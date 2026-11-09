//! 冲突内容提交：过期基线上的不同内容成为**一个**冲突条目与一条映射（design §10.1—10.2、T02/T03、
//! I01、I06）。
//!
//! 场景：客户端拿着过期基线（`expected_revision`）提交与服务器 head 不同的内容。按 §10.1 第一行，
//! head 保留主路径，迟到的本地内容提交为新的冲突条目；同一 `OpId` 的重试（响应丢失、崩溃重发）必须
//! 返回原结果与同一个副本身份，不得再造第二份（§10.1 末行、T03）。
//!
//! 顺序即不变量：
//!
//! 1. **请求形状**：冲突路径必须是 Vault 内相对路径，暂存名只能是单个文件名（I10）；这些在任何端口
//!    调用之前检查。
//! 2. **Vault 串行化**：复用 [`VaultCommitGate`]，同一 Vault 的冲突提交与普通 put 不会并发改同一棵树。
//! 3. **epoch 先于重放**：恢复后的服务器不是同一套事实，旧 epoch 的请求不自动重放（design §9.4）。
//! 4. **幂等记录先于“过期/不同内容”判断**：重放自己已提交的结果时，head 一定已经前进且内容不同，
//!    所以先查 `OpId` 记录，再判定本次请求是不是“过期 + 不同内容”，否则重试会被误判成新冲突。
//! 5. **过期且不同内容**：head revision 仍是请求基线（没有过期）→ `conflict_not_stale`；head 内容
//!    已经等于请求内容（同结果）→ `content_already_committed`（§10.1 第二行：不额外造副本）。两者都
//!    映射 `RECONCILE_REQUIRED`，让客户端重新拉事实再规划。
//! 6. **目标是空缺的新条目**：冲突路径上已有东西就拒绝（`RECONCILE_REQUIRED`）重新规划另名，
//!    且这一步在登记 `pending` 之前，不让一个不可能完成的请求锁住该 `OpId`。
//! 7. **先登记 pending 再动磁盘**：`begin_conflict` 原子写入 `OpId → pending`，`pending` 不是
//!    committed；重试看到 `pending` 时不重做（`operation_pending` / `NEEDS_RECOVERY`），把收尾交给
//!    恢复流程。
//! 8. **同一套受恢复保护的提交**：冲突条目也走 [`publish_put`]（意图 → 发布 → `published`）与一个
//!    元数据事务（条目/版本/冲突映射/事件批次/操作结果/意图收尾）。中途失败与普通 put 一样保留
//!    `prepared`/`published` 意图作为恢复证据，绝不 ACK（I01）。
//! 9. **证据不一致就停写**：事务声称成功但返回的副本身份、路径、内容或意图阶段与请求不符，按
//!    `store_contract_violation`（`NEEDS_RECOVERY`）处理，不返回 ACK。
//!
//! 冲突名称（§10.2）只由 `planner::conflict::conflict_entry_path` 一处生成，本模块接收它给出的
//! `target_path` 并校验形状；身份的稳定性来自 `OpId` 记录，不来自名称。

use vaultsyncd_domain::ids::{EntryId, IdSource, IntentId, OpId};
use vaultsyncd_domain::version::{Revision, ServerEpoch, UtcTimestamp, VaultSeq};

use crate::commit::intent::{
    NewPutIntent, PutFailure, PutFailureReason, TargetAfter, TargetBefore, publish_put,
};
use crate::commit::put::{
    VaultCommitGate, base_mismatch, invalid_request, reclassify_publish_failure, validate_name,
    validate_target_path,
};
use crate::objects::{ExpectedContent, ObjectLayout};
use crate::ports::conflict_store::{
    ConflictBegin, ConflictCommitStore, ConflictHead, ConflictRecord, ConflictStoreError,
    ConflictWrite, OperationKey, RecordedOperation, RequestDigest,
};
use crate::ports::intent_log::IntentLog;
use crate::ports::target::TargetPort;

/// 一次冲突内容提交的请求：过期基线 + 迟到的本地内容 + 冲突条目的路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRequest {
    /// 意图身份（由调用方生成，重试必须复用同一个身份）。
    pub intent_id: IntentId,
    /// 幂等键：`OpId` 的业务结果按 `(VaultId, DeviceId, OpId)` 唯一（design §7.2、I06）。
    pub op: OperationKey,
    /// 本次请求的规范化摘要，由调用方用存储层的 `operations::request_digest` 算出。
    ///
    /// 同一 `OpId` 携带不同摘要即“同一 `OpId` 不同请求”，返回 `OP_ID_REUSED`；
    /// 这里必须包含本次请求的全部语义字段（源条目、基线 revision、冲突路径、内容摘要与长度），
    /// 否则摘要相同的两个不同请求会被当成重试。
    pub digest: RequestDigest,
    /// 客户端想覆盖的服务器 head 条目。
    pub entry: EntryId,
    /// 客户端事实基础所属的服务器 epoch。
    pub expected_epoch: ServerEpoch,
    /// 客户端计划时的条目 revision（过期时 head 已前进）。
    pub expected_revision: Revision,
    /// 冲突条目在 Vault 内的相对路径（§10.2 名称由 `planner` 生成，重试不改名）。
    pub target_path: String,
    /// 目标所在子卷内的暂存文件名（单个文件名）。
    pub staging_path: String,
    /// 迟到分支的内容（对象必须已持久化）。
    pub content: ExpectedContent,
}

impl ConflictRequest {
    /// 在调用任何端口之前检查请求自身的形状。
    fn validate(&self) -> Result<(), Box<PutFailure>> {
        ObjectLayout::validate_staging_name(&self.staging_path)
            .map_err(|error| invalid_request(error.to_string()))?;
        validate_target_path(&self.target_path).map_err(invalid_request)?;
        let last = self.target_path.rsplit('/').next().unwrap_or_default();
        validate_name(last).map_err(invalid_request)?;
        Ok(())
    }
}

/// 一次冲突提交的业务结果（首次提交与重放共用）。
///
/// 它同时是 `operations` 里该 `OpId` 的终态结果：重试必须返回同一 `sibling`、同一 `path` 与同一
/// 内容，因此字段只增不改。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedConflict {
    /// 产生该结果的逻辑操作身份。
    pub op: OpId,
    /// 服务器 head 保留的源条目。
    pub source: EntryId,
    /// 迟到内容的冲突条目身份（服务器为该 `OpId` 首次分配）。
    pub sibling: EntryId,
    /// 冲突条目在 Vault 内的路径（首次提交时生成）。
    pub path: String,
    /// 冲突条目的内容（按摘要可查）。
    pub content: ExpectedContent,
    /// 冲突条目的 revision（服务器分配）。
    pub revision: Revision,
    /// 该批次占用的 VaultSeq。
    pub seq: VaultSeq,
}

/// 提交一次冲突内容。
///
/// 顺序见模块文档：请求形状 → Vault 票据 → epoch → `OpId` 记录（重放/拒绝）→ 源条目现状与
/// “过期 + 不同内容”判定 → 目标空缺预检 → 登记 pending → 意图 → 发布 → 冲突事务（条目/版本/映射/
/// 结果/收尾）→ 核对证据 → ACK。无论成功还是失败，退出前都会释放 Vault 票据。
///
/// # Errors
///
/// 见 [`PutFailure`]：请求不成立、Vault 忙、epoch 变化、同一 `OpId` 不同请求（`OP_ID_REUSED`）、
/// 记录仍是 pending（`NEEDS_RECOVERY`）、基线未过期或内容已提交（`RECONCILE_REQUIRED`）、源条目
/// 已消失、意图/发布/存储失败、存储契约不一致。失败都不是 committed，已持久化的意图行与 `pending`
/// 记录保留为恢复证据。
pub async fn commit_conflict<S, L, T, I>(
    gate: &mut VaultCommitGate,
    store: &mut S,
    log: &mut L,
    target: &mut T,
    ids: &mut I,
    at: UtcTimestamp,
    request: &ConflictRequest,
) -> Result<CommittedConflict, Box<PutFailure>>
where
    S: ConflictCommitStore + ?Sized,
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
    I: IdSource,
{
    request.validate()?;
    let ticket = gate
        .acquire(request.op.vault)
        .map_err(|error| Box::new(PutFailure::new(PutFailureReason::Gate(error))))?;
    let outcome = commit_under_ticket(store, log, target, ids, at, request).await;
    gate.release(ticket);
    outcome
}

async fn commit_under_ticket<S, L, T, I>(
    store: &mut S,
    log: &mut L,
    target: &mut T,
    ids: &mut I,
    at: UtcTimestamp,
    request: &ConflictRequest,
) -> Result<CommittedConflict, Box<PutFailure>>
where
    S: ConflictCommitStore + ?Sized,
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
    I: IdSource,
{
    // 1. 服务器事实：epoch 与源条目现状。epoch 先于重放（design §9.4：旧 epoch 不自动重放）。
    let head = store
        .conflict_head(request.op.vault, request.entry)
        .await
        .map_err(conflict_store_failure)?;
    check_epoch(&head, request)?;

    // 2. 幂等记录先于“过期/不同内容”判定：重放自己已提交的结果时 head 早已不是请求基线。
    if let Some(recorded) = store
        .operation(request.op)
        .await
        .map_err(conflict_store_failure)?
    {
        return resolve_recorded(recorded, request);
    }

    // 3. 源条目现状：不在了就不再是“迟到内容”而是一条过期提交，交给客户端重新规划。
    let Some(source) = head.source.as_ref() else {
        return Err(base_mismatch(format!("源条目 {} 已不存在", request.entry)));
    };

    // 4. 必须“过期 + 不同内容”才是冲突副本（§10.1 第一、二行）。
    if source.revision == request.expected_revision {
        return Err(Box::new(PutFailure::new(
            PutFailureReason::ConflictNotStale {
                expected: request.expected_revision,
                actual: source.revision,
            },
        )));
    }
    if !source.kind.is_file() {
        return Err(Box::new(PutFailure::new(
            PutFailureReason::UnsupportedEntry {
                detail: format!(
                    "源条目 {} 是{}，不能承载内容",
                    source.entry,
                    source.kind.as_text()
                ),
            },
        )));
    }
    let Some(head_digest) = source.content else {
        return Err(Box::new(PutFailure::new(PutFailureReason::StoreContract {
            detail: format!("普通文件 {} 没有当前内容版本", source.entry),
        })));
    };
    if head_digest == request.content.digest {
        return Err(Box::new(PutFailure::new(
            PutFailureReason::ContentAlreadyCommitted {
                digest: head_digest,
            },
        )));
    }

    // 5. 冲突条目是新条目：目标必须空缺（§10.1：不覆盖已存在目标）。先自己观察一次，让“路径被
    //    占用”这类可重新规划的情况在登记 pending 之前就被拒绕，不把 OpId 锁在一个不可能完成的请求上。
    let observed = target
        .observe(&request.target_path)
        .map_err(|error| Box::new(PutFailure::new(PutFailureReason::TargetUnreadable(error))))?;
    let vacancy = TargetBefore::Absent;
    if !vacancy.satisfied_by(&observed) {
        return Err(Box::new(PutFailure::new(PutFailureReason::Reconcile {
            expected: vacancy,
            observed,
        })));
    }

    // 6. 先登记 pending：并发的同一 OpId 只有一个能继续，崩溃重发得到的也是 pending 而不是第二份副本。
    match store
        .begin_conflict(request.op, request.digest)
        .await
        .map_err(conflict_store_failure)?
    {
        ConflictBegin::Started => {}
        ConflictBegin::Recorded(recorded) => return resolve_recorded(recorded, request),
    }

    // 7. 身份由服务器为该 OpId 分配，并在同一事务里与结果一起持久化。
    let sibling = EntryId::generate(ids);
    let new = NewPutIntent {
        intent_id: request.intent_id,
        vault: request.op.vault,
        target_path: request.target_path.clone(),
        staging_path: request.staging_path.clone(),
        before: TargetBefore::Absent,
        after: TargetAfter {
            digest: request.content.digest,
            size: request.content.bytes,
        },
    };
    let published = publish_put(log, target, at, &new)
        .await
        .map_err(|failure| reclassify_publish_failure(*failure))?;

    // 8. 同一个数据库事务：冲突条目 + 版本 + 映射 + 事件批次 + 操作结果 + 意图收尾。
    let write = ConflictWrite {
        intent_id: request.intent_id,
        key: request.op,
        digest: request.digest,
        source: request.entry,
        sibling,
        path: request.target_path.clone(),
        content: request.content,
        at,
    };
    let record = store
        .apply_conflict(&write)
        .await
        .map_err(|error| match error {
            // 库里该路径已被占用：不覆盖目标，让客户端重新规划另名；已发布的意图仍是恢复证据。
            ConflictStoreError::PathOccupied { path } => Box::new(PutFailure::after_intent(
                PutFailureReason::SubtreeChanged {
                    detail: format!("冲突路径 {path:?} 已被占用，不能覆盖目标"),
                },
                &published.intent,
            )),
            error => Box::new(PutFailure::after_intent(
                PutFailureReason::ConflictStore(error),
                &published.intent,
            )),
        })?;
    check_record(&record, request, sibling)?;
    Ok(record.conflict)
}

fn check_epoch(head: &ConflictHead, request: &ConflictRequest) -> Result<(), Box<PutFailure>> {
    if head.epoch == request.expected_epoch {
        return Ok(());
    }
    Err(Box::new(PutFailure::new(PutFailureReason::EpochChanged {
        expected: request.expected_epoch,
        actual: head.epoch,
    })))
}

/// 按已记录的 `OpId` 状态回答：重放原结果、拒绝不同请求，或拒绝重做未完成的记录。
fn resolve_recorded(
    recorded: RecordedOperation,
    request: &ConflictRequest,
) -> Result<CommittedConflict, Box<PutFailure>> {
    if recorded.digest != request.digest {
        return Err(Box::new(PutFailure::new(PutFailureReason::OpIdReused {
            op: request.op.op,
            recorded: recorded.digest,
            requested: request.digest,
        })));
    }
    let Some(result) = recorded.result else {
        return Err(Box::new(PutFailure::new(
            PutFailureReason::OperationPending { op: request.op.op },
        )));
    };
    // 摘要相同就必须描述同一个请求；否则摘要没有覆盖这些语义字段，是调用方缺陷，停写核对。
    if result.op != request.op.op
        || result.source != request.entry
        || result.path != request.target_path
        || result.content != request.content
    {
        return Err(Box::new(PutFailure::new(PutFailureReason::StoreContract {
            detail: format!(
                "同一 OpId {} 的请求摘要相同但记录的是另一份请求（源条目 {}、路径 {:?}、内容 {}）",
                request.op.op, result.source, result.path, result.content.digest
            ),
        })));
    }
    Ok(result)
}

/// 事务声称成功时核对返回的证据：不一致说明适配器违反了端口契约，必须停写人工核对，不能 ACK。
fn check_record(
    record: &ConflictRecord,
    request: &ConflictRequest,
    sibling: EntryId,
) -> Result<(), Box<PutFailure>> {
    let conflict = &record.conflict;
    let problem = if record.intent_id != request.intent_id {
        Some(format!(
            "事务收尾的意图 {} 不是请求意图 {}",
            record.intent_id, request.intent_id
        ))
    } else if !record.phase.is_finished() {
        Some(format!("意图阶段是 {} 而不是 finalized", record.phase))
    } else if conflict.op != request.op.op
        || conflict.source != request.entry
        || conflict.sibling != sibling
        || conflict.path != request.target_path
        || conflict.content != request.content
    {
        Some(format!(
            "事务返回的冲突结果是条目 {}（路径 {:?}、内容 {}），不是请求的副本 {sibling}（路径 {:?}、内容 {}）",
            conflict.sibling,
            conflict.path,
            conflict.content.digest,
            request.target_path,
            request.content.digest
        ))
    } else {
        None
    };
    match problem {
        Some(detail) => Err(Box::new(PutFailure::new(PutFailureReason::StoreContract {
            detail,
        }))),
        None => Ok(()),
    }
}

fn conflict_store_failure(error: ConflictStoreError) -> Box<PutFailure> {
    Box::new(PutFailure::new(PutFailureReason::ConflictStore(error)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use vaultsyncd_domain::error::ErrorCode;

    use crate::commit::intent::PutPhase;
    use crate::objects::ContentDigest;
    use crate::ports::object_store::StorageFailure;
    use crate::ports::target::{TargetError, TargetObservation};
    use crate::test_support::{
        CountingIds, FakeConflictStore, FakeIntentLog, FakeTarget, Trace, device_id, entry_id,
        epoch, op_id, request_digest, revision, user_time, vault_id, vault_seq,
    };

    /// 迟到分支（本设备）的内容。
    const LATE_HEX: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    /// 另一设备的迟到内容：与 `LATE_HEX` 不同，但同样“过期 + 不同内容”。
    const OTHER_HEX: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    /// 第三台设备的迟到内容。
    const THIRD_HEX: &str = "4444444444444444444444444444444444444444444444444444444444444444";
    /// 服务器 head 的现场内容（与两份迟到内容都不同）。
    const HEAD_HEX: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const CONFLICT_PATH: &str = "docs/note（冲突-PC1-20261105T083000Z-41）.txt";

    fn digest(hex: &str) -> ContentDigest {
        ContentDigest::from_hex(hex).expect("测试用摘要合法")
    }

    fn key(device: u8, op: u8) -> OperationKey {
        OperationKey {
            vault: vault_id(0x21),
            device: device_id(device),
            op: op_id(op),
        }
    }

    /// 一份“基线 7、head 已到 9、内容不同”的冲突请求。
    fn request(late_hex: &str, device: u8, op: u8) -> ConflictRequest {
        ConflictRequest {
            intent_id: crate::test_support::intent_id(0x41),
            op: key(device, op),
            digest: request_digest(op),
            entry: entry_id(0x51),
            expected_epoch: epoch(3),
            expected_revision: revision(7),
            target_path: CONFLICT_PATH.to_owned(),
            staging_path: "conflict-0041".to_owned(),
            content: ExpectedContent::new(digest(late_hex), 5),
        }
    }

    type Fixture = (
        ConflictRequest,
        VaultCommitGate,
        FakeConflictStore,
        FakeIntentLog,
        FakeTarget,
        CountingIds,
    );

    fn fixture(late_hex: &str, device: u8, op: u8) -> Fixture {
        let trace = Trace::default();
        let mut store = FakeConflictStore::for_source();
        store.trace = trace.clone();
        let log = FakeIntentLog {
            trace: trace.clone(),
            ..FakeIntentLog::default()
        };
        let target = FakeTarget::absent().with_trace(trace);
        (
            request(late_hex, device, op),
            VaultCommitGate::new(),
            store,
            log,
            target,
            CountingIds::new(0x90),
        )
    }

    /// 已提交结果（与 `request` 相匹配），用来模拟“上次提交成功但响应丢失”。
    fn committed(request: &ConflictRequest, sibling_marker: u8) -> CommittedConflict {
        CommittedConflict {
            op: request.op.op,
            source: request.entry,
            sibling: entry_id(sibling_marker),
            path: request.target_path.clone(),
            content: request.content,
            revision: revision(1),
            seq: vault_seq(21),
        }
    }

    #[tokio::test]
    async fn stale_different_content_commits_one_conflict_copy_and_mapping() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);

        let committed = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect("过期基线上的不同内容成为冲突副本");

        assert_eq!(
            store.trace.entries(),
            vec![
                format!("store conflict_head {} {}", request.op.vault, request.entry),
                format!("store operation {}", request.op.op),
                format!("target observe {CONFLICT_PATH}"),
                format!(
                    "store begin_conflict {} {:?}",
                    request.op.op, request.digest
                ),
                format!("target observe {CONFLICT_PATH}"),
                "intent begin prepared".to_owned(),
                format!("target publish {CONFLICT_PATH}"),
                "intent advance published".to_owned(),
                "store apply_conflict".to_owned(),
            ],
            "顺序固定：事实/幂等记录 → 目标空缺预检 → 登记 pending → 意图 → 发布 → 冲突事务"
        );
        // 副本身份由服务器分配，与结果一起持久化；head 保留在主路径上。
        assert_eq!(committed.op, request.op.op);
        assert_eq!(committed.source, request.entry);
        assert_eq!(committed.sibling, entry_id(0x91));
        assert_eq!(committed.path, CONFLICT_PATH);
        assert_eq!(committed.content, request.content);
        assert_eq!(store.writes.len(), 1);
        assert_eq!(store.writes[0].sibling, entry_id(0x91));
        assert_eq!(store.writes[0].source, request.entry);
        assert_eq!(store.writes[0].key, request.op);
        assert_eq!(store.writes[0].digest, request.digest);
        assert_eq!(store.writes[0].path, CONFLICT_PATH);
        // 发布到工作目录的内容就是迟到分支内容（按 hash 可查）。
        assert_eq!(target.publishes.len(), 1);
        assert_eq!(target.publishes[0].digest, digest(LATE_HEX));
        assert_eq!(target.publishes[0].size, 5);
        // 意图走同一套阶段机：假端口分开记录，真实适配器在同一事务里收尾。
        assert_eq!(log.intents[0].phase, PutPhase::Published);
        assert_eq!(log.intents[0].target_path, CONFLICT_PATH);
        assert!(!gate.is_held(request.op.vault), "退出前必须释放 Vault 票据");
    }

    #[tokio::test]
    async fn retry_after_a_lost_response_returns_the_original_copy_without_a_second_one() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        let first = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect("首次提交成功");

        let before = store.trace.entries().len();
        let replay = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(140),
            &request,
        )
        .await
        .expect("响应丢失后重试必须拿到原结果");

        assert_eq!(
            replay, first,
            "同一 OpId 同一请求返回同一份副本与同一条映射"
        );
        assert_eq!(
            store.trace.entries()[before..].to_vec(),
            vec![
                format!("store conflict_head {} {}", request.op.vault, request.entry),
                format!("store operation {}", request.op.op),
            ],
            "重放只读事实与幂等记录，不再登记、不再发布"
        );
        assert_eq!(store.writes.len(), 1, "只写了一条冲突映射");
        assert_eq!(target.publishes.len(), 1, "没有第二份副本");
        assert_eq!(log.intents.len(), 1, "没有第二个意图");
    }

    #[tokio::test]
    async fn same_op_id_with_a_different_request_is_rejected() {
        let (mut request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        store.record_result(request.op, request.digest, committed(&request, 0x91));
        // 同一个 OpId 换一份内容：摘要不同，按协议错误拒绝，不用旧结果回答新请求。
        request.content = ExpectedContent::new(digest(OTHER_HEX), 5);
        request.digest = request_digest(0x99);

        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("同一 OpId 不同请求");

        assert_eq!(failure.code(), "op_id_reused");
        assert_eq!(failure.error_code(), Some(ErrorCode::OpIdReused));
        assert!(!failure.is_committed());
        assert!(store.writes.is_empty(), "不得用新请求覆盖旧结果");
        assert!(target.publishes.is_empty());
        assert!(!gate.is_held(request.op.vault));
    }

    #[tokio::test]
    async fn recorded_result_for_another_payload_is_a_store_contract_violation() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        let mut other = committed(&request, 0x91);
        other.path = "docs/another.conflict.txt".to_owned();
        store.record_result(request.op, request.digest, other);

        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("摘要相同却记录着另一份请求");

        assert_eq!(failure.code(), "store_contract_violation");
        assert_eq!(failure.error_code(), Some(ErrorCode::NeedsRecovery));
        assert!(target.publishes.is_empty());
    }

    #[tokio::test]
    async fn a_pending_operation_is_never_redone() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        // 另一进程刚登记了同一 OpId 但还没有结果（或上次在发布后崩溃）。
        store
            .begin_script
            .push_back(ConflictBegin::Recorded(RecordedOperation {
                digest: request.digest,
                result: None,
            }));

        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("pending 不是 committed");

        assert_eq!(failure.code(), "operation_pending");
        assert_eq!(failure.error_code(), Some(ErrorCode::NeedsRecovery));
        assert!(!failure.is_committed());
        assert!(store.writes.is_empty());
        assert!(target.publishes.is_empty(), "不重做就不会有第二份副本");
        assert!(log.intents.is_empty(), "不重做就不写第二个意图");
    }

    #[tokio::test]
    async fn current_baseline_is_not_a_conflict() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        if let Some(source) = store.head.source.as_mut() {
            source.revision = revision(7);
        }

        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("基线没过期，不构成冲突副本");

        assert_eq!(failure.code(), "conflict_not_stale");
        assert_eq!(failure.error_code(), Some(ErrorCode::ReconcileRequired));
        assert!(store.writes.is_empty());
        assert!(target.publishes.is_empty());
        assert!(
            !store
                .trace
                .entries()
                .iter()
                .any(|line| line.starts_with("store begin_conflict")),
            "被拒绝的请求不登记 pending"
        );
    }

    #[tokio::test]
    async fn identical_content_at_head_is_merged_without_a_copy() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        if let Some(source) = store.head.source.as_mut() {
            source.content = Some(digest(LATE_HEX));
        }

        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("同结果只归并");

        assert_eq!(failure.code(), "content_already_committed");
        assert_eq!(failure.error_code(), Some(ErrorCode::ReconcileRequired));
        assert!(store.writes.is_empty());
        assert!(target.publishes.is_empty(), "同内容不额外造副本");
    }

    #[tokio::test]
    async fn epoch_change_is_rejected_before_any_replay() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        store.record_result(request.op, request.digest, committed(&request, 0x91));
        store.head.epoch = epoch(4);

        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("旧 epoch 不自动重放");

        assert_eq!(failure.code(), "epoch_changed");
        assert_eq!(failure.error_code(), Some(ErrorCode::EpochChanged));
        assert_eq!(
            store.trace.entries(),
            vec![format!(
                "store conflict_head {} {}",
                request.op.vault, request.entry
            )],
            "epoch 不符时连幂等记录都不读"
        );
    }

    #[tokio::test]
    async fn recorded_result_is_replayed_even_after_the_source_entry_is_gone() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        store.record_result(request.op, request.digest, committed(&request, 0x91));
        store.head.source = None;

        let replay = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect("副本还在，重试仍返回原结果");

        assert_eq!(replay.sibling, entry_id(0x91));
        assert!(store.writes.is_empty());
        assert!(target.publishes.is_empty());
    }

    #[tokio::test]
    async fn a_missing_source_without_a_recorded_result_is_a_base_mismatch() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        store.head.source = None;

        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("源条目已不存在");

        assert_eq!(failure.code(), "base_mismatch");
        assert_eq!(failure.error_code(), Some(ErrorCode::BaseMismatch));
        assert!(target.publishes.is_empty());
    }

    #[tokio::test]
    async fn directory_source_cannot_take_a_conflict_copy() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        if let Some(source) = store.head.source.as_mut() {
            source.kind = crate::ports::put_store::EntryKind::Directory;
            source.content = None;
        }

        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("目录不能承载内容");

        assert_eq!(failure.code(), "unsupported_entry");
        assert_eq!(failure.error_code(), Some(ErrorCode::UnsupportedEntry));
        assert!(target.publishes.is_empty());
    }

    #[tokio::test]
    async fn an_occupied_conflict_target_is_rejected_before_the_op_is_claimed() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        target.set_default(TargetObservation::Present {
            identity: crate::ports::object_store::FileKey {
                device: 7,
                inode: 77,
                links: 1,
                bytes: 3,
            },
            digest: digest(OTHER_HEX),
        });

        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("目标已被占用");

        assert_eq!(failure.code(), "reconcile_required");
        assert_eq!(failure.error_code(), Some(ErrorCode::ReconcileRequired));
        assert!(log.intents.is_empty());
        assert!(store.writes.is_empty());
        assert!(
            !store
                .trace
                .entries()
                .iter()
                .any(|line| line.starts_with("store begin_conflict")),
            "可重新规划的目标冲突不锁住 OpId"
        );
    }

    #[tokio::test]
    async fn publish_failure_leaves_the_intent_prepared_and_never_commits() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        target.publish_error = Some(TargetError::Storage {
            stage: "stage_publish",
            failure: StorageFailure::NoSpace,
            detail: "No space left on device".to_owned(),
        });

        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("满盘");

        assert_eq!(failure.code(), "no_space");
        assert!(!failure.is_committed());
        assert_eq!(failure.intent_id, Some(request.intent_id));
        assert_eq!(failure.phase, Some(PutPhase::Prepared));
        assert_eq!(log.intents[0].phase, PutPhase::Prepared, "意图是恢复证据");
        assert!(store.writes.is_empty(), "发布失败不得写冲突映射");
        assert!(!gate.is_held(request.op.vault));
    }

    #[tokio::test]
    async fn transaction_failure_keeps_published_and_the_retry_waits_for_recovery() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        store.apply_error = Some(ConflictStoreError::Storage {
            detail: "SQLITE_BUSY".to_owned(),
        });

        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("事务失败");

        assert_eq!(failure.code(), "conflict_store_failed");
        assert!(!failure.is_committed());
        assert_eq!(failure.phase, Some(PutPhase::Published));
        assert_eq!(target.publishes.len(), 1, "工作文件已发布，但元数据未提交");

        // 重试：pending 记录还在，不能重做到第二份副本。
        let retry = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(140),
            &request,
        )
        .await
        .expect_err("等恢复收尾");
        assert_eq!(retry.code(), "operation_pending");
        assert_eq!(target.publishes.len(), 1);
    }

    #[tokio::test]
    async fn three_devices_keep_three_findable_copies_and_each_retry_reuses_its_own() {
        let mut gate = VaultCommitGate::new();
        let trace = Trace::default();
        let mut store = FakeConflictStore::for_source();
        store.trace = trace.clone();
        let mut log = FakeIntentLog {
            trace: trace.clone(),
            ..FakeIntentLog::default()
        };
        let mut target = FakeTarget::absent().with_trace(trace);
        let mut ids = CountingIds::new(0x90);

        let mut results = Vec::new();
        for (index, (late, device, op)) in [
            (LATE_HEX, 0x71, 0x41),
            (OTHER_HEX, 0x72, 0x42),
            (THIRD_HEX, 0x73, 0x43),
        ]
        .into_iter()
        .enumerate()
        {
            let mut request = request(late, device, op);
            request.target_path = format!("docs/note（冲突-device{index}-op{op:x}）.txt");
            // 三台设备的冲突路径不同，每条路径的现场默认空缺；意图身份也各自独立。

            request.intent_id = crate::test_support::intent_id(0x41 + index as u8);
            let committed = commit_conflict(
                &mut gate,
                &mut store,
                &mut log,
                &mut target,
                &mut ids,
                user_time(100 + index as i64),
                &request,
            )
            .await
            .expect("每台设备的不同内容都成为自己的冲突条目");
            results.push((request, committed));
        }

        assert_eq!(store.writes.len(), 3);
        assert_eq!(target.publishes.len(), 3);
        let siblings: std::collections::BTreeSet<_> = results
            .iter()
            .map(|(_, committed)| committed.sibling)
            .collect();
        assert_eq!(siblings.len(), 3, "三台设备各自一条副本身份");
        let digests: std::collections::BTreeSet<_> = results
            .iter()
            .map(|(_, committed)| committed.content.digest)
            .collect();
        assert_eq!(digests.len(), 3, "三份不同内容都能按 hash 找到");
        for (request, committed) in &results {
            assert_eq!(
                committed.source, request.entry,
                "映射都指向同一个 head 条目"
            );
        }

        // 每台设备重试只拿到自己的副本。
        let publishes = target.publishes.len();
        for (request, expected) in &results {
            let replay = commit_conflict(
                &mut gate,
                &mut store,
                &mut log,
                &mut target,
                &mut ids,
                user_time(200),
                request,
            )
            .await
            .expect("重试返回原结果");
            assert_eq!(&replay, expected);
        }
        assert_eq!(target.publishes.len(), publishes, "重试没有新增副本");
        assert_eq!(store.writes.len(), 3);
    }

    #[tokio::test]
    async fn request_shape_is_checked_before_any_port_is_touched() {
        let (mut request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);

        request.staging_path = "../escape".to_owned();
        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("暂存名逃逸");
        assert_eq!(failure.code(), "invalid_request");
        assert!(store.trace.entries().is_empty(), "形状不成立时不得调用端口");

        let (mut request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        request.target_path = "../outside.txt".to_owned();
        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("冲突路径逃逸 Vault");
        assert_eq!(failure.code(), "invalid_request");
        assert!(target.publishes.is_empty());
        assert!(!gate.is_held(request.op.vault));
    }

    #[tokio::test]
    async fn one_vault_commits_at_a_time() {
        let (request, mut gate, mut store, mut log, mut target, mut ids) =
            fixture(LATE_HEX, 0x71, 0x41);
        let held = gate.acquire(request.op.vault).expect("占住该 Vault");

        let failure = commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect_err("同 Vault 已有提交");
        assert_eq!(failure.code(), "vault_commit_busy");
        assert!(store.trace.entries().is_empty(), "未拿到票据不得读库");
        assert!(gate.is_held(request.op.vault), "别人的票据不被顺手释放");

        gate.release(held);
        commit_conflict(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            &mut ids,
            user_time(100),
            &request,
        )
        .await
        .expect("释放后可以提交");
    }

    #[test]
    fn request_digest_hex_is_canonical() {
        assert_eq!(request_digest(0xab).to_hex(), "ab".repeat(32));
        assert_eq!(
            format!("{:?}", request_digest(0x0f)),
            format!("RequestDigest({})", "0f".repeat(32))
        );
        assert_eq!(request_digest(0x01).as_bytes(), &[0x01; 32]);
    }

    #[test]
    fn fixture_head_differs_from_every_late_content() {
        // 夹具自身的不变量：head 内容与三份迟到内容都不同，才谈得上“过期 + 不同内容”。
        assert_ne!(digest(HEAD_HEX), digest(LATE_HEX));
        assert_ne!(digest(HEAD_HEX), digest(OTHER_HEX));
        assert_ne!(digest(HEAD_HEX), digest(THIRD_HEX));
        assert_ne!(key(0x71, 0x41), key(0x72, 0x41));
        assert_ne!(key(0x71, 0x41), key(0x71, 0x42));
    }
}
