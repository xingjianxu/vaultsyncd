//! 测试用假端口（**只在 `cfg(test)` 编译**，不进入任何发布路径）。
//!
//! 这些替身只实现执行器与端口契约需要的行为：内存队列、内存意图、固定时钟、可记录的处理器。
//! 它们不读文件、不发网络、不建数据库，因此执行器的调度、意图顺序与失败保留可以在没有任何平台
//! 资源的条件下被完整验证。SQLite 适配器另用真实客户端库测试（见 `adapters::client_store`）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vaultsyncd_client_store::bindings::{self, NewBinding, SyncMode};
use vaultsyncd_client_store::db::Database;
use vaultsyncd_client_store::queue::{self, NewPendingOp};
use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{BindingId, EntryId, OpId};
use vaultsyncd_domain::version::{BoundError, Generation, ServerEpoch, UtcTimestamp};
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_protocol::generated::{OperationRequest, OperationResult};
use vaultsyncd_transfer::ports::{
    LocalErrorKind, PortError, PortFuture, SnapshotSource, UploadTarget, VersionedContent,
};

use crate::executor::{ActionFailure, ActionHandler, ActionOutcome};
use crate::ports::Ports;
use crate::ports::local::{
    ApplyReceipt, ApplyRequest, LocalCapabilities, LocalFacts, LocalPort, MoveRequest,
    ReleaseRequest, RemoveRequest,
};
use crate::ports::remote::RemotePort;
use crate::ports::state::{
    ActionIntent, IntentStore, LocalWriteKind, OperationStore, QueuedOperation, StoredIntent,
    StoredUploadResume, UploadResumeStore,
};

/// 组合起来的假状态：队列与意图共享同一份内存，模拟“同一个客户端库”。
#[derive(Clone, Default)]
pub struct FakeState {
    /// 待执行队列。
    pub operations: FakeOperationStore,
    /// 本地写入意图。
    pub intents: FakeIntentStore,
}

impl FakeState {
    /// 空状态。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置队列内容（按给定顺序即插入顺序）。
    pub fn queue(&mut self, ops: Vec<QueuedOperation>) {
        self.operations.set(ops);
    }
}

/// 内存待执行队列。
#[derive(Clone, Default)]
pub struct FakeOperationStore {
    inner: Arc<Mutex<Vec<QueuedOperation>>>,
}

impl FakeOperationStore {
    /// 覆盖队列内容。
    pub fn set(&self, ops: Vec<QueuedOperation>) {
        *self.inner.lock().expect("队列锁") = ops;
    }

    /// 当前队列（测试断言用）。
    ///
    /// # Errors
    ///
    /// 假端口不会失败；返回 `Result` 只为与端口签名一致。
    pub fn pending(&self) -> Result<Vec<QueuedOperation>, PortError> {
        Ok(self.inner.lock().expect("队列锁").clone())
    }
}

impl OperationStore for FakeOperationStore {
    fn pending_ops(
        &self,
        binding: BindingId,
    ) -> PortFuture<'_, Result<Vec<QueuedOperation>, PortError>> {
        Box::pin(async move {
            Ok(self
                .inner
                .lock()
                .expect("队列锁")
                .iter()
                .filter(|op| op.binding == binding)
                .cloned()
                .collect())
        })
    }

    fn record_failure(&self, op: OpId, code: ErrorCode) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let mut queue = self.inner.lock().expect("队列锁");
            match queue.iter_mut().find(|queued| queued.op == op) {
                Some(queued) => {
                    queued.attempts = queued.attempts.saturating_add(1);
                    queued.last_error = Some(code);
                    Ok(())
                }
                None => Err(PortError::local(
                    LocalErrorKind::Missing,
                    format!("排队操作 {op} 不存在"),
                )),
            }
        })
    }

    fn complete(&self, op: OpId) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let mut queue = self.inner.lock().expect("队列锁");
            let before = queue.len();
            queue.retain(|queued| queued.op != op);
            if queue.len() == before {
                return Err(PortError::local(
                    LocalErrorKind::Missing,
                    format!("排队操作 {op} 不存在"),
                ));
            }
            Ok(())
        })
    }
}

/// 一条被假意图端口保存的意图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeIntentRow {
    /// 本地意图序号。
    pub id: i64,
    /// 意图内容。
    pub intent: ActionIntent,
    /// 是否已标记失败。
    pub failed: bool,
    /// 失败原因。
    pub last_error: Option<ErrorCode>,
}

