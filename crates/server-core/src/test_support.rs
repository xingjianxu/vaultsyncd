//! 服务端核心测试共享的假端口与身份构造（仅 `cfg(test)` 编译）。
//!
//! 假端口只回答“被问了什么、按剧本答什么”，业务判定全部留给被测代码；调用顺序记录在
//! [`Trace`] 里，用来断言“先持久意图再发布”这类顺序不变量。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use vaultsyncd_domain::ids::{
    DeviceId, EntryId, ID_LEN, IntentId, ObjectLeaseId, OpId, RootId, TxId, VersionId,
};
use vaultsyncd_domain::version::{Revision, RootSeq, ServerEpoch, UtcTimestamp};

use crate::commit::conflict::CommittedConflict;
use crate::commit::delete::{
    DeleteBatch, DeleteIntent, NewDeleteIntent, SubtreeEntry, SubtreeImage,
};
use crate::commit::intent::{NewPutIntent, PutIntent, PutPhase, TargetSnapshot};
use crate::commit::namespace::{NamespaceIntent, NewNamespaceIntent};
use crate::history::trash::CommittedTrashRestore;
use crate::history::version::CommittedVersionRestore;
use crate::objects::{ContentDigest, ExpectedContent};
use crate::ports::conflict_store::{
    ConflictBegin, ConflictCommitStore, ConflictHead, ConflictRecord, ConflictStoreError,
    ConflictWrite, HeadSource, OperationKey, RecordedOperation, RequestDigest,
};
use crate::ports::content_lease::{
    ContentLease, ContentLeaseError, ContentLeasePort, ContentLeaseRequest,
};
use crate::ports::intent_log::{DeleteIntentLog, IntentLog, IntentLogError, NamespaceIntentLog};
use crate::ports::namespace::{NamespaceError, NamespacePort, PathState};
use crate::ports::namespace_store::{
    NamespaceCommitStore, NamespaceRecord, NamespaceStoreError, NamespaceWrite,
};
use crate::ports::object_store::FileKey;
use crate::ports::put_store::{
    EntryBaseline, EntryKind, ObjectState, PutCommitStore, PutRecord, PutStoreError, PutWrite,
    RootFacts,
};
use crate::ports::removal::{RemovalError, RemovalPort, RemovalProgress};
use crate::ports::restore_store::{
    RecordedTrashRestore, RecordedVersionRestore, RestoreBegin, RestoreStore, RestoreStoreError,
    TrashEntryFacts, TrashRestoreRecord, TrashRestoreStore, TrashRestoreWrite, TrashSubtree,
    VersionFacts, VersionRestoreRecord, VersionRestoreWrite,
};
use crate::ports::target::{
    PublishRequest, PublishedTarget, TargetError, TargetObservation, TargetPort,
};
use crate::ports::trash_store::{DeleteApply, DeleteRecord, DeleteStore, TrashStoreError};

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
    /// 命名空间意图与 put 意图分开存：两者证据形态不同，混在一起会让断言失去意义。
    pub(crate) namespace_intents: Vec<NamespaceIntent>,
    /// 删除意图单独存：证据里多了子树 token 与条目数。
    pub(crate) delete_intents: Vec<DeleteIntent>,
    pub(crate) trace: Trace,
    pub(crate) begin_error: Option<IntentLogError>,
    pub(crate) advance_error: Option<IntentLogError>,
    pub(crate) namespace_begin_error: Option<IntentLogError>,
    pub(crate) namespace_advance_error: Option<IntentLogError>,
    pub(crate) delete_begin_error: Option<IntentLogError>,
    pub(crate) delete_advance_error: Option<IntentLogError>,
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

impl NamespaceIntentLog for FakeIntentLog {
    async fn begin_namespace(
        &mut self,
        new: &NewNamespaceIntent,
        _at: UtcTimestamp,
    ) -> Result<NamespaceIntent, IntentLogError> {
        self.trace.record("intent begin namespace prepared");
        if let Some(error) = self.namespace_begin_error.take() {
            return Err(error);
        }
        let intent = new.prepared();
        self.namespace_intents.push(intent.clone());
        Ok(intent)
    }

