//! 服务端核心测试共享的假端口与身份构造（仅 `cfg(test)` 编译）。
//!
//! 假端口只回答“被问了什么、按剧本答什么”，业务判定全部留给被测代码；调用顺序记录在
//! [`Trace`] 里，用来断言“先持久意图再发布”这类顺序不变量。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use vaultsyncd_domain::ids::{DeviceId, EntryId, ID_LEN, IntentId, OpId, RootId, TxId, VersionId};
use vaultsyncd_domain::version::{Revision, RootSeq, ServerEpoch, UtcTimestamp};

use crate::commit::conflict::CommittedConflict;
use crate::commit::intent::{NewPutIntent, PutIntent, PutPhase, TargetSnapshot};
use crate::objects::ContentDigest;
use crate::ports::conflict_store::{
    ConflictBegin, ConflictCommitStore, ConflictHead, ConflictRecord, ConflictStoreError,
    ConflictWrite, HeadSource, OperationKey, RecordedOperation, RequestDigest,
};
use crate::ports::intent_log::{IntentLog, IntentLogError};
use crate::ports::object_store::FileKey;
use crate::ports::put_store::{
    EntryBaseline, EntryKind, ObjectState, PutCommitStore, PutRecord, PutStoreError, PutWrite,
    RootFacts,
};
use crate::ports::target::{
    PublishRequest, PublishedTarget, TargetError, TargetObservation, TargetPort,
};

/// 多个假端口共享的调用轨迹。
#[derive(Clone, Default)]
pub(crate) struct Trace(Rc<RefCell<Vec<String>>>);

impl Trace {
    pub(crate) fn record(&self, line: impl Into<String>) {
        self.0.borrow_mut().push(line.into());
    }

    pub(crate) fn entries(&self) -> Vec<String> {
        self.0.borrow().clone()
    }
}

/// 共享同一条调用轨迹的意图日志与目标端口，用来断言跨端口的顺序。
pub(crate) fn linked_ports() -> (FakeIntentLog, FakeTarget) {
    let trace = Trace::default();
    (
        FakeIntentLog {
            trace: trace.clone(),
            ..FakeIntentLog::default()
        },
        FakeTarget {
            trace,
            ..FakeTarget::absent()
        },
    )
}

fn identity(marker: u8) -> [u8; ID_LEN] {
    [marker; ID_LEN]
}

pub(crate) fn intent_id(marker: u8) -> IntentId {
    IntentId::from_bytes(identity(marker))
}

pub(crate) fn root_id(marker: u8) -> RootId {
    RootId::from_bytes(identity(marker))
}

pub(crate) fn entry_id(marker: u8) -> EntryId {
    EntryId::from_bytes(identity(marker))
}

pub(crate) fn device_id(marker: u8) -> DeviceId {
    DeviceId::from_bytes(identity(marker))
}

pub(crate) fn op_id(marker: u8) -> OpId {
    OpId::from_bytes(identity(marker))
}

pub(crate) fn request_digest(marker: u8) -> RequestDigest {
    RequestDigest::from_bytes([marker; 32])
}

pub(crate) fn user_time(seconds: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_seconds(seconds).expect("测试用时间合法")
}

pub(crate) fn revision(value: u64) -> Revision {
    Revision::new(value).expect("测试用 revision 合法")
}

pub(crate) fn epoch(value: u64) -> ServerEpoch {
    ServerEpoch::new(value).expect("测试用 epoch 合法")
}

pub(crate) fn version_id(marker: u8) -> VersionId {
    VersionId::from_bytes(identity(marker))
}

pub(crate) fn tx_id(marker: u8) -> TxId {
    TxId::from_bytes(identity(marker))
}

pub(crate) fn root_seq(value: u64) -> RootSeq {
    RootSeq::new(value).expect("测试用 RootSeq 合法")
}

/// 测试用现场快照：设备 7、inode 41、单链接、revision 7、条目 0x51。
pub(crate) fn target_snapshot(hex: &str, size: u64) -> TargetSnapshot {
    TargetSnapshot {
        entry: entry_id(0x51),
        revision: 7,
        digest: ContentDigest::from_hex(hex).expect("测试用摘要合法"),
        identity: FileKey {
            device: 7,
            inode: 41,
            links: 1,
            bytes: size,
        },
    }
}

/// 内存意图日志：把阶段推进规则写成断言可读的记录。
#[derive(Default)]
pub(crate) struct FakeIntentLog {
    pub(crate) intents: Vec<PutIntent>,
    pub(crate) trace: Trace,
    pub(crate) begin_error: Option<IntentLogError>,
    pub(crate) advance_error: Option<IntentLogError>,
}

