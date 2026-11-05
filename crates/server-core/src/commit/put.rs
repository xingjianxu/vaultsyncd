//! 内容 put 的 CAS 提交：Root 内串行化 → 基线复核 → 对象持久 → 意图 → 目标发布 → 元数据事务 → ACK
//! （design §11.2、§12.2、I01、I04）。
//!
//! 本模块把 S034 的单文件意图状态机包成一次完整提交。它只在核心做判定，I/O 全部走端口：
//!
//! 1. **Root 内串行化**：同一 Root 同时只有一个提交持有 [`RootCommitTicket`]；另一个提交得到
//!    `root_commit_busy` 而不是并发改同一棵树（内部锁只序列化本服务操作，不能锁住外部应用，
//!    design §11.2 步骤 5）。
//! 2. **基线复核**：Root epoch 与请求预期不符 → `EPOCH_CHANGED`；条目 revision 已不是请求基线 →
//!    `BASE_MISMATCH`；父目录/名称在计划后变化 → `SUBTREE_CHANGED`；条目不是普通文件 →
//!    `UNSUPPORTED_ENTRY`。这些都在动磁盘之前完成，过期基线绝不覆盖服务器的新事实（I04）。
//! 3. **对象持久**：内容对象必须已按内容地址登记且长度相符，否则不开始发布（design §11.2 步骤 3）。
//! 4. **现场指纹**：工作目录实测与请求的 `before` 不符 → `RECONCILE_REQUIRED`，等服务端采集完外部
//!    变化后重新规划；不把外部写入当成可覆盖的东西（design §12.2）。
//! 5. **意图 → 发布 → 元数据事务**：复用 S034 的 [`publish_put`] 顺序，然后在一个数据库事务里写
//!    版本/条目 revision/事件批次/操作结果并把意图收尾到 `finalized`。事务没提交就没有 ACK
//!    （I01：发布完成不等于 committed）。
//!
//! 失败永远不是 committed：任何路径返回 [`PutFailure`]，`is_committed()` 为 `false`，已持久化的
//! 意图行保留作为恢复证据（design §11.3）。失败原因带 wire 错误码（[`PutFailure::error_code`]），
//! 由 API 层直接映射，不在上层重写一套字符串。

use std::collections::BTreeMap;
use std::fmt;

use vaultsyncd_domain::ids::{EntryId, IntentId, RootId};
use vaultsyncd_domain::version::{Revision, ServerEpoch, UtcTimestamp};

use crate::commit::intent::{
    NewPutIntent, PutFailure, PutFailureReason, TargetAfter, TargetBefore, publish_put,
};
use crate::objects::{ExpectedContent, ObjectLayout};
use crate::ports::intent_log::IntentLog;
use crate::ports::put_store::{
    EntryBaseline, PutCommitStore, PutRecord, PutStoreError, PutWrite, RootFacts,
};
use crate::ports::target::{PublishedTarget, TargetPort};

/// 一次内容 put 的请求：客户端看到的基线 + 已上传并校验的内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutRequest {
    /// 意图身份（由调用方生成，重试必须复用同一个身份）。
    pub intent_id: IntentId,
    /// 所属 Root。
    pub root: RootId,
    /// 被替换内容的条目。
    pub entry: EntryId,
    /// 客户端事实基础所属的服务器 epoch。
    pub expected_epoch: ServerEpoch,
    /// 客户端计划时的条目 revision。
    pub expected_revision: Revision,
    /// 客户端计划时的父目录。
    pub expected_parent: EntryId,
    /// 客户端计划时的名称。
    pub expected_name: String,
    /// 目标在 Root 内的相对路径（最后一段必须等于 `expected_name`）。
    pub target_path: String,
    /// 目标所在子卷内的暂存文件名（单个文件名）。
    pub staging_path: String,
    /// 已上传并校验的内容（对象必须已持久化）。
    pub content: ExpectedContent,
    /// 客户端观察到的现场指纹（发布前必须仍然相符）。
    pub before: TargetBefore,
}