    async fn advance_namespace(
        &mut self,
        id: IntentId,
        next: PutPhase,
        _at: UtcTimestamp,
    ) -> Result<NamespaceIntent, IntentLogError> {
        if let Some(error) = self.namespace_advance_error.take() {
            return Err(error);
        }
        let found = self
            .namespace_intents
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

    async fn load_namespace(
        &mut self,
        id: IntentId,
    ) -> Result<Option<NamespaceIntent>, IntentLogError> {
        Ok(self
            .namespace_intents
            .iter()
            .find(|intent| intent.intent_id == id)
            .cloned())
    }

    async fn unfinished_namespaces(
        &mut self,
        root: RootId,
    ) -> Result<Vec<NamespaceIntent>, IntentLogError> {
        Ok(self
            .namespace_intents
            .iter()
            .filter(|intent| intent.root == root && intent.phase.is_open())
            .cloned()
            .collect())
    }
}

/// 内存目标端口：可脚本化观察错误、发布错误与“名义成功但内容不符”。
///
/// 现场按 **target_path** 分别记录：一次恢复会陆续发布多个文件，每个文件的目标各有自己的现场；
/// `set_default` 设定“从未被具体观察过的路径”的现场，`current` 只是最近一次观察/发布的结果（断言用）。
pub(crate) struct FakeTarget {
    default_state: TargetObservation,
    pub(crate) current: TargetObservation,
    /// 路径 → 最近一次实测/发布后的现场。
    pub(crate) states: std::collections::BTreeMap<String, TargetObservation>,
    pub(crate) trace: Trace,
    pub(crate) publishes: Vec<PublishRequest>,
    pub(crate) observe_error: Option<TargetError>,
    /// 依次替换后两次及以后 observe 的返回值，用来模拟“预检查之后、发布之前”的外部写入。
    pub(crate) observe_script: std::collections::VecDeque<TargetObservation>,
    pub(crate) publish_error: Option<TargetError>,
    /// 指定目标路径的下一次发布失败（模拟批量恢复中途的发布故障）。
    pub(crate) fail_publish_at: Option<(String, TargetError)>,
    pub(crate) publish_reports: Option<PublishedTarget>,
    next_inode: u64,
}

impl FakeTarget {
    pub(crate) fn absent() -> Self {
        FakeTarget {
            default_state: TargetObservation::Absent,
            current: TargetObservation::Absent,
            states: std::collections::BTreeMap::new(),
            trace: Trace::default(),
            publishes: Vec::new(),
            observe_error: None,
            observe_script: std::collections::VecDeque::new(),
            publish_error: None,
            fail_publish_at: None,
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
        target.set_default(TargetObservation::Present {
            identity: snapshot.identity,
            digest: snapshot.digest,
        });
        target.next_inode = snapshot.identity.inode + 1;
        target
    }

    /// 改变“从未被具体观察过的路径”的现场（模拟外部程序把目标改成了这个状态）。
    pub(crate) fn set_default(&mut self, state: TargetObservation) {
        self.default_state = state;
    }
}

impl TargetPort for FakeTarget {
    fn observe(&mut self, target_path: &str) -> Result<TargetObservation, TargetError> {
        self.trace.record(format!("target observe {target_path}"));
        if let Some(error) = self.observe_error.take() {
            return Err(error);
        }
        if let Some(next) = self.observe_script.pop_front() {
            self.states.insert(target_path.to_owned(), next);
        }
        let observed = self
            .states
            .get(target_path)
            .cloned()
            .unwrap_or_else(|| self.default_state.clone());
        self.current = observed.clone();
        Ok(observed)
    }