impl IntentLog for FakeIntentLog {
    async fn begin_put(
        &mut self,
        new: &NewPutIntent,
        _at: UtcTimestamp,
    ) -> Result<PutIntent, IntentLogError> {
        self.trace.record("intent begin prepared");
        if let Some(error) = self.begin_error.take() {
            return Err(error);
        }
        let intent = new.prepared();
        self.intents.push(intent.clone());
        Ok(intent)
    }

    async fn advance_put(
        &mut self,
        id: IntentId,
        next: PutPhase,
        _at: UtcTimestamp,
    ) -> Result<PutIntent, IntentLogError> {
        if let Some(error) = self.advance_error.take() {
            return Err(error);
        }
        let found = self
            .intents
            .iter_mut()
            .find(|intent| intent.intent_id == id)
            .ok_or(IntentLogError::UnknownIntent { id })?;
        if !found.phase.allows_transition_to(next) {
            return Err(IntentLogError::IllegalTransition {
                from: found.phase,
                to: next,
            });
        }
        found.phase = next;
        self.trace
            .record(format!("intent advance {}", next.as_text()));
        Ok(found.clone())
    }

    async fn load_put(&mut self, id: IntentId) -> Result<Option<PutIntent>, IntentLogError> {
        Ok(self
            .intents
            .iter()
            .find(|intent| intent.intent_id == id)
            .cloned())
    }

    async fn unfinished_puts(&mut self, root: RootId) -> Result<Vec<PutIntent>, IntentLogError> {
        Ok(self
            .intents
            .iter()
            .filter(|intent| intent.root == root && intent.phase.is_open())
            .cloned()
            .collect())
    }
}

/// 内存目标端口：可脚本化观察错误、发布错误与“名义成功但内容不符”。
pub(crate) struct FakeTarget {
    pub(crate) current: TargetObservation,
    pub(crate) trace: Trace,
    pub(crate) publishes: Vec<PublishRequest>,
    pub(crate) observe_error: Option<TargetError>,
    /// 依次替换后两次及以后 observe 的返回值，用来模拟“预检查之后、发布之前”的外部写入。
    pub(crate) observe_script: std::collections::VecDeque<TargetObservation>,
    pub(crate) publish_error: Option<TargetError>,
    pub(crate) publish_reports: Option<PublishedTarget>,
    next_inode: u64,
}

impl FakeTarget {
    pub(crate) fn absent() -> Self {
        FakeTarget {
            current: TargetObservation::Absent,
            trace: Trace::default(),
            publishes: Vec::new(),
            observe_error: None,
            observe_script: std::collections::VecDeque::new(),
            publish_error: None,
            publish_reports: None,
            next_inode: 90,
        }
    }

    /// 换用共享轨迹，便于断言跨端口的调用顺序。
    pub(crate) fn with_trace(mut self, trace: Trace) -> Self {
        self.trace = trace;
        self
    }

    /// 目标当前是一个已存在文件（现场快照 `snapshot`）。
    pub(crate) fn present(snapshot: &TargetSnapshot) -> Self {
        let mut target = FakeTarget::absent();
        target.current = TargetObservation::Present {
            identity: snapshot.identity,
            digest: snapshot.digest,
        };
        target.next_inode = snapshot.identity.inode + 1;
        target
    }
}

impl TargetPort for FakeTarget {
    fn observe(&mut self, target_path: &str) -> Result<TargetObservation, TargetError> {
        self.trace.record(format!("target observe {target_path}"));
        if let Some(error) = self.observe_error.take() {
            return Err(error);
        }
        if let Some(next) = self.observe_script.pop_front() {
            self.current = next;
        }
        Ok(self.current.clone())
    }

    fn publish(&mut self, request: &PublishRequest) -> Result<PublishedTarget, TargetError> {
        self.trace
            .record(format!("target publish {}", request.target_path));
        self.publishes.push(request.clone());
        if let Some(error) = self.publish_error.take() {
            return Err(error);
        }
        let published = match self.publish_reports.take() {
            Some(reported) => reported,
            None => {
                self.next_inode += 1;
                PublishedTarget {
                    identity: FileKey {
                        device: 7,
                        inode: self.next_inode,
                        links: 1,
                        bytes: request.size,
                    },
                    digest: request.digest,
                }
            }
        };
        self.current = TargetObservation::Present {
            identity: published.identity,
            digest: published.digest,
        };
        Ok(published)
    }
}