impl PutRequest {
    /// 在调用任何端口之前检查请求自身的形状。
    ///
    /// “检查名称”和“发布到 target_path”必须是同一件事：暂存名只能是单个文件名（不能借 `../`
    /// 把发布指向别处，I10），`target_path` 的最后一段必须就是 `expected_name`；`before` 里记录的
    /// 条目/revision 必须与请求基线一致，否则同一请求里有两套基线。
    fn validate(&self) -> Result<(), Box<PutFailure>> {
        ObjectLayout::validate_staging_name(&self.staging_path)
            .map_err(|error| invalid_request(error.to_string()))?;
        validate_name(&self.expected_name).map_err(invalid_request)?;
        validate_target_path(&self.target_path).map_err(invalid_request)?;
        if target_name(&self.target_path) != Some(self.expected_name.as_str()) {
            return Err(invalid_request(format!(
                "目标路径 {:?} 的最后一段不是基线名称 {:?}",
                self.target_path, self.expected_name
            )));
        }
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

/// 一次成功提交：元数据事务已提交，意图已收尾，目标实测等于请求内容。
#[derive(Clone, Debug)]
pub struct CommittedPut {
    /// 元数据事务结果（`phase` 必须是 `finalized`）。
    pub record: PutRecord,
    /// 发布后目标的实测现场。
    pub target: PublishedTarget,
}

impl CommittedPut {
    /// 已收尾的意图身份。
    #[must_use]
    pub fn intent_id(&self) -> IntentId {
        self.record.intent_id
    }
}

/// 一次 Root 内提交的占用票据（只能由 [`RootCommitGate::acquire`] 产生）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootCommitTicket {
    root: RootId,
    serial: u64,
}

impl RootCommitTicket {
    /// 被串行化的 Root。
    #[must_use]
    pub fn root(&self) -> RootId {
        self.root
    }

    /// 本次占用在该 Gate 内的序号（日志与诊断用）。
    #[must_use]
    pub fn serial(&self) -> u64 {
        self.serial
    }
}

/// Root 提交串行化冲突。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitGateError {
    /// 同一 Root 已有提交在进行：调用方应稍后重试，不并发改同一棵树。
    Busy {
        /// 被占用的 Root。
        root: RootId,
        /// 正在进行的占用序号。
        serial: u64,
    },
}

impl CommitGateError {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            CommitGateError::Busy { .. } => "root_commit_busy",
        }
    }
}

impl fmt::Display for CommitGateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitGateError::Busy { root, serial } => write!(
                f,
                "Root {root} 已有提交在进行（占用序号 {serial}），本请求不能并发进入"
            ),
        }
    }
}

impl std::error::Error for CommitGateError {}

/// 进程内的 Root 提交调度锁：同一 Root 同时只允许一次提交，不同 Root 互不阻塞。
///
/// 只做串行化，不做排队：第二个同 Root 提交得到 [`CommitGateError::Busy`]，由调用方退避重试，
/// 避免请求在锁上无限堆积。跨进程串行化（多个服务进程共享一份数据库）需要数据库侧的锁，属服务端
/// 组装卡的职责；本 Gate 保证单进程内不会有两个 put 同时改同一棵树。
#[derive(Default)]
pub struct RootCommitGate {
    held: BTreeMap<RootId, u64>,
    next_serial: u64,
}

impl RootCommitGate {
    /// 建立空 Gate。
    #[must_use]
    pub fn new() -> Self {
        RootCommitGate::default()
    }

    /// 取得某个 Root 的提交票据；该 Root 已被占用时返回 `Busy`，不阻塞。
    ///
    /// # Errors
    ///
    /// 同一 Root 已被占用时返回 [`CommitGateError::Busy`]。
    pub fn acquire(&mut self, root: RootId) -> Result<RootCommitTicket, CommitGateError> {
        if let Some(serial) = self.held.get(&root) {
            return Err(CommitGateError::Busy {
                root,
                serial: *serial,
            });
        }
        self.next_serial += 1;
        self.held.insert(root, self.next_serial);
        Ok(RootCommitTicket {
            root,
            serial: self.next_serial,
        })
    }

    /// 释放票据。
    ///
    /// 票只能由 [`acquire`](RootCommitGate::acquire) 产生，因此释放不失败；票据与当前占用不匹配
    /// 说明调用方重复释放或混用了别的 Gate，属于编程错误。
    pub fn release(&mut self, ticket: RootCommitTicket) {
        let held = self.held.remove(&ticket.root);
        assert_eq!(
            held,
            Some(ticket.serial),
            "提交票据与 Root {} 的占用不匹配",
            ticket.root
        );
    }