impl FakeIntentRow {
    fn stored(&self) -> StoredIntent {
        StoredIntent {
            id: self.id,
            binding: self.intent.binding,
            kind: self.intent.kind,
            entry: self.intent.entry,
            target_path: self.intent.target_path.to_string(),
            source_path: self.intent.source_path.clone(),
            content_hash: self.intent.content_hash.map(|hash| hash.to_vec()),
            generation: Some(self.intent.generation),
            failed: self.failed,
            last_error: self.last_error,
        }
    }
}

/// 内存本地意图。
#[derive(Clone, Default)]
pub struct FakeIntentStore {
    inner: Arc<Mutex<Vec<FakeIntentRow>>>,
    next_id: Arc<Mutex<i64>>,
}

impl FakeIntentStore {
    /// 当前全部意图行（测试断言用）。
    #[must_use]
    pub fn all(&self) -> Vec<StoredIntent> {
        self.inner
            .lock()
            .expect("意图锁")
            .iter()
            .map(FakeIntentRow::stored)
            .collect()
    }

    /// 共享的原始行（供处理器观察“执行前意图已经存在”）。
    #[must_use]
    pub fn shared(&self) -> Arc<Mutex<Vec<FakeIntentRow>>> {
        Arc::clone(&self.inner)
    }
}

impl IntentStore for FakeIntentStore {
    fn begin_intent<'a>(
        &'a self,
        intent: &'a ActionIntent,
    ) -> PortFuture<'a, Result<i64, PortError>> {
        Box::pin(async move {
            let mut rows = self.inner.lock().expect("意图锁");
            if let Some(existing) = rows.iter().find(|row| {
                row.intent.binding == intent.binding && row.intent.target_path == intent.target_path
            }) {
                // 幂等复用：同一路径同一期望返回同一条意图；期望不同则拒绝，不覆盖别人的意图。
                if existing.intent.kind == intent.kind
                    && existing.intent.entry == intent.entry
                    && existing.intent.source_path == intent.source_path
                    && existing.intent.content_hash == intent.content_hash
                    && existing.intent.generation == intent.generation
                {
                    return Ok(existing.id);
                }
                return Err(PortError::rejected(
                    "INVALID_REQUEST",
                    format!("路径 {} 已有另一条意图", intent.target_path),
                ));
            }
            let mut next = self.next_id.lock().expect("意图序号锁");
            *next += 1;
            let id = *next;
            rows.push(FakeIntentRow {
                id,
                intent: intent.clone(),
                failed: false,
                last_error: None,
            });
            Ok(id)
        })
    }

    fn fail_intent(&self, intent: i64, code: ErrorCode) -> PortFuture<'_, Result<bool, PortError>> {
        Box::pin(async move {
            let mut rows = self.inner.lock().expect("意图锁");
            match rows.iter_mut().find(|row| row.id == intent) {
                Some(row) => {
                    row.failed = true;
                    row.last_error = Some(code);
                    Ok(true)
                }
                None => Ok(false),
            }
        })
    }

    fn complete_intent(&self, intent: i64) -> PortFuture<'_, Result<bool, PortError>> {
        Box::pin(async move {
            let mut rows = self.inner.lock().expect("意图锁");
            let before = rows.len();
            rows.retain(|row| row.id != intent);
            Ok(rows.len() != before)
        })
    }

    fn pending_intents(
        &self,
        binding: BindingId,
    ) -> PortFuture<'_, Result<Vec<StoredIntent>, PortError>> {
        Box::pin(async move {
            Ok(self
                .inner
                .lock()
                .expect("意图锁")
                .iter()
                .filter(|row| row.intent.binding == binding)
                .map(FakeIntentRow::stored)
                .collect())
        })
    }
}

/// 可推进的假时钟（秒）。
#[derive(Clone)]
pub struct FakeClock {
    now: Arc<Mutex<i64>>,
}

impl FakeClock {
    /// 从给定 Unix 秒开始。
    #[must_use]
    pub fn at(unix_seconds: i64) -> Self {
        Self {
            now: Arc::new(Mutex::new(unix_seconds)),
        }
    }

    /// 前进一段时间。
    pub fn advance(&self, delta: Duration) {
        let mut now = self.now.lock().expect("时钟锁");
        *now = now.saturating_add(i64::try_from(delta.as_secs()).unwrap_or(i64::MAX));
    }
}

impl vaultsyncd_domain::version::Clock for FakeClock {
    fn now_utc(&self) -> Result<UtcTimestamp, BoundError> {
        UtcTimestamp::from_unix_seconds(*self.now.lock().expect("时钟锁"))
    }
}