/// 内存提交存储：可脚本化每个读方法的错误与事务结果，并记录被写入的事务输入。
pub(crate) struct FakePutStore {
    pub(crate) trace: Trace,
    pub(crate) facts: RootFacts,
    pub(crate) baseline: EntryBaseline,
    pub(crate) object: ObjectState,
    pub(crate) root_facts_error: Option<PutStoreError>,
    pub(crate) baseline_error: Option<PutStoreError>,
    pub(crate) object_error: Option<PutStoreError>,
    pub(crate) apply_error: Option<PutStoreError>,
    pub(crate) writes: Vec<PutWrite>,
    pub(crate) record: Option<PutRecord>,
}

impl FakePutStore {
    /// 一个已存在普通文件的基线（revision 7、条目 0x51、名字 `note.txt`）。
    pub(crate) fn for_entry(baseline: EntryBaseline) -> Self {
        FakePutStore {
            trace: Trace::default(),
            facts: RootFacts { epoch: epoch(3) },
            baseline,
            object: ObjectState::Persisted { bytes: 5 },
            root_facts_error: None,
            baseline_error: None,
            object_error: None,
            apply_error: None,
            writes: Vec::new(),
            record: None,
        }
    }

    /// 默认的“成功事务”证据：由事务输入派生（revision +1、内容一致、阶段 finalized）。
    fn default_record(write: &PutWrite) -> PutRecord {
        PutRecord {
            intent_id: write.intent_id,
            phase: PutPhase::Finalized,
            version: version_id(0x81),
            revision: write
                .expected_revision
                .next()
                .expect("测试用 revision 可推进"),
            seq: root_seq(11),
            tx: tx_id(0x71),
            digest: write.content.digest,
            size: write.content.bytes,
        }
    }
}

impl PutCommitStore for FakePutStore {
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, PutStoreError> {
        self.trace.record(format!("store root_facts {root}"));
        match self.root_facts_error.take() {
            Some(error) => Err(error),
            None => Ok(self.facts),
        }
    }

    async fn entry_baseline(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<EntryBaseline, PutStoreError> {
        self.trace
            .record(format!("store entry_baseline {root} {entry}"));
        match self.baseline_error.take() {
            Some(error) => Err(error),
            None => Ok(self.baseline.clone()),
        }
    }

    async fn object_state(
        &mut self,
        root: RootId,
        digest: ContentDigest,
    ) -> Result<ObjectState, PutStoreError> {
        self.trace
            .record(format!("store object_state {root} {digest}"));
        match self.object_error.take() {
            Some(error) => Err(error),
            None => Ok(self.object),
        }
    }

    async fn apply_put(&mut self, write: &PutWrite) -> Result<PutRecord, PutStoreError> {
        self.trace.record("store apply_put".to_owned());
        self.writes.push(write.clone());
        match self.apply_error.take() {
            Some(error) => Err(error),
            None => Ok(self
                .record
                .clone()
                .unwrap_or_else(|| Self::default_record(write))),
        }
    }
}

/// 测试用条目基线：revision 7、父目录 0x61、名称 `note.txt`、普通文件。
pub(crate) fn file_baseline(root: RootId) -> EntryBaseline {
    EntryBaseline {
        entry: entry_id(0x51),
        root,
        parent: entry_id(0x61),
        name: "note.txt".to_owned(),
        revision: revision(7),
        kind: EntryKind::File,
    }
}

/// 测试用服务器 head 内容摘要（与请求内容不同）。
pub(crate) const HEAD_HEX: &str =
    "3333333333333333333333333333333333333333333333333333333333333333";

/// 内存冲突提交存储：脚本化每个读方法的错误、`begin_conflict` 的并发剧本与事务结果，并记录被写入的
/// 事务输入。成功的 `apply_conflict` 会把结果记进“已提交记录”，因此“先提交、再重试”在假端口上也
/// 走真实的重放路径。
pub(crate) struct FakeConflictStore {
    pub(crate) trace: Trace,
    pub(crate) head: ConflictHead,
    pub(crate) recorded: Vec<(OperationKey, RecordedOperation)>,
    pub(crate) head_error: Option<ConflictStoreError>,
    pub(crate) operation_error: Option<ConflictStoreError>,
    pub(crate) begin_error: Option<ConflictStoreError>,
    /// 依次替换 `begin_conflict` 的返回，用来模拟并发进程先一步登记或提交。
    pub(crate) begin_script: VecDeque<ConflictBegin>,
    pub(crate) apply_error: Option<ConflictStoreError>,
    pub(crate) writes: Vec<ConflictWrite>,
    pub(crate) record: Option<ConflictRecord>,
}

impl FakeConflictStore {
    /// 源条目 revision 已到 9（请求基线 7 已过期）、内容是 `HEAD_HEX` 的普通文件。
    pub(crate) fn for_source() -> Self {
        FakeConflictStore {
            trace: Trace::default(),
            head: ConflictHead {
                epoch: epoch(3),
                source: Some(HeadSource {
                    entry: entry_id(0x51),
                    revision: revision(9),
                    kind: EntryKind::File,
                    content: Some(ContentDigest::from_hex(HEAD_HEX).expect("测试用摘要合法")),
                }),
            },
            recorded: Vec::new(),
            head_error: None,
            operation_error: None,
            begin_error: None,
            begin_script: VecDeque::new(),
            apply_error: None,
            writes: Vec::new(),
            record: None,
        }
    }