    /// 该 Root 当前是否已有提交在进行。
    #[must_use]
    pub fn is_held(&self, root: RootId) -> bool {
        self.held.contains_key(&root)
    }
}

/// 提交一次内容 put。
///
/// 顺序见模块文档：请求形状 → Root 串行化 → epoch → 命名空间基线 → 对象持久 → 现场指纹 →
/// 意图 → 目标发布 → 元数据事务 → ACK。无论成功还是失败，退出前都会释放 Root 票据。
///
/// # Errors
///
/// 见 [`PutFailure`]：请求不成立、Root 忙、epoch 变化、基线过期/子树变化、对象未持久、现场不符、
/// 意图/发布/存储失败、存储契约不一致。失败都不是 committed，已持久化的意图行保留为恢复证据。
pub async fn commit_put<S, L, T>(
    gate: &mut RootCommitGate,
    store: &mut S,
    log: &mut L,
    target: &mut T,
    at: UtcTimestamp,
    request: &PutRequest,
) -> Result<CommittedPut, Box<PutFailure>>
where
    S: PutCommitStore + ?Sized,
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
{
    request.validate()?;
    let ticket = gate
        .acquire(request.root)
        .map_err(|error| Box::new(PutFailure::new(PutFailureReason::Gate(error))))?;
    let outcome = commit_under_ticket(store, log, target, at, request).await;
    gate.release(ticket);
    outcome
}

async fn commit_under_ticket<S, L, T>(
    store: &mut S,
    log: &mut L,
    target: &mut T,
    at: UtcTimestamp,
    request: &PutRequest,
) -> Result<CommittedPut, Box<PutFailure>>
where
    S: PutCommitStore + ?Sized,
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
{
    // 1. epoch：恢复过的服务器不是同一套事实，旧预期的提交直接作废。
    let facts = store
        .root_facts(request.root)
        .await
        .map_err(store_failure)?;
    check_epoch(&facts, request)?;

    // 2. 命名空间基线：过期 revision 与已变化的父目录/名称都不得覆盖。
    let baseline = match store.entry_baseline(request.root, request.entry).await {
        Ok(baseline) => baseline,
        Err(PutStoreError::EntryMissing { entry }) => {
            return Err(base_mismatch(format!("条目 {entry} 已不存在")));
        }
        Err(PutStoreError::UnknownRoot { root }) => {
            return Err(base_mismatch(format!("Root {root} 已不存在")));
        }
        Err(error) => return Err(store_failure(error)),
    };
    check_baseline(&baseline, request)?;

    // 3. 对象持久：内容还没固定成对象就没有可发布的内容。
    match store
        .object_state(request.root, request.content.digest)
        .await
        .map_err(store_failure)?
    {
        crate::ports::put_store::ObjectState::Persisted { bytes } => {
            if bytes != request.content.bytes {
                return Err(Box::new(PutFailure::new(
                    PutFailureReason::ObjectSizeMismatch {
                        expected: request.content.bytes,
                        actual: bytes,
                    },
                )));
            }
        }
        crate::ports::put_store::ObjectState::Missing => {
            return Err(Box::new(PutFailure::new(
                PutFailureReason::ObjectNotPersisted {
                    digest: request.content.digest,
                },
            )));
        }
    }

    // 4. 现场指纹：外部写入不是可覆盖的东西（design §12.2）。
    let observed = target
        .observe(&request.target_path)
        .map_err(|error| Box::new(PutFailure::new(PutFailureReason::TargetUnreadable(error))))?;
    if !request.before.satisfied_by(&observed) {
        return Err(Box::new(PutFailure::new(PutFailureReason::Reconcile {
            expected: request.before.clone(),
            observed,
        })));
    }

    // 5. 意图 → 发布：S034 的顺序，发布后实测不符时不推进阶段。
    let new = NewPutIntent {
        intent_id: request.intent_id,
        root: request.root,
        target_path: request.target_path.clone(),
        staging_path: request.staging_path.clone(),
        before: request.before.clone(),
        after: TargetAfter {
            digest: request.content.digest,
            size: request.content.bytes,
        },
    };
    let published = publish_put(log, target, at, &new)
        .await
        .map_err(|failure| reclassify_publish_failure(*failure))?;

    // 6. 元数据/changes/结果事务：与意图收尾同一事务，事务没提交就没有 ACK。
    let write = PutWrite {
        intent_id: request.intent_id,
        root: request.root,
        entry: request.entry,
        expected_revision: request.expected_revision,
        content: request.content,
        target_path: request.target_path.clone(),
        at,
    };
    let record = store.apply_put(&write).await.map_err(|error| match error {
        PutStoreError::RevisionChanged { expected, actual } => Box::new(PutFailure::after_intent(
            PutFailureReason::BaseMismatch {
                detail: format!("提交时条目 revision 已从 {expected} 变为 {actual}"),
            },
            &published.intent,
        )),
        PutStoreError::EntryMissing { entry } => Box::new(PutFailure::after_intent(
            PutFailureReason::BaseMismatch {
                detail: format!("提交时条目 {entry} 已不存在"),
            },
            &published.intent,
        )),
        error => Box::new(PutFailure::after_intent(
            PutFailureReason::Store(error),
            &published.intent,
        )),
    })?;
    check_record(&record, request, &published.intent)?;
    Ok(CommittedPut {
        record,
        target: published.target,
    })
}