/// 假本地端口：没有真实文件，任何读写都明确返回“没有本地内容”。
#[derive(Clone, Copy, Debug, Default)]
pub struct FakeLocal;

impl FakeLocal {
    /// 具备全部能力的本地端口（执行器在 S055 里不调用它）。
    #[must_use]
    pub const fn full() -> Self {
        Self
    }

    fn unsupported(operation: &str) -> PortError {
        PortError::local(
            LocalErrorKind::Missing,
            format!("假本地端口没有实现 {operation}"),
        )
    }
}

fn fake_receipt() -> ApplyReceipt {
    ApplyReceipt {
        stage: crate::ports::local::ApplyStage::Applied,
        generation: Generation::new(1).expect("代际"),
        file_identity: None,
    }
}

impl LocalPort for FakeLocal {
    fn capabilities(&self) -> LocalCapabilities {
        LocalCapabilities::full()
    }

    fn observe(&self, _path: &RelativePath) -> PortFuture<'_, Result<LocalFacts, PortError>> {
        Box::pin(async move { Err(Self::unsupported("observe")) })
    }

    fn snapshot(
        &self,
        _path: &RelativePath,
        _generation: Generation,
    ) -> PortFuture<'_, Result<Arc<dyn SnapshotSource>, PortError>> {
        Box::pin(async move { Err(Self::unsupported("snapshot")) })
    }

    fn apply(&self, _request: &ApplyRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async move { Err(Self::unsupported("apply")) })
    }

    fn move_entry(
        &self,
        _request: &MoveRequest,
    ) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async move { Ok(fake_receipt()) })
    }

    fn remove(&self, _request: &RemoveRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async move { Ok(fake_receipt()) })
    }

    fn release(
        &self,
        _request: &ReleaseRequest,
    ) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async move { Ok(fake_receipt()) })
    }
}

/// 假远端端口：任何调用都明确报“未连接”，不伪造成功。
#[derive(Clone, Copy, Debug, Default)]
pub struct FakeRemote;

impl FakeRemote {
    /// 构造。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn offline() -> PortError {
        PortError::unavailable("假远端端口未连接", None)
    }
}

impl RemotePort for FakeRemote {
    fn submit(
        &self,
        _request: &OperationRequest,
    ) -> PortFuture<'_, Result<OperationResult, PortError>> {
        Box::pin(async move { Err(Self::offline()) })
    }

    fn operation(
        &self,
        _binding: BindingId,
        _op: OpId,
    ) -> PortFuture<'_, Result<OperationResult, PortError>> {
        Box::pin(async move { Err(Self::offline()) })
    }

    fn upload(
        &self,
        _binding: BindingId,
    ) -> PortFuture<'_, Result<Arc<dyn UploadTarget>, PortError>> {
        Box::pin(async move { Err(Self::offline()) })
    }

    fn content(
        &self,
        _binding: BindingId,
    ) -> PortFuture<'_, Result<Arc<dyn VersionedContent>, PortError>> {
        Box::pin(async move { Err(Self::offline()) })
    }
}

/// 内存上传续传端口。
#[derive(Clone, Default)]
pub struct FakeResumeStore {
    inner: Arc<Mutex<HashMap<[u8; 16], StoredUploadResume>>>,
}

impl FakeResumeStore {
    /// 构造空端口。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl UploadResumeStore for FakeResumeStore {
    fn load_resume(
        &self,
        op: OpId,
    ) -> PortFuture<'_, Result<Option<StoredUploadResume>, PortError>> {
        Box::pin(async move {
            Ok(self
                .inner
                .lock()
                .expect("续传锁")
                .get(op.as_bytes())
                .cloned())
        })
    }

    fn save_resume<'a>(
        &'a self,
        resume: &'a StoredUploadResume,
    ) -> PortFuture<'a, Result<StoredUploadResume, PortError>> {
        Box::pin(async move {
            self.inner
                .lock()
                .expect("续传锁")
                .insert(*resume.op.as_bytes(), resume.clone());
            Ok(resume.clone())
        })
    }

    fn discard_resume(&self, op: OpId) -> PortFuture<'_, Result<bool, PortError>> {
        Box::pin(async move {
            Ok(self
                .inner
                .lock()
                .expect("续传锁")
                .remove(op.as_bytes())
                .is_some())
        })
    }
}

/// 假端口组装：每个端口独立构造，再一次性放进 [`Ports`]。
pub struct FakePorts;