    /// 让下一次 `operation` 返回一条已提交记录（同一 OpId、同一摘要）。
    pub(crate) fn record_result(
        &mut self,
        key: OperationKey,
        digest: RequestDigest,
        result: CommittedConflict,
    ) {
        self.recorded.push((
            key,
            RecordedOperation {
                digest,
                result: Some(result),
            },
        ));
    }

    fn lookup(&self, key: OperationKey) -> Option<RecordedOperation> {
        self.recorded
            .iter()
            .find(|(recorded, _)| *recorded == key)
            .map(|(_, record)| record.clone())
    }

    /// 默认的“成功事务”证据：由事务输入派生（副本身份/路径/内容一致、阶段 finalized）。
    fn default_record(write: &ConflictWrite) -> ConflictRecord {
        ConflictRecord {
            intent_id: write.intent_id,
            phase: PutPhase::Finalized,
            conflict: CommittedConflict {
                op: write.key.op,
                source: write.source,
                sibling: write.sibling,
                path: write.path.clone(),
                content: write.content,
                revision: revision(1),
                seq: root_seq(21),
            },
            tx: tx_id(0x72),
        }
    }
}

impl ConflictCommitStore for FakeConflictStore {
    async fn conflict_head(
        &mut self,
        root: RootId,
        source: EntryId,
    ) -> Result<ConflictHead, ConflictStoreError> {
        self.trace
            .record(format!("store conflict_head {root} {source}"));
        match self.head_error.take() {
            Some(error) => Err(error),
            None => Ok(self.head.clone()),
        }
    }

    async fn operation(
        &mut self,
        key: OperationKey,
    ) -> Result<Option<RecordedOperation>, ConflictStoreError> {
        self.trace.record(format!("store operation {}", key.op));
        match self.operation_error.take() {
            Some(error) => Err(error),
            None => Ok(self.lookup(key)),
        }
    }

    async fn begin_conflict(
        &mut self,
        key: OperationKey,
        digest: RequestDigest,
    ) -> Result<ConflictBegin, ConflictStoreError> {
        self.trace
            .record(format!("store begin_conflict {} {:?}", key.op, digest));
        if let Some(error) = self.begin_error.take() {
            return Err(error);
        }
        if let Some(scripted) = self.begin_script.pop_front() {
            return Ok(scripted);
        }
        if let Some(record) = self.lookup(key) {
            return Ok(ConflictBegin::Recorded(record));
        }
        self.recorded.push((
            key,
            RecordedOperation {
                digest,
                result: None,
            },
        ));
        Ok(ConflictBegin::Started)
    }

    async fn apply_conflict(
        &mut self,
        write: &ConflictWrite,
    ) -> Result<ConflictRecord, ConflictStoreError> {
        self.trace.record("store apply_conflict".to_owned());
        self.writes.push(write.clone());
        if let Some(error) = self.apply_error.take() {
            return Err(error);
        }
        let record = self
            .record
            .clone()
            .unwrap_or_else(|| Self::default_record(write));
        // 事务提交后的记录：pending 行原地变成终态，重试走重放而不是再造一份副本。
        let entry = (
            write.key,
            RecordedOperation {
                digest: write.digest,
                result: Some(record.conflict.clone()),
            },
        );
        match self
            .recorded
            .iter_mut()
            .find(|(recorded, _)| *recorded == write.key)
        {
            Some(slot) => *slot = entry,
            None => self.recorded.push(entry),
        }
        Ok(record)
    }
}

/// 计数式身份来源：从给定的字节开始逐个递增，让冲突条目身份在测试里可复现。
pub(crate) struct CountingIds {
    next: u8,
}

impl CountingIds {
    pub(crate) fn new(start: u8) -> Self {
        CountingIds { next: start }
    }
}

impl vaultsyncd_domain::ids::IdSource for CountingIds {
    fn next_id_bytes(&mut self) -> [u8; ID_LEN] {
        self.next += 1;
        [self.next; ID_LEN]
    }
}