fn check_epoch(facts: &RootFacts, request: &PutRequest) -> Result<(), Box<PutFailure>> {
    if facts.epoch == request.expected_epoch {
        return Ok(());
    }
    Err(Box::new(PutFailure::new(PutFailureReason::EpochChanged {
        expected: request.expected_epoch,
        actual: facts.epoch,
    })))
}

fn check_baseline(baseline: &EntryBaseline, request: &PutRequest) -> Result<(), Box<PutFailure>> {
    if baseline.entry != request.entry || baseline.root != request.root {
        return Err(base_mismatch(format!(
            "库里的条目 {}（Root {}）不是请求条目 {}",
            baseline.entry, baseline.root, request.entry
        )));
    }
    if baseline.revision != request.expected_revision {
        return Err(base_mismatch(format!(
            "条目 revision 已是 {}，请求基线是 {}",
            baseline.revision, request.expected_revision
        )));
    }
    if baseline.parent != request.expected_parent || baseline.name != request.expected_name {
        return Err(Box::new(PutFailure::new(
            PutFailureReason::SubtreeChanged {
                detail: format!(
                    "条目现在挂在 {} 下名为 {:?}，请求基线是 {} 下名为 {:?}",
                    baseline.parent, baseline.name, request.expected_parent, request.expected_name
                ),
            },
        )));
    }
    if !baseline.kind.is_file() {
        return Err(Box::new(PutFailure::new(
            PutFailureReason::UnsupportedEntry {
                detail: format!(
                    "条目 {} 是{}，不能承载内容",
                    baseline.entry,
                    baseline.kind.as_text()
                ),
            },
        )));
    }
    Ok(())
}

/// 事务声称成功时核对返回的证据：不一致说明适配器违反了端口契约，必须停写人工核对，不能 ACK。
fn check_record(
    record: &PutRecord,
    request: &PutRequest,
    published: &crate::commit::intent::PutIntent,
) -> Result<(), Box<PutFailure>> {
    let problem = if record.intent_id != request.intent_id {
        Some(format!(
            "事务收尾的意图 {} 不是请求意图 {}",
            record.intent_id, request.intent_id
        ))
    } else if !record.phase.is_finished() {
        Some(format!("意图阶段是 {} 而不是 finalized", record.phase))
    } else if record.digest != request.content.digest || record.size != request.content.bytes {
        Some(format!(
            "版本内容是 {}（{} 字节），不是请求内容 {}",
            record.digest, record.size, request.content.digest
        ))
    } else if record.revision <= request.expected_revision {
        Some(format!(
            "条目 revision 没有推进：{} 不大于基线 {}",
            record.revision, request.expected_revision
        ))
    } else {
        None
    };
    match problem {
        Some(detail) => Err(Box::new(PutFailure::after_intent(
            PutFailureReason::StoreContract { detail },
            published,
        ))),
        None => Ok(()),
    }
}

pub(crate) fn invalid_request(detail: impl Into<String>) -> Box<PutFailure> {
    Box::new(PutFailure::new(PutFailureReason::InvalidRequest {
        detail: detail.into(),
    }))
}