impl FakePorts {
    /// 组装全部端口。
    #[must_use]
    pub fn assemble(
        local: FakeLocal,
        clock: FakeClock,
        remote: FakeRemote,
        operations: FakeOperationStore,
        intents: FakeIntentStore,
        upload_resumes: FakeResumeStore,
    ) -> Ports {
        Ports::new(
            Arc::new(local),
            Arc::new(clock),
            Arc::new(remote),
            Arc::new(operations),
            Arc::new(intents),
            Arc::new(upload_resumes),
        )
    }
}

/// 构造一条测试意图（generation 7）。
#[must_use]
pub fn intent_for(
    op: OpId,
    binding: BindingId,
    kind: LocalWriteKind,
    target: &str,
) -> ActionIntent {
    ActionIntent {
        op,
        binding,
        kind,
        entry: Some(EntryId::from_bytes([0xEE; 16])),
        target_path: RelativePath::parse(target).expect("测试路径"),
        source_path: None,
        content_hash: Some([0x44; 32]),
        generation: Generation::new(7).expect("代际"),
    }
}

#[derive(Default)]
struct HandlerState {
    executed: Vec<OpId>,
    intents: HashMap<[u8; 16], Option<ActionIntent>>,
    failures: HashMap<[u8; 16], ActionFailure>,
    pendings: HashMap<[u8; 16], Option<Duration>>,
    saw_intent_before_execute: bool,
    seen_intents: Vec<ActionIntent>,
}

/// 可编排的假处理器：默认成功，可对指定操作注入失败/pending 与意图。
#[derive(Clone, Default)]
pub struct FakeHandler {
    state: Arc<Mutex<HandlerState>>,
    watched_intents: Option<Arc<Mutex<Vec<FakeIntentRow>>>>,
}

impl FakeHandler {
    /// 默认全部成功的处理器。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 观察意图表：执行时若表里已有行，记录“意图先于动作”。
    #[must_use]
    pub fn watch_intents(mut self, rows: Arc<Mutex<Vec<FakeIntentRow>>>) -> Self {
        self.watched_intents = Some(rows);
        self
    }

    /// 为某个操作编排执行前意图。
    #[must_use]
    pub fn intent_for(self, op: OpId, intent: Option<ActionIntent>) -> Self {
        self.state
            .lock()
            .expect("处理器锁")
            .intents
            .insert(*op.as_bytes(), intent);
        self
    }

    /// 为某个操作编排失败。
    #[must_use]
    pub fn fail(self, op: OpId, failure: ActionFailure) -> Self {
        self.state
            .lock()
            .expect("处理器锁")
            .failures
            .insert(*op.as_bytes(), failure);
        self
    }

    /// 为某个操作编排 pending。
    #[must_use]
    pub fn pending(self, op: OpId, poll_after: Option<Duration>) -> Self {
        self.state
            .lock()
            .expect("处理器锁")
            .pendings
            .insert(*op.as_bytes(), poll_after);
        self
    }

    /// 已执行的操作，按执行顺序。
    #[must_use]
    pub fn executed(&self) -> Vec<OpId> {
        self.state.lock().expect("处理器锁").executed.clone()
    }

    /// 执行时意图是否已经落库。
    #[must_use]
    pub fn saw_intent_before_execute(&self) -> bool {
        self.state
            .lock()
            .expect("处理器锁")
            .saw_intent_before_execute
    }

    /// 执行时看到的意图（按执行顺序）。
    #[must_use]
    pub fn seen_intents(&self) -> Vec<ActionIntent> {
        self.state.lock().expect("处理器锁").seen_intents.clone()
    }
}

impl ActionHandler for FakeHandler {
    fn plan_intent<'a>(
        &'a self,
        op: &'a QueuedOperation,
    ) -> PortFuture<'a, Result<Option<ActionIntent>, ActionFailure>> {
        Box::pin(async move {
            Ok(self
                .state
                .lock()
                .expect("处理器锁")
                .intents
                .get(op.op.as_bytes())
                .cloned()
                .flatten())
        })
    }

    fn execute<'a>(
        &'a self,
        _ports: &'a Ports,
        op: &'a QueuedOperation,
        intent: Option<&'a ActionIntent>,
    ) -> PortFuture<'a, Result<ActionOutcome, ActionFailure>> {
        Box::pin(async move {
            let mut state = self.state.lock().expect("处理器锁");
            state.executed.push(op.op);
            if let Some(intent) = intent {
                state.seen_intents.push(intent.clone());
            }
            if intent.is_some() {
                if let Some(rows) = &self.watched_intents {
                    if !rows.lock().expect("意图锁").is_empty() {
                        state.saw_intent_before_execute = true;
                    }
                }
            }
            if let Some(failure) = state.failures.get(op.op.as_bytes()).cloned() {
                return Err(failure);
            }
            if let Some(poll_after) = state.pendings.get(op.op.as_bytes()).copied() {
                return Ok(ActionOutcome::Pending { poll_after });
            }
            Ok(ActionOutcome::Completed)
        })
    }
}