    fn publish(&mut self, request: &PublishRequest) -> Result<PublishedTarget, TargetError> {
        self.trace
            .record(format!("target publish {}", request.target_path));
        self.publishes.push(request.clone());
        if let Some(error) = self.publish_error.take() {
            return Err(error);
        }
        if let Some((path, error)) = self.fail_publish_at.take() {
            if path == request.target_path {
                return Err(error);
            }
            self.fail_publish_at = Some((path, error));
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
        let state = TargetObservation::Present {
            identity: published.identity,
            digest: published.digest,
        };
        self.states
            .insert(request.target_path.clone(), state.clone());
        self.current = state;
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

/// 内存命名空间端口：一条条路径的目录/文件表，外加可脚本化的观察与动作错误。
///
/// 只模拟 `mkdir`、`rename` 与观察：没有任何内容操作可用，因此“移动不搬内容”在假端口上不是靠计数，
/// 而是**没有那样的方法可调**（轨迹里也不会出现发布/对象调用）。
pub(crate) struct FakeNamespace {
    pub(crate) trace: Trace,
    /// 路径 → (种类, 身份)。
    pub(crate) paths: std::collections::BTreeMap<String, (EntryKind, FileKey)>,
    /// 这些路径的祖先被认为不存在。
    pub(crate) ancestor_missing: Vec<String>,
    pub(crate) inspect_error: Option<NamespaceError>,
    pub(crate) create_error: Option<NamespaceError>,
    pub(crate) rename_error: Option<NamespaceError>,
    /// 脚本化移除错误（`remove_file` 与 `remove_directory_batch` 共用）。
    pub(crate) remove_error: Option<RemovalError>,
    /// 脚本化“移除报告成功但什么都没删”（模拟撒谎的适配器）。
    pub(crate) removal_keeps_path: bool,
    /// 适配器自己更小的批次粒度（用来观察核心“循环到完成为止”）。
    pub(crate) removal_batch_limit: usize,
    /// 移除调用次数（文件按 1 次计）。
    pub(crate) removal_batches: usize,
    /// 脚本化 `rename` 的返回（模拟适配器报告错误现场）。
    pub(crate) rename_reports: Option<PathState>,
    /// 报告成功但把源留在原处（模拟被实现成复制的适配器）。
    pub(crate) rename_keeps_source: bool,
    pub(crate) renames: Vec<(String, String)>,
    next_inode: u64,
}

impl Default for FakeNamespace {
    fn default() -> Self {
        FakeNamespace {
            trace: Trace::default(),
            paths: std::collections::BTreeMap::new(),
            ancestor_missing: Vec::new(),
            inspect_error: None,
            create_error: None,
            rename_error: None,
            remove_error: None,
            removal_keeps_path: false,
            removal_batch_limit: 2,
            removal_batches: 0,
            rename_reports: None,
            rename_keeps_source: false,
            renames: Vec::new(),
            next_inode: 100,
        }
    }
}

impl FakeNamespace {
    pub(crate) fn new() -> Self {
        FakeNamespace::default()
    }

    pub(crate) fn with_trace(mut self, trace: Trace) -> Self {
        self.trace = trace;
        self
    }

    /// 放一个已存在的目录（inode 由 `next_inode` 递增分配）。
    pub(crate) fn with_directory(mut self, path: &str) -> Self {
        self.next_inode += 1;
        let identity = FileKey {
            device: 7,
            inode: self.next_inode,
            links: 2,
            bytes: 64,
        };
        self.paths
            .insert(path.to_owned(), (EntryKind::Directory, identity));
        self
    }

    /// 放一个已存在的普通文件。
    pub(crate) fn with_file(mut self, path: &str) -> Self {
        self.next_inode += 1;
        let identity = FileKey {
            device: 7,
            inode: self.next_inode,
            links: 1,
            bytes: 5,
        };
        self.paths
            .insert(path.to_owned(), (EntryKind::File, identity));
        self
    }

    /// 某路径现在的现场（测试断言用）。
    pub(crate) fn state_of(&self, path: &str) -> PathState {
        match self.paths.get(path) {
            Some((EntryKind::Directory, identity)) => PathState::Directory {
                identity: *identity,
            },
            Some((EntryKind::File, identity)) => PathState::File {
                identity: *identity,
            },
            None => PathState::Absent,
        }
    }

    fn state(&self, path: &str) -> PathState {
        if self.paths.contains_key(path) {
            return self.state_of(path);
        }
        let missing = self
            .ancestor_missing
            .iter()
            .any(|ancestor| path == ancestor || path.starts_with(&format!("{ancestor}/")));
        if missing {
            PathState::AncestorMissing
        } else {
            PathState::Absent
        }
    }
}

impl NamespacePort for FakeNamespace {
    fn inspect(&mut self, path: &str) -> Result<PathState, NamespaceError> {
        self.trace.record(format!("namespace inspect {path}"));
        if let Some(error) = self.inspect_error.take() {
            return Err(error);
        }
        Ok(self.state(path))
    }

    fn create_directory(&mut self, path: &str) -> Result<PathState, NamespaceError> {
        self.trace.record(format!("namespace mkdir {path}"));
        if let Some(error) = self.create_error.take() {
            return Err(error);
        }
        if !matches!(self.state(path), PathState::Absent) {
            return Err(NamespaceError::Storage {
                stage: "mkdir",
                failure: crate::ports::object_store::StorageFailure::Other,
                detail: "目标已存在".to_owned(),
            });
        }
        self.next_inode += 1;
        let identity = FileKey {
            device: 7,
            inode: self.next_inode,
            links: 2,
            bytes: 64,
        };
        self.paths
            .insert(path.to_owned(), (EntryKind::Directory, identity));
        Ok(PathState::Directory { identity })
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<PathState, NamespaceError> {
        self.trace.record(format!("namespace rename {from} {to}"));
        self.renames.push((from.to_owned(), to.to_owned()));
        if let Some(error) = self.rename_error.take() {
            return Err(error);
        }
        match self.paths.remove(from) {
            Some(entry) => {
                let state = match entry {
                    (EntryKind::Directory, identity) => PathState::Directory { identity },
                    (EntryKind::File, identity) => PathState::File { identity },
                };
                self.paths.insert(to.to_owned(), entry);
                if self.rename_keeps_source {
                    self.paths.insert(from.to_owned(), entry);
                }
                Ok(self.rename_reports.take().unwrap_or(state))
            }
            None => Err(NamespaceError::Storage {
                stage: "rename",
                failure: crate::ports::object_store::StorageFailure::Other,
                detail: format!("源位置 {from} 不存在"),
            }),
        }
    }
}

/// 内存命名空间存储：按条目应答基线，记录事务输入，成功的 `apply_namespace` 只推进条目 revision。
///
/// 假实现刻意**没有**内容版本写入路径：移动“不写版本、不改内容引用”在假端口上同样由类型保证，
/// 测试只需断言内容引用表在提交前后一致。
pub(crate) struct FakeNamespaceStore {
    pub(crate) trace: Trace,
    pub(crate) facts: RootFacts,
    pub(crate) baselines: std::collections::BTreeMap<EntryId, EntryBaseline>,
    pub(crate) content_versions: std::collections::BTreeMap<EntryId, VersionId>,
    pub(crate) root_facts_error: Option<NamespaceStoreError>,
    pub(crate) baseline_error: Option<NamespaceStoreError>,
    pub(crate) apply_error: Option<NamespaceStoreError>,
    pub(crate) writes: Vec<NamespaceWrite>,
    pub(crate) record: Option<NamespaceRecord>,
}

impl Default for FakeNamespaceStore {
    fn default() -> Self {
        FakeNamespaceStore {
            trace: Trace::default(),
            facts: RootFacts { epoch: epoch(3) },
            baselines: std::collections::BTreeMap::new(),
            content_versions: std::collections::BTreeMap::new(),
            root_facts_error: None,
            baseline_error: None,
            apply_error: None,
            writes: Vec::new(),
            record: None,
        }
    }
}

impl FakeNamespaceStore {
    pub(crate) fn new() -> Self {
        FakeNamespaceStore::default()
    }

    /// 加入一条条目基线；普通文件预置一个内容版本以观察“移动后引用不变”。
    pub(crate) fn with_baseline(mut self, baseline: EntryBaseline) -> Self {
        if baseline.kind == EntryKind::File {
            self.content_versions
                .insert(baseline.entry, version_id(0x91));
        }
        self.baselines.insert(baseline.entry, baseline);
        self
    }

    /// 新建目录默认证据：revision 0。
    fn default_record(write: &NamespaceWrite) -> NamespaceRecord {
        NamespaceRecord {
            intent_id: write.intent_id,
            phase: PutPhase::Finalized,
            entry: write.entry,
            revision: match write.expected_revision {
                Some(revision) => revision.next().expect("测试用 revision 可推进"),
                None => revision(0),
            },
            seq: root_seq(11),
            tx: tx_id(0x71),
        }
    }
}

impl NamespaceCommitStore for FakeNamespaceStore {
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, NamespaceStoreError> {
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
    ) -> Result<EntryBaseline, NamespaceStoreError> {
        self.trace
            .record(format!("store entry_baseline {root} {entry}"));
        if let Some(error) = self.baseline_error.take() {
            return Err(error);
        }
        self.baselines
            .get(&entry)
            .cloned()
            .ok_or(NamespaceStoreError::EntryMissing { entry })
    }

    async fn apply_namespace(
        &mut self,
        write: &NamespaceWrite,
    ) -> Result<NamespaceRecord, NamespaceStoreError> {
        self.trace.record("store apply_namespace".to_owned());
        self.writes.push(write.clone());
        if let Some(error) = self.apply_error.take() {
            return Err(error);
        }
        Ok(self
            .record
            .clone()
            .unwrap_or_else(|| Self::default_record(write)))
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

impl DeleteIntentLog for FakeIntentLog {
    async fn begin_delete(
        &mut self,
        new: &NewDeleteIntent,
        _at: UtcTimestamp,
    ) -> Result<DeleteIntent, IntentLogError> {
        self.trace.record("intent begin delete prepared");
        if let Some(error) = self.delete_begin_error.take() {
            return Err(error);
        }
        let intent = new.prepared();
        self.delete_intents.push(intent.clone());
        Ok(intent)
    }

    async fn advance_delete(
        &mut self,
        id: IntentId,
        next: PutPhase,
        _at: UtcTimestamp,
    ) -> Result<DeleteIntent, IntentLogError> {
        if let Some(error) = self.delete_advance_error.take() {
            return Err(error);
        }
        let found = self
            .delete_intents
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

    async fn load_delete(&mut self, id: IntentId) -> Result<Option<DeleteIntent>, IntentLogError> {
        Ok(self
            .delete_intents
            .iter()
            .find(|intent| intent.intent_id == id)
            .cloned())
    }

    async fn unfinished_deletes(
        &mut self,
        root: RootId,
    ) -> Result<Vec<DeleteIntent>, IntentLogError> {
        Ok(self
            .delete_intents
            .iter()
            .filter(|intent| intent.root == root && intent.phase.is_open())
            .cloned()
            .collect())
    }
}

impl RemovalPort for FakeNamespace {
    fn remove_file(&mut self, path: &str) -> Result<(), RemovalError> {
        self.trace.record(format!("namespace remove_file {path}"));
        self.removal_batches += 1;
        if let Some(error) = self.remove_error.take() {
            return Err(error);
        }
        match self.paths.get(path) {
            Some((EntryKind::File, _)) if !self.removal_keeps_path => {
                self.paths.remove(path);
                Ok(())
            }
            Some((EntryKind::Directory, _)) => Err(RemovalError::InvalidRequest {
                detail: format!("{path} 是目录"),
            }),
            Some(_) => Ok(()),
            None => Err(RemovalError::InvalidRequest {
                detail: format!("{path} 不存在"),
            }),
        }
    }

    fn remove_directory_batch(
        &mut self,
        path: &str,
        max_entries: usize,
    ) -> Result<RemovalProgress, RemovalError> {
        self.trace.record(format!(
            "namespace remove_directory {path} limit {max_entries}"
        ));
        self.removal_batches += 1;
        if let Some(error) = self.remove_error.take() {
            return Err(error);
        }
        if !matches!(self.state(path), PathState::Directory { .. }) {
            // 已经删完：重复调用是幂等的（删除任务可被打断后继续）。
            return Ok(RemovalProgress::finished(0));
        }
        if self.removal_keeps_path {
            // 脚本化“报告完成但什么都没删”：核心必须靠复核现场拦住它。
            return Ok(RemovalProgress::finished(0));
        }
        let limit = max_entries.min(self.removal_batch_limit);
        let prefix = format!("{path}/");
        let victims: Vec<String> = self
            .paths
            .keys()
            .filter(|candidate| candidate.starts_with(&prefix))
            .cloned()
            .collect();
        let mut removed = 0;
        for victim in victims.into_iter().take(limit) {
            self.paths.remove(&victim);
            removed += 1;
        }
        if self
            .paths
            .keys()
            .any(|candidate| candidate.starts_with(&prefix))
        {
            Ok(RemovalProgress::partial(removed))
        } else {
            self.paths.remove(path);
            Ok(RemovalProgress::finished(removed + 1))
        }
    }
}

/// 内存删除存储：按条目应答基线、按条目应答子树清单，记录事务输入。
///
/// 假实现刻意没有任何内容/版本写路径：删除“不改版本、不动内容对象”在假端口上同样由类型保证。
pub(crate) struct FakeDeleteStore {
    pub(crate) trace: Trace,
    pub(crate) facts: RootFacts,
    pub(crate) baselines: std::collections::BTreeMap<EntryId, EntryBaseline>,
    pub(crate) images: std::collections::BTreeMap<EntryId, SubtreeImage>,
    pub(crate) content_versions: std::collections::BTreeMap<EntryId, VersionId>,
    pub(crate) root_facts_error: Option<TrashStoreError>,
    pub(crate) baseline_error: Option<TrashStoreError>,
    pub(crate) image_error: Option<TrashStoreError>,
    pub(crate) apply_error: Option<TrashStoreError>,
    pub(crate) writes: Vec<DeleteApply>,
    pub(crate) record: Option<DeleteRecord>,
}

impl Default for FakeDeleteStore {
    fn default() -> Self {
        FakeDeleteStore {
            trace: Trace::default(),
            facts: RootFacts { epoch: epoch(3) },
            baselines: std::collections::BTreeMap::new(),
            images: std::collections::BTreeMap::new(),
            content_versions: std::collections::BTreeMap::new(),
            root_facts_error: None,
            baseline_error: None,
            image_error: None,
            apply_error: None,
            writes: Vec::new(),
            record: None,
        }
    }
}

impl FakeDeleteStore {
    pub(crate) fn new() -> Self {
        FakeDeleteStore::default()
    }

    /// 加入一份冻结清单，同时按清单根条目推出基线（revision/父目录/种类取清单里的值）。
    pub(crate) fn with_image(mut self, image: SubtreeImage) -> Self {
        let root = image
            .find(image.root())
            .expect("清单必定包含根条目")
            .clone();
        self.baselines.insert(
            root.entry,
            EntryBaseline {
                entry: root.entry,
                root: root_id(0x21),
                parent: root.parent.expect("子树根必须有父目录"),
                name: root.path.rsplit('/').next().expect("路径非空").to_owned(),
                revision: root.revision,
                kind: root.kind,
            },
        );
        if let Some(version) = root.content_version {
            self.content_versions.insert(root.entry, version);
        }
        self.images.insert(image.root(), image);
        self
    }

    fn default_record(apply: &DeleteApply) -> DeleteRecord {
        DeleteRecord {
            intent_id: apply.intent_id,
            phase: PutPhase::Finalized,
            root: apply.root,
            entry: apply.entry,
            trashed: apply.entries() as u64,
            seq: root_seq(12),
            tx: tx_id(0x72),
        }
    }
}

impl DeleteStore for FakeDeleteStore {
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, TrashStoreError> {
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
    ) -> Result<EntryBaseline, TrashStoreError> {
        self.trace
            .record(format!("store entry_baseline {root} {entry}"));
        if let Some(error) = self.baseline_error.take() {
            return Err(error);
        }
        self.baselines
            .get(&entry)
            .cloned()
            .ok_or(TrashStoreError::EntryMissing { entry })
    }

    async fn subtree_image(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<SubtreeImage, TrashStoreError> {
        self.trace
            .record(format!("store subtree_image {root} {entry}"));
        if let Some(error) = self.image_error.take() {
            return Err(error);
        }
        self.images
            .get(&entry)
            .cloned()
            .ok_or(TrashStoreError::EntryMissing { entry })
    }

    async fn apply_delete(&mut self, apply: &DeleteApply) -> Result<DeleteRecord, TrashStoreError> {
        self.trace.record("store apply_delete".to_owned());
        self.writes.push(apply.clone());
        if let Some(error) = self.apply_error.take() {
            return Err(error);
        }
        Ok(self
            .record
            .clone()
            .unwrap_or_else(|| Self::default_record(apply)))
    }
}

/// 测试用子树清单：`entries` 是（条目标记, 路径, 种类, revision）序列，第一条是子树根。
///
/// 父目录按路径前缀推出：`docs/notes.txt` 的父目录是清单里路径为 `docs` 的那一条；子树根的父目录
/// 固定为 `parent`。
pub(crate) fn subtree_image(
    root: EntryId,
    parent: EntryId,
    entries: &[(u8, &str, EntryKind, u64)],
) -> SubtreeImage {
    let items: Vec<SubtreeEntry> = entries
        .iter()
        .map(|(marker, path, kind, rev)| {
            let id = entry_id(*marker);
            let parent = if id == root {
                parent
            } else {
                let parent_path = path.rsplit_once('/').map_or("", |(head, _)| head);
                entries
                    .iter()
                    .find(|(_, candidate, _, _)| *candidate == parent_path)
                    .map(|(marker, _, _, _)| entry_id(*marker))
                    .expect("清单里的条目必须有父目录")
            };
            SubtreeEntry {
                entry: id,
                parent: Some(parent),
                path: (*path).to_owned(),
                kind: *kind,
                revision: revision(*rev),
                content_version: match kind {
                    EntryKind::File => Some(version_id(*marker)),
                    EntryKind::Directory => None,
                },
            }
        })
        .collect();
    SubtreeImage::new(root, items).expect("测试清单合法")
}

/// 只取清单条目身份（断言批次覆盖全树用）。
pub(crate) fn batch_entries(batches: &[DeleteBatch]) -> Vec<EntryId> {
    batches
        .iter()
        .flat_map(|batch| batch.entries.iter().map(|item| item.entry))
        .collect()
}

/// 内存内容租约：记录请求与解除，可脚本化错误。
#[derive(Default)]
pub(crate) struct FakeContentLease {
    pub(crate) trace: Trace,
    pub(crate) acquired: Vec<ContentLeaseRequest>,
    pub(crate) released: Vec<ObjectLeaseId>,
    pub(crate) acquire_error: Option<ContentLeaseError>,
    pub(crate) release_error: Option<ContentLeaseError>,
    next: u64,
}

impl FakeContentLease {
    pub(crate) fn new() -> Self {
        FakeContentLease::default()
    }

    pub(crate) fn with_trace(mut self, trace: Trace) -> Self {
        self.trace = trace;
        self
    }
}

impl ContentLeasePort for FakeContentLease {
    async fn acquire(
        &mut self,
        request: &ContentLeaseRequest,
    ) -> Result<ContentLease, ContentLeaseError> {
        self.trace.record("lease acquire".to_owned());
        if let Some(error) = self.acquire_error.take() {
            return Err(error);
        }
        self.acquired.push(*request);
        self.next += 1;
        Ok(ContentLease {
            id: ObjectLeaseId::from_bytes([self.next as u8; ID_LEN]),
            purpose: request.purpose,
            expires_at: request.expires_at,
        })
    }

    async fn release(&mut self, lease: &ContentLease) -> Result<(), ContentLeaseError> {
        self.trace.record("lease release".to_owned());
        if let Some(error) = self.release_error.take() {
            return Err(error);
        }
        self.released.push(lease.id);
        Ok(())
    }
}

/// 历史版本恢复的内存存储：可脚本化事实与错误，并记录事务输入。
pub(crate) struct FakeRestoreStore {
    pub(crate) trace: Trace,
    pub(crate) facts: RootFacts,
    pub(crate) version: Option<VersionFacts>,
    pub(crate) recorded: Vec<(OperationKey, RecordedVersionRestore)>,
    pub(crate) root_facts_error: Option<RestoreStoreError>,
    pub(crate) version_error: Option<RestoreStoreError>,
    pub(crate) apply_error: Option<RestoreStoreError>,
    pub(crate) writes: Vec<VersionRestoreWrite>,
    pub(crate) record: Option<VersionRestoreRecord>,
}

impl FakeRestoreStore {
    pub(crate) fn new(version: VersionFacts) -> Self {
        FakeRestoreStore {
            trace: Trace::default(),
            facts: RootFacts { epoch: epoch(3) },
            version: Some(version),
            recorded: Vec::new(),
            root_facts_error: None,
            version_error: None,
            apply_error: None,
            writes: Vec::new(),
            record: None,
        }
    }

    pub(crate) fn default_record(write: &VersionRestoreWrite) -> VersionRestoreRecord {
        VersionRestoreRecord {
            intent_id: write.intent_id,
            phase: PutPhase::Finalized,
            tx: tx_id(0x73),
            result: CommittedVersionRestore {
                op: write.key.op,
                root: write.key.root,
                entry: write.entry,
                version: version_id(0x82),
                restored_from: write.restored_from,
                content: write.content,
                revision: write
                    .expected_revision
                    .next()
                    .expect("测试用 revision 可推进"),
                seq: root_seq(13),
                path: write.path.clone(),
            },
        }
    }
}

impl RestoreStore for FakeRestoreStore {
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, RestoreStoreError> {
        self.trace.record(format!("store root_facts {root}"));
        match self.root_facts_error.take() {
            Some(error) => Err(error),
            None => Ok(self.facts),
        }
    }

    async fn operation(
        &mut self,
        key: OperationKey,
    ) -> Result<Option<RecordedVersionRestore>, RestoreStoreError> {
        self.trace.record("store operation".to_owned());
        Ok(self
            .recorded
            .iter()
            .find(|(recorded, _)| *recorded == key)
            .map(|(_, record)| record.clone()))
    }

    async fn begin_restore(
        &mut self,
        key: OperationKey,
        digest: RequestDigest,
    ) -> Result<RestoreBegin, RestoreStoreError> {
        self.trace.record("store begin_restore".to_owned());
        if self.recorded.iter().any(|(recorded, _)| *recorded == key) {
            return Ok(RestoreBegin::Recorded);
        }
        self.recorded.push((
            key,
            RecordedVersionRestore {
                digest,
                result: None,
            },
        ));
        Ok(RestoreBegin::Started)
    }

    async fn version_facts(
        &mut self,
        root: RootId,
        entry: EntryId,
        version: VersionId,
    ) -> Result<VersionFacts, RestoreStoreError> {
        self.trace
            .record(format!("store version_facts {root} {entry} {version}"));
        if let Some(error) = self.version_error.take() {
            return Err(error);
        }
        self.version
            .clone()
            .ok_or(RestoreStoreError::VersionMissing { version })
    }

    async fn apply_version_restore(
        &mut self,
        write: &VersionRestoreWrite,
    ) -> Result<VersionRestoreRecord, RestoreStoreError> {
        self.trace.record("store apply_version_restore".to_owned());
        self.writes.push(write.clone());
        if let Some(error) = self.apply_error.take() {
            return Err(error);
        }
        let record = self
            .record
            .clone()
            .unwrap_or_else(|| Self::default_record(write));
        let entry = (
            write.key,
            RecordedVersionRestore {
                digest: write.digest,
                result: Some(record.result.clone()),
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

/// 回收站恢复的内存存储：清单、占用、内容事实与事务输入。
pub(crate) struct FakeTrashRestoreStore {
    pub(crate) trace: Trace,
    pub(crate) facts: RootFacts,
    pub(crate) subtrees: std::collections::BTreeMap<EntryId, TrashSubtree>,
    pub(crate) parents: std::collections::BTreeMap<EntryId, EntryKind>,
    /// 名称 → 占用者；按（父目录, 名称）唯一。
    pub(crate) occupants: std::collections::BTreeMap<(EntryId, String), EntryId>,
    pub(crate) contents: std::collections::BTreeMap<VersionId, ExpectedContent>,
    pub(crate) recorded: Vec<(OperationKey, RecordedTrashRestore)>,
    pub(crate) root_facts_error: Option<RestoreStoreError>,
    pub(crate) subtree_error: Option<RestoreStoreError>,
    pub(crate) apply_error: Option<RestoreStoreError>,
    pub(crate) writes: Vec<TrashRestoreWrite>,
    pub(crate) record: Option<TrashRestoreRecord>,
}

impl Default for FakeTrashRestoreStore {
    fn default() -> Self {
        FakeTrashRestoreStore {
            trace: Trace::default(),
            facts: RootFacts { epoch: epoch(3) },
            subtrees: std::collections::BTreeMap::new(),
            parents: std::collections::BTreeMap::new(),
            occupants: std::collections::BTreeMap::new(),
            contents: std::collections::BTreeMap::new(),
            recorded: Vec::new(),
            root_facts_error: None,
            subtree_error: None,
            apply_error: None,
            writes: Vec::new(),
            record: None,
        }
    }
}

impl FakeTrashRestoreStore {
    pub(crate) fn new() -> Self {
        FakeTrashRestoreStore::default()
    }

    /// 放入一个回收站条目（`parent` 同时登记为已存在目录）。
    pub(crate) fn with_trashed(self, facts: TrashEntryFacts, parent: EntryId) -> Self {
        self.with_subtree(vec![facts], parent)
    }

    /// 放入一个多行回收站子树（根在第一位）；每行文件的内容版本先登记成 `digest(0x0d)`/11 字节，
    /// 需要区分多个文件时用 [`with_content`](Self::with_content) 覆盖。
    pub(crate) fn with_subtree(mut self, entries: Vec<TrashEntryFacts>, parent: EntryId) -> Self {
        self.parents.insert(parent, EntryKind::Directory);
        let root = entries.first().cloned().expect("清单至少要有根这一行");
        for facts in &entries {
            if let Some(version) = facts.content_version {
                self.contents
                    .entry(version)
                    .or_insert(ExpectedContent::new(digest(0x0d), 11));
            }
        }
        self.subtrees.insert(root.entry, TrashSubtree { entries });
        self
    }

    /// 指定某个内容版本的实际内容（不同文件不同摘要时用）。
    pub(crate) fn with_content(mut self, version: VersionId, content: ExpectedContent) -> Self {
        self.contents.insert(version, content);
        self
    }

    /// 让某个（父目录, 名称）被另一个活跃条目占用。
    pub(crate) fn occupy(&mut self, parent: EntryId, name: &str, occupant: EntryId) {
        self.occupants.insert((parent, name.to_owned()), occupant);
    }

    fn default_record(write: &TrashRestoreWrite) -> TrashRestoreRecord {
        TrashRestoreRecord {
            intents: write.intents.clone(),
            phase: PutPhase::Finalized,
            tx: tx_id(0x74),
            result: CommittedTrashRestore {
                op: write.key.op,
                root: write.key.root,
                entry: write.entry,
                path: write.path.clone(),
                name: write.name.clone(),
                renamed: write.renamed,
                revision: write
                    .expected_revision
                    .next()
                    .expect("测试用 revision 可推进"),
                seq: root_seq(14),
                entries: write.entries.len() as u64,
            },
        }
    }
}

impl TrashRestoreStore for FakeTrashRestoreStore {
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, RestoreStoreError> {
        self.trace.record(format!("store root_facts {root}"));
        match self.root_facts_error.take() {
            Some(error) => Err(error),
            None => Ok(self.facts),
        }
    }

    async fn operation(
        &mut self,
        key: OperationKey,
    ) -> Result<Option<RecordedTrashRestore>, RestoreStoreError> {
        self.trace.record("store operation".to_owned());
        Ok(self
            .recorded
            .iter()
            .find(|(recorded, _)| *recorded == key)
            .map(|(_, record)| record.clone()))
    }

    async fn begin_restore(
        &mut self,
        key: OperationKey,
        digest: RequestDigest,
    ) -> Result<RestoreBegin, RestoreStoreError> {
        self.trace.record("store begin_restore".to_owned());
        if self.recorded.iter().any(|(recorded, _)| *recorded == key) {
            return Ok(RestoreBegin::Recorded);
        }
        self.recorded.push((
            key,
            RecordedTrashRestore {
                digest,
                result: None,
            },
        ));
        Ok(RestoreBegin::Started)
    }

    async fn trash_subtree(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<Option<TrashSubtree>, RestoreStoreError> {
        self.trace
            .record(format!("store trash_subtree {root} {entry}"));
        if let Some(error) = self.subtree_error.take() {
            return Err(error);
        }
        Ok(self.subtrees.get(&entry).cloned())
    }

    async fn parent_kind(
        &mut self,
        root: RootId,
        parent: EntryId,
    ) -> Result<Option<EntryKind>, RestoreStoreError> {
        self.trace
            .record(format!("store parent_kind {root} {parent}"));
        Ok(self.parents.get(&parent).copied())
    }

    async fn name_occupant(
        &mut self,
        root: RootId,
        parent: EntryId,
        name: &str,
    ) -> Result<Option<EntryId>, RestoreStoreError> {
        self.trace
            .record(format!("store name_occupant {root} {parent} {name}"));
        Ok(self.occupants.get(&(parent, name.to_owned())).copied())
    }

    async fn content_facts(
        &mut self,
        root: RootId,
        version: VersionId,
    ) -> Result<Option<ExpectedContent>, RestoreStoreError> {
        self.trace
            .record(format!("store content_facts {root} {version}"));
        Ok(self.contents.get(&version).copied())
    }

    async fn apply_trash_restore(
        &mut self,
        write: &TrashRestoreWrite,
    ) -> Result<TrashRestoreRecord, RestoreStoreError> {
        self.trace.record("store apply_trash_restore".to_owned());
        self.writes.push(write.clone());
        if let Some(error) = self.apply_error.take() {
            return Err(error);
        }
        let record = self
            .record
            .clone()
            .unwrap_or_else(|| Self::default_record(write));
        let entry = (
            write.key,
            RecordedTrashRestore {
                digest: write.digest,
                result: Some(record.result.clone()),
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

/// 测试用内容摘要：`marker` 字节填满 32 字节。
pub(crate) fn digest(marker: u8) -> ContentDigest {
    ContentDigest::from_bytes([marker; 32])
}