pub(crate) fn base_mismatch(detail: impl Into<String>) -> Box<PutFailure> {
    Box::new(PutFailure::new(PutFailureReason::BaseMismatch {
        detail: detail.into(),
    }))
}

fn store_failure(error: PutStoreError) -> Box<PutFailure> {
    Box::new(PutFailure::new(PutFailureReason::Store(error)))
}

/// 现场复核在发布前又发现变化（与预检查之间的竞态）：同样按 `RECONCILE_REQUIRED` 处理。
pub(crate) fn reclassify_publish_failure(failure: PutFailure) -> Box<PutFailure> {
    match failure.reason {
        PutFailureReason::BeforeMismatch { expected, observed } => {
            Box::new(PutFailure::new(PutFailureReason::Reconcile {
                expected,
                observed,
            }))
        }
        reason => Box::new(PutFailure {
            reason,
            intent_id: failure.intent_id,
            phase: failure.phase,
        }),
    }
}

/// 单个名称：非空、不含路径分隔符/NUL、不是 `.`/`..`。
pub(crate) fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("名称不能为空".to_owned());
    }
    if name == "." || name == ".." {
        return Err(format!("名称 {name:?} 不是有效文件名称"));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(format!("名称 {name:?} 含路径分隔符或 NUL"));
    }
    Ok(())
}

/// Root 内相对路径：非空、不以 `/` 起始、无空分量与 `.`/`..` 分量。
pub(crate) fn validate_target_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("目标路径不能为空".to_owned());
    }
    if path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return Err(format!("目标路径 {path:?} 必须是 Root 内的相对路径"));
    }
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(format!("目标路径 {path:?} 含空、`.` 或 `..` 分量"));
        }
    }
    Ok(())
}

pub(crate) fn target_name(path: &str) -> Option<&str> {
    path.rsplit('/').next()
}

#[cfg(test)]
mod tests {
    use super::*;

    use vaultsyncd_domain::error::ErrorCode;

    use crate::commit::intent::{PutPhase, TargetSnapshot};
    use crate::objects::ContentDigest;
    use crate::ports::object_store::StorageFailure;
    use crate::ports::put_store::{EntryKind, ObjectState, PutRecord};
    use crate::ports::target::{TargetError, TargetObservation};
    use crate::test_support::{
        FakeIntentLog, FakePutStore, FakeTarget, Trace, entry_id, epoch, file_baseline, revision,
        root_id, root_seq, target_snapshot, tx_id, user_time, version_id,
    };

    const OLD_HEX: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const NEW_HEX: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn digest(hex: &str) -> ContentDigest {
        ContentDigest::from_hex(hex).expect("测试用摘要合法")
    }

    fn request(before: TargetBefore) -> PutRequest {
        PutRequest {
            intent_id: crate::test_support::intent_id(0x41),
            root: root_id(0x21),
            entry: entry_id(0x51),
            expected_epoch: epoch(3),
            expected_revision: revision(7),
            expected_parent: entry_id(0x61),
            expected_name: "note.txt".to_owned(),
            target_path: "docs/note.txt".to_owned(),
            staging_path: "put-0041".to_owned(),
            content: ExpectedContent::new(digest(NEW_HEX), 5),
            before,
        }
    }

    /// 一份“覆盖已有 9 字节旧内容”的请求 + 相应现场。
    fn overwrite() -> (
        PutRequest,
        TargetSnapshot,
        RootCommitGate,
        FakePutStore,
        FakeTarget,
    ) {
        let root = root_id(0x21);
        let snapshot = target_snapshot(OLD_HEX, 9);
        let request = request(TargetBefore::Present(snapshot.clone()));
        let trace = Trace::default();
        let mut store = FakePutStore::for_entry(file_baseline(root));
        store.trace = trace.clone();
        let target = FakeTarget::present(&snapshot).with_trace(trace);
        (request, snapshot, RootCommitGate::new(), store, target)
    }

    #[tokio::test]
    async fn content_put_commits_in_order_and_acks_after_the_metadata_transaction() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        let mut log = FakeIntentLog {
            trace: store.trace.clone(),
            ..FakeIntentLog::default()
        };

        let committed = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect("内容提交成功");