/// 测试独占的临时目录；`Drop` 时连同 WAL 与迁移前备份一起删除。
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    /// 新建一个以标签区分的临时目录。
    #[must_use]
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vaultsyncd-s055-{}-{label}-{index}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    /// 该目录下的客户端库路径。
    #[must_use]
    pub fn database(&self) -> PathBuf {
        self.path.join("client.sqlite")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 打开（必要时建库并迁移到当前 schema）测试客户端库。
pub async fn open_database(dir: &TestDir) -> Database {
    Database::open(dir.database()).await.expect("打开测试库")
}

/// 固定字节的操作身份，便于断言里看出是哪一个。
#[must_use]
pub fn test_op_id(marker: u8) -> OpId {
    OpId::from_bytes([marker; 16])
}

/// 建一个测试绑定并提交，返回它的身份。
pub async fn seed_binding(database: &Database, marker: u8) -> BindingId {
    let id = BindingId::from_bytes([marker; 16]);
    let mut tx = database.begin_write().await.expect("开始写事务");
    bindings::insert_binding(
        &mut tx,
        &NewBinding {
            id,
            server: vaultsyncd_domain::ids::ServerId::from_bytes([0x01; 16]),
            device: vaultsyncd_domain::ids::DeviceId::from_bytes([0x02; 16]),
            vault: vaultsyncd_domain::ids::VaultId::from_bytes([0x03; 16]),
            local_path: format!(r"C:\vaultsyncd-test-{marker:02x}"),
            cache_path: format!(r"C:\vaultsyncd-test-{marker:02x}\.cache"),
            mode: SyncMode::OnDemand,
            rules_version: "rules-v1".to_owned(),
            server_epoch: ServerEpoch::new(4).expect("epoch"),
        },
    )
    .await
    .expect("写测试绑定");
    tx.commit().await.expect("提交测试绑定");
    id
}

/// 入队一条最小操作（无依赖、无基线），供需要 `pending_ops` 的适配器测试使用。
pub async fn enqueue_operation(database: &Database, binding: BindingId, op: OpId) {
    let mut tx = database.begin_write().await.expect("开始写事务");
    queue::enqueue(
        &mut tx,
        &NewPendingOp {
            op,
            binding,
            kind: "upload_content".to_owned(),
            entry: None,
            baseline_revision: None,
            baseline_version: None,
            depends_on: None,
            pinned_version: None,
        },
    )
    .await
    .expect("入队测试操作");
    tx.commit().await.expect("提交测试操作");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fakes_are_independent_ports_without_construction_cycles() {
        // 每个端口都能单独构造，再任意顺序组装：端口之间没有“构造 A 需要 B”的依赖。
        let local = FakeLocal::full();
        let clock = FakeClock::at(1);
        let remote = FakeRemote::new();
        let operations = FakeOperationStore::default();
        let intents = FakeIntentStore::default();
        let resumes = FakeResumeStore::new();
        let ports = FakePorts::assemble(local, clock, remote, operations, intents, resumes);
        assert!(ports.local.capabilities().on_demand_read);
        assert!(ports.clock.now_utc().is_ok());
        assert!(ports.remote.submit(&dummy_request()).await.is_err());
        assert!(ports.operations.pending_ops(dummy_binding()).await.is_ok());
        assert!(ports.intents.pending_intents(dummy_binding()).await.is_ok());
        assert!(ports.upload_resumes.load_resume(dummy_op()).await.is_ok());
    }

    fn dummy_binding() -> BindingId {
        BindingId::from_bytes([0x10; 16])
    }

    fn dummy_op() -> OpId {
        OpId::from_bytes([0x11; 16])
    }

    fn dummy_request() -> OperationRequest {
        OperationRequest {
            base_revision: None,
            base_version_id: None,
            conflict_policy: None,
            device_id: "11".repeat(16),
            entry_id: None,
            expected_absent: Some(true),
            kind: vaultsyncd_protocol::generated::OperationKind::Create,
            local_generation: None,
            name: Some("report.txt".to_owned()),
            op_id: "00".repeat(16),
            parent_id: Some("22".repeat(16)),
            server_epoch: "1".to_owned(),
            subtree_token: None,
            upload_id: None,
            version_id: None,
        }
    }
}