        assert_eq!(
            store.trace.entries(),
            vec![
                format!("store root_facts {}", request.root),
                format!("store entry_baseline {} {}", request.root, request.entry),
                format!("store object_state {} {}", request.root, digest(NEW_HEX)),
                // 现场观察两次：CAS 预检查一次，发布前 S034 的 publish_put 再复核一次。
                // 这缩小但按 design §12.2 不消除外部写入的竞态窗口。
                "target observe docs/note.txt".to_owned(),
                "target observe docs/note.txt".to_owned(),
                "intent begin prepared".to_owned(),
                "target publish docs/note.txt".to_owned(),
                "intent advance published".to_owned(),
                "store apply_put".to_owned(),
            ],
            "顺序固定：epoch → 基线 → 对象 → 现场 → 意图 → 发布 → 元数据事务"
        );
        // 字节一致：发布与 ACK 的内容就是请求内容。
        assert_eq!(target.publishes.len(), 1);
        assert_eq!(target.publishes[0].digest, digest(NEW_HEX));
        assert_eq!(target.publishes[0].size, 5);
        assert_eq!(committed.target.digest, digest(NEW_HEX));
        assert_eq!(committed.target.identity.bytes, 5);
        assert_eq!(committed.record.digest, digest(NEW_HEX));
        assert_eq!(committed.record.size, 5);
        assert_eq!(committed.record.phase, PutPhase::Finalized);
        assert!(committed.record.phase.is_finished());
        assert_eq!(committed.record.revision, revision(8));
        assert_eq!(committed.intent_id(), request.intent_id);
        assert_eq!(store.writes.len(), 1);
        assert_eq!(store.writes[0].expected_revision, revision(7));
        // 真实适配器在同一事务里把 fs_intents 收尾（端口契约第 3 条）；假端口分开记录。
        assert_eq!(log.intents[0].phase, PutPhase::Published);
        assert!(!gate.is_held(request.root), "退出前必须释放 Root 票据");
    }

    #[tokio::test]
    async fn stale_revision_is_rejected_before_any_disk_change() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        store.baseline.revision = revision(8);
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("过期基线");
        assert_eq!(failure.code(), "base_mismatch");
        assert_eq!(failure.error_code(), Some(ErrorCode::BaseMismatch));
        assert!(!failure.is_committed());
        assert_eq!(failure.intent_id, None, "基线不符不得写意图");
        assert!(log.intents.is_empty());
        assert!(target.publishes.is_empty());
        assert!(store.writes.is_empty());
        assert!(!gate.is_held(request.root));
    }

    #[tokio::test]
    async fn external_site_change_returns_reconcile_required() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        // 外部程序把文件删掉了：现场不再是请求指纹，不能当成覆盖继续。
        target.set_default(TargetObservation::Absent);
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("现场已变");
        assert_eq!(failure.code(), "reconcile_required");
        assert_eq!(failure.error_code(), Some(ErrorCode::ReconcileRequired));
        assert!(!failure.is_committed());
        assert!(log.intents.is_empty(), "现场不符不得写意图");
        assert!(target.publishes.is_empty());
        assert!(!gate.is_held(request.root));
    }

    #[tokio::test]
    async fn epoch_change_is_rejected_and_mapped_to_the_wire_code() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        store.facts = crate::ports::put_store::RootFacts { epoch: epoch(4) };
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("epoch 已变");
        assert_eq!(failure.code(), "epoch_changed");
        assert_eq!(failure.error_code(), Some(ErrorCode::EpochChanged));
        assert!(!failure.is_committed());
        assert!(target.publishes.is_empty());
        assert!(!gate.is_held(request.root));
    }

    #[tokio::test]
    async fn parent_or_name_change_is_subtree_changed() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        let mut log = FakeIntentLog::default();
        store.baseline.name = "renamed.txt".to_owned();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("名称已变");
        assert_eq!(failure.code(), "subtree_changed");
        assert_eq!(failure.error_code(), Some(ErrorCode::SubtreeChanged));
        assert!(log.intents.is_empty());

        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        store.baseline.parent = entry_id(0x62);
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("父目录已变");
        assert_eq!(failure.code(), "subtree_changed");
    }

    #[tokio::test]
    async fn directory_entry_cannot_take_content() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        store.baseline.kind = EntryKind::Directory;
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
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
    async fn object_must_be_persisted_before_the_intent_starts() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        store.object = ObjectState::Missing;
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("对象还没持久化");
        assert_eq!(failure.code(), "object_not_persisted");
        assert_eq!(failure.error_code(), None);
        assert!(log.intents.is_empty(), "没有对象就没有意图");
        assert!(target.publishes.is_empty());
        assert!(!gate.is_held(request.root));
    }

    #[tokio::test]
    async fn object_size_mismatch_is_rejected() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        store.object = ObjectState::Persisted { bytes: 4 };
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("对象长度不符");
        assert_eq!(failure.code(), "object_size_mismatch");
        assert!(log.intents.is_empty());
    }

    #[tokio::test]
    async fn publish_failure_leaves_the_intent_prepared_and_never_commits() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        target.publish_error = Some(TargetError::Storage {
            stage: "stage_publish",
            failure: StorageFailure::NoSpace,
            detail: "No space left on device".to_owned(),
        });
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("满盘");
        assert_eq!(failure.code(), "no_space");
        assert!(!failure.is_committed());
        assert_eq!(failure.intent_id, Some(request.intent_id));
        assert_eq!(failure.phase, Some(PutPhase::Prepared));
        assert_eq!(log.intents[0].phase, PutPhase::Prepared);
        assert!(store.writes.is_empty(), "发布失败不得写元数据");
        assert!(!gate.is_held(request.root));
    }

    #[tokio::test]
    async fn metadata_failure_keeps_published_phase_and_no_ack() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        store.apply_error = Some(PutStoreError::Storage {
            detail: "SQLITE_BUSY".to_owned(),
        });
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("事务失败");
        assert_eq!(failure.code(), "put_store_failed");
        assert!(!failure.is_committed());
        assert_eq!(failure.phase, Some(PutPhase::Published));
        assert_eq!(target.publishes.len(), 1, "工作文件已发布，但元数据未提交");
        assert!(!gate.is_held(request.root));
    }

    #[tokio::test]
    async fn revision_changed_inside_the_transaction_maps_to_base_mismatch() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        store.apply_error = Some(PutStoreError::RevisionChanged {
            expected: revision(7),
            actual: revision(9),
        });
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("事务里 revision 已变");
        assert_eq!(failure.code(), "base_mismatch");
        assert_eq!(failure.error_code(), Some(ErrorCode::BaseMismatch));
        assert_eq!(failure.phase, Some(PutPhase::Published));
        assert!(!failure.is_committed());
    }

    #[tokio::test]
    async fn store_evidence_inconsistency_is_not_reported_as_committed() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        store.record = Some(PutRecord {
            intent_id: request.intent_id,
            phase: PutPhase::Finalized,
            version: version_id(0x81),
            revision: revision(8),
            seq: root_seq(11),
            tx: tx_id(0x71),
            digest: digest(OLD_HEX),
            size: 9,
        });
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("证据不一致");
        assert_eq!(failure.code(), "store_contract_violation");
        assert_eq!(failure.error_code(), Some(ErrorCode::NeedsRecovery));
        assert!(!failure.is_committed());
        assert!(!gate.is_held(request.root));
    }

    #[tokio::test]
    async fn short_write_after_publish_stays_prepared_and_is_never_committed() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        // 端口报告发布成功，但实测只有 3 字节：不能记 published，更不能 ACK。
        target.publish_reports = Some(crate::ports::target::PublishedTarget {
            identity: crate::ports::object_store::FileKey {
                device: 7,
                inode: 91,
                links: 1,
                bytes: 3,
            },
            digest: digest(NEW_HEX),
        });
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("短写");
        assert_eq!(failure.code(), "published_target_mismatch");
        assert_eq!(failure.phase, Some(PutPhase::Prepared));
        assert!(!failure.is_committed());
        assert!(store.writes.is_empty(), "发布证据不符时不得写元数据");
        assert!(!gate.is_held(request.root));
    }

    #[tokio::test]
    async fn external_write_between_the_two_observations_is_reconcile_required() {
        let (request, snapshot, mut gate, mut store, mut target) = overwrite();
        // 第一次（CAS 预检查）看到请求指纹，第二次（发布前 S034 复核）已被外部删除。
        target.observe_script.push_back(TargetObservation::Present {
            identity: snapshot.identity,
            digest: snapshot.digest,
        });
        target.observe_script.push_back(TargetObservation::Absent);
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("预检查后被外部改写");
        assert_eq!(failure.code(), "reconcile_required");
        assert_eq!(failure.error_code(), Some(ErrorCode::ReconcileRequired));
        assert!(!failure.is_committed());
        assert!(log.intents.is_empty(), "发布前的复核失败发生在写意图之前");
        assert!(target.publishes.is_empty());
        assert!(!gate.is_held(request.root));
    }

    #[tokio::test]
    async fn transaction_that_does_not_finalize_the_intent_is_not_acknowledged() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        store.record = Some(PutRecord {
            intent_id: request.intent_id,
            phase: PutPhase::Published,
            version: version_id(0x81),
            revision: revision(8),
            seq: root_seq(11),
            tx: tx_id(0x71),
            digest: digest(NEW_HEX),
            size: 5,
        });
        let mut log = FakeIntentLog::default();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("意图没有收尾");
        assert_eq!(failure.code(), "store_contract_violation");
        assert_eq!(failure.error_code(), Some(ErrorCode::NeedsRecovery));
        assert!(!failure.is_committed());
    }

    #[tokio::test]
    async fn request_shape_is_checked_before_any_port_is_touched() {
        let (mut request, _snapshot, mut gate, mut store, mut target) = overwrite();
        let mut log = FakeIntentLog::default();

        let mut escaping = request.clone();
        escaping.staging_path = "../escape".to_owned();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &escaping,
        )
        .await
        .expect_err("暂存名逃逸");
        assert_eq!(failure.code(), "invalid_request");
        assert!(store.trace.entries().is_empty(), "形状不成立时不得调用端口");
        assert!(!gate.is_held(request.root));

        request.target_path = "docs/other.txt".to_owned();
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("路径与名称不一致");
        assert_eq!(failure.code(), "invalid_request");
        assert!(store.trace.entries().is_empty());

        // 指纹记录的 revision 与请求基线不一致：同一请求里有两套基线。
        let (mut request, _snapshot, mut gate, mut store, mut target) = overwrite();
        request.expected_revision = revision(6);
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("两套基线");
        assert_eq!(failure.code(), "invalid_request");
        assert!(target.publishes.is_empty());
    }

    #[tokio::test]
    async fn one_root_commits_at_a_time_while_other_roots_proceed() {
        let (request, _snapshot, mut gate, mut store, mut target) = overwrite();
        let mut log = FakeIntentLog::default();

        let held = gate.acquire(request.root).expect("占住该 Root");
        let failure = commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect_err("同 Root 并发提交");
        assert_eq!(failure.code(), "root_commit_busy");
        assert_eq!(failure.error_code(), None);
        assert!(store.trace.entries().is_empty(), "未拿到 Root 票据不得读库");
        assert!(gate.is_held(request.root), "别人的票据不被顺手释放");

        // 另一个 Root 不被阻塞。
        let other = gate.acquire(root_id(0x22)).expect("另一个 Root 独立");
        gate.release(other);

        gate.release(held);
        commit_put(
            &mut gate,
            &mut store,
            &mut log,
            &mut target,
            user_time(100),
            &request,
        )
        .await
        .expect("释放后可以提交");
        assert!(!gate.is_held(request.root));
    }

    #[test]
    fn gate_tickets_are_unique_per_acquisition() {
        let mut gate = RootCommitGate::new();
        let root = root_id(0x21);
        let first = gate.acquire(root).expect("首次占用");
        assert_eq!(first.root(), root);
        assert!(matches!(
            gate.acquire(root),
            Err(CommitGateError::Busy { root: busy, serial }) if busy == root && serial == first.serial()
        ));
        gate.release(first);
        let second = gate.acquire(root).expect("释放后可再占用");
        assert!(second.serial() > first.serial());
    }

    #[test]
    fn entry_baseline_defaults_describe_a_file() {
        // 测试夹具自身的不变量：改动夹具时不至于悄悄换成目录或别的 revision。
        let baseline = file_baseline(root_id(0x21));
        assert_eq!(baseline.entry, entry_id(0x51));
        assert_eq!(baseline.revision, revision(7));
        assert!(baseline.kind.is_file());
    }
}
