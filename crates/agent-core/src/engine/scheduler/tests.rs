use super::*;
use crate::{
    adapters::ClientStorePorts,
    executor::{ActionFailure, ActionHandler},
    ports::{
        PortFuture,
        state::{ActionIntent, LocalWriteKind},
    },
    testing::{FakeClock, FakeLocal, FakeRemote, TestDir, seed_binding},
};
use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::Waker,
};
use vaultsyncd_client_store::queue::{self, NewPendingOp};
use vaultsyncd_domain::{ids::VersionId, version::Generation};
use vaultsyncd_fs_common::path::RelativePath;

struct Samples(AtomicU64);
impl Jitter for Samples {
    fn sample(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}
#[derive(Default)]
struct Gate {
    closed: AtomicBool,
}
impl DispatchGate for Gate {
    fn allowed<'a>(&'a self, _: &'a QueuedOperation) -> PortFuture<'a, Result<bool, PortError>> {
        Box::pin(async move { Ok(!self.closed.load(Ordering::SeqCst)) })
    }
}
#[derive(Clone)]
enum Behavior {
    Complete,
    Wait,
    CommittedWait,
    Pending,
    Fail(TaskError),
}
#[derive(Default)]
struct HandlerState {
    calls: Mutex<Vec<OpId>>,
    behavior: Mutex<HashMap<OpId, Behavior>>,
    intents: Mutex<HashSet<OpId>>,
    released: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
    gate_on_plan: Mutex<Option<Arc<Gate>>>,
}
#[derive(Clone, Default)]
struct Handler(Arc<HandlerState>);
impl Handler {
    fn set(&self, op: OpId, value: Behavior) {
        self.0.behavior.lock().unwrap().insert(op, value);
    }
    fn calls(&self) -> Vec<OpId> {
        self.0.calls.lock().unwrap().clone()
    }
    fn release(&self) {
        self.0.released.store(true, Ordering::SeqCst);
        for w in self.0.wakers.lock().unwrap().drain(..) {
            w.wake();
        }
    }
}
impl ActionHandler for Handler {
    fn plan_intent<'a>(
        &'a self,
        op: &'a QueuedOperation,
    ) -> PortFuture<'a, Result<Option<ActionIntent>, ActionFailure>> {
        Box::pin(async move {
            if let Some(gate) = self.0.gate_on_plan.lock().unwrap().as_ref() {
                gate.closed.store(true, Ordering::SeqCst);
            }
            Ok(self
                .0
                .intents
                .lock()
                .unwrap()
                .contains(&op.op)
                .then(|| ActionIntent {
                    op: op.op,
                    binding: op.binding,
                    kind: LocalWriteKind::Replace,
                    entry: op.entry,
                    target_path: RelativePath::parse(&format!("file-{}.txt", op.op)).unwrap(),
                    source_path: None,
                    content_hash: None,
                    generation: Generation::new(7).unwrap(),
                }))
        })
    }
    fn execute<'a>(
        &'a self,
        _: &'a Ports,
        _: &'a QueuedOperation,
        _: Option<&'a ActionIntent>,
    ) -> PortFuture<'a, Result<ActionOutcome, ActionFailure>> {
        Box::pin(async { panic!("调度器应调用支持取消的入口") })
    }
}
impl ScheduledHandler for Handler {
    fn execute_scheduled<'a>(
        &'a self,
        ports: &'a Ports,
        op: &'a QueuedOperation,
        intent: Option<&'a ActionIntent>,
        cancel: &'a Cancellation,
    ) -> PortFuture<'a, Result<ActionOutcome, TaskError>> {
        Box::pin(async move {
            if intent.is_some() {
                assert!(
                    ports
                        .intents
                        .pending_intents(op.binding)
                        .await
                        .unwrap()
                        .iter()
                        .any(|i| i.entry == op.entry)
                );
            }
            self.0.calls.lock().unwrap().push(op.op);
            let behavior = self
                .0
                .behavior
                .lock()
                .unwrap()
                .get(&op.op)
                .cloned()
                .unwrap_or(Behavior::Complete);
            if matches!(behavior, Behavior::Wait | Behavior::CommittedWait) {
                poll_fn(|cx| {
                    if self.0.released.load(Ordering::SeqCst) {
                        Poll::Ready(())
                    } else {
                        self.0.wakers.lock().unwrap().push(cx.waker().clone());
                        Poll::Pending
                    }
                })
                .await;
                if cancel.is_cancelled() && matches!(behavior, Behavior::Wait) {
                    return Err(TaskError::Cancelled);
                }
            }
            match behavior {
                Behavior::Fail(error) => Err(error),
                Behavior::Pending => Ok(ActionOutcome::Pending {
                    poll_after: Some(Duration::ZERO),
                }),
                _ => Ok(ActionOutcome::Completed),
            }
        })
    }
}
struct Rig {
    dir: TestDir,
    db: Arc<Database>,
    binding: BindingId,
    clock: FakeClock,
    handler: Handler,
    gate: Arc<Gate>,
}
impl Rig {
    async fn new() -> Self {
        let dir = TestDir::new("s062");
        let db = Arc::new(Database::open(dir.database()).await.unwrap());
        let binding = seed_binding(&db, 0x62).await;
        Self {
            dir,
            db,
            binding,
            clock: FakeClock::at(1000),
            handler: Handler::default(),
            gate: Arc::default(),
        }
    }
    fn scheduler(&self) -> Scheduler<Handler> {
        build_scheduler(
            self.db.clone(),
            self.clock.clone(),
            self.handler.clone(),
            self.gate.clone(),
        )
    }
    async fn enqueue(&self, marker: u8, kind: &str, entry: u8, dependency: Option<OpId>) -> OpId {
        let op = OpId::from_bytes([marker; 16]);
        let mut tx = self.db.begin_write().await.unwrap();
        queue::enqueue(
            &mut tx,
            &NewPendingOp {
                op,
                binding: self.binding,
                kind: kind.into(),
                entry: Some(EntryId::from_bytes([entry; 16])),
                baseline_revision: None,
                baseline_version: None,
                depends_on: dependency,
                pinned_version: Some(VersionId::from_bytes([marker; 16])),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        op
    }
}
fn build_scheduler(
    db: Arc<Database>,
    clock: FakeClock,
    handler: Handler,
    gate: Arc<Gate>,
) -> Scheduler<Handler> {
    let state = Arc::new(ClientStorePorts::new(db.clone()));
    let ports = Ports::new(
        Arc::new(FakeLocal),
        Arc::new(clock),
        Arc::new(FakeRemote),
        state.clone(),
        state.clone(),
        state,
    );
    Scheduler::new(
        db,
        ports,
        handler,
        gate,
        Arc::new(Samples(AtomicU64::new(0))),
        Limits {
            content: 2,
            metadata: 1,
            hash: 2,
        },
    )
    .unwrap()
}
fn failure(code: ErrorCode) -> TaskError {
    TaskError::Failed {
        failure: ActionFailure::new(code, "不输出敏感诊断"),
        retry_after: None,
        offline: code == ErrorCode::PlaceholderOffline,
    }
}

#[tokio::test]
async fn read_precedes_large_background_and_metadata_has_its_own_slot() {
    let rig = Rig::new().await;
    let scheduler = rig.scheduler();
    let background = rig.enqueue(1, "apply_remote", 1, None).await;
    let open = rig.enqueue(2, "apply_remote", 2, None).await;
    let metadata = rig.enqueue(3, "merge_baseline", 3, None).await;
    scheduler
        .prioritize(open, Priority::UserRead)
        .await
        .unwrap();
    assert!(
        scheduler
            .prioritize(background, Priority::Metadata)
            .await
            .is_err()
    );
    assert!(
        scheduler
            .prioritize(metadata, Priority::UserRead)
            .await
            .is_err()
    );
    let report = scheduler.run_round(rig.binding).await.unwrap();
    let calls = rig.handler.calls();
    assert_eq!(calls.len(), 3);
    for op in [open, metadata, background] {
        assert!(calls.contains(&op));
    }
    assert_eq!(report.tasks.len(), 3);
    assert!(scheduler.status(rig.binding).await.unwrap().is_empty());
}

#[tokio::test]
async fn user_read_and_metadata_start_while_background_is_still_running() {
    let rig = Rig::new().await;
    let scheduler = rig.scheduler();
    let background = rig.enqueue(1, "apply_remote", 1, None).await;
    let waiting = rig.enqueue(2, "apply_remote", 2, None).await;
    rig.handler.set(background, Behavior::Wait);
    rig.handler.set(waiting, Behavior::Wait);
    let first = scheduler.run_round(rig.binding);
    tokio::pin!(first);
    poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        if rig.handler.calls().contains(&background) {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
    let open = rig.enqueue(3, "apply_remote", 3, None).await;
    scheduler
        .prioritize(open, Priority::UserRead)
        .await
        .unwrap();
    let metadata = rig.enqueue(4, "merge_baseline", 4, None).await;
    let second = scheduler.run_round(rig.binding).await.unwrap();
    assert!(second.tasks.contains(&(open, Outcome::Completed)));
    assert!(second.tasks.contains(&(metadata, Outcome::Completed)));
    assert!(
        !rig.handler.calls().contains(&waiting),
        "后台不能侵占用户保留槽"
    );
    rig.handler.release();
    first.await.unwrap();
}

#[tokio::test]
async fn same_entry_serializes_even_user_priority_and_hash_is_bounded() {
    let rig = Rig::new().await;
    let scheduler = rig.scheduler();
    let first = rig.enqueue(1, "apply_remote", 1, None).await;
    let same = rig.enqueue(2, "merge_baseline", 1, None).await;
    let hash1 = rig.enqueue(3, "hash_content", 3, None).await;
    let hash2 = rig.enqueue(4, "hash_content", 4, None).await;
    let hash3 = rig.enqueue(5, "hash_content", 5, None).await;
    rig.handler.set(first, Behavior::Wait);
    rig.handler.set(same, Behavior::Wait);
    for op in [hash1, hash2, hash3] {
        rig.handler.set(op, Behavior::Wait);
    }
    let round = scheduler.run_round(rig.binding);
    tokio::pin!(round);
    poll_fn(|cx| {
        assert!(round.as_mut().poll(cx).is_pending());
        if rig.handler.calls().len() == 3 {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
    let calls = rig.handler.calls();
    assert!(calls.contains(&same));
    assert!(!calls.contains(&first));
    assert!(calls.contains(&hash1));
    assert!(calls.contains(&hash2));
    assert!(!calls.contains(&hash3));
    assert!(
        scheduler
            .run_round(rig.binding)
            .await
            .unwrap()
            .tasks
            .is_empty()
    );
    rig.handler.release();
    round.await.unwrap();
}

#[tokio::test]
async fn retry_after_persists_with_attempts_offline_and_no_busy_loop_after_reopen() {
    let rig = Rig::new().await;
    let op = rig.enqueue(1, "apply_remote", 1, None).await;
    rig.handler.set(
        op,
        Behavior::Fail(TaskError::Failed {
            failure: ActionFailure::new(ErrorCode::PlaceholderOffline, "offline"),
            retry_after: Some(Duration::from_millis(120_001)),
            offline: true,
        }),
    );
    let scheduler = rig.scheduler();
    scheduler.run_round(rig.binding).await.unwrap();
    let state = scheduler.status(rig.binding).await.unwrap().remove(0);
    assert_eq!(state.due.as_unix_seconds(), 1121);
    assert_eq!(state.attempts, 1);
    assert!(state.offline);
    assert!(!state.needs_attention());
    drop(scheduler);
    let db = Arc::new(Database::open(rig.dir.database()).await.unwrap());
    let restarted = build_scheduler(db, rig.clock.clone(), rig.handler.clone(), rig.gate.clone());
    for _ in 0..10 {
        assert!(
            restarted
                .run_round(rig.binding)
                .await
                .unwrap()
                .tasks
                .is_empty()
        );
    }
    assert_eq!(rig.handler.calls(), vec![op]);
    rig.clock.advance(Duration::from_secs(121));
    restarted.run_round(rig.binding).await.unwrap();
    assert_eq!(rig.handler.calls(), vec![op, op]);
    assert_eq!(restarted.status(rig.binding).await.unwrap()[0].attempts, 2);
}

#[tokio::test]
async fn jitter_is_bounded_and_retry_exhaustion_keeps_queue_and_version_pin() {
    let rig = Rig::new().await;
    let op = rig.enqueue(1, "apply_remote", 1, None).await;
    rig.handler
        .set(op, Behavior::Fail(failure(ErrorCode::RateLimited)));
    let scheduler = rig.scheduler();
    for attempt in 1..=3 {
        scheduler.run_round(rig.binding).await.unwrap();
        let state = scheduler.status(rig.binding).await.unwrap().remove(0);
        assert_eq!(state.attempts, attempt);
        let delay = state.due.as_unix_seconds()
            - crate::ports::clock::now(&rig.clock)
                .unwrap()
                .as_unix_seconds();
        let cap = 1_i64 << (attempt - 1);
        assert!((((cap + 1) / 2).max(1)..=cap).contains(&delay));
        rig.clock.advance(Duration::from_secs(60));
    }
    assert!(scheduler.status(rig.binding).await.unwrap()[0].halted);
    assert!(
        scheduler
            .run_round(rig.binding)
            .await
            .unwrap()
            .tasks
            .is_empty()
    );
    let mut conn = rig.db.pool().acquire().await.unwrap();
    assert_eq!(
        queue::pending_op(&mut conn, op)
            .await
            .unwrap()
            .unwrap()
            .pinned_version,
        Some(VersionId::from_bytes([1; 16]))
    );
    assert_eq!(rig.handler.calls(), vec![op, op, op]);
}

#[tokio::test]
async fn pending_intent_and_dependencies_survive_until_actual_commit() {
    let rig = Rig::new().await;
    let first = rig.enqueue(1, "apply_remote", 1, None).await;
    let child = rig.enqueue(2, "merge_baseline", 2, Some(first)).await;
    rig.handler.0.intents.lock().unwrap().insert(first);
    rig.handler.set(first, Behavior::Pending);
    let scheduler = rig.scheduler();
    let report = scheduler.run_round(rig.binding).await.unwrap();
    assert_eq!(report.tasks, vec![(first, Outcome::Pending)]);
    assert_eq!(scheduler.status(rig.binding).await.unwrap().len(), 2);
    assert_eq!(
        scheduler
            .ports
            .intents
            .pending_intents(rig.binding)
            .await
            .unwrap()
            .len(),
        1
    );
    rig.clock.advance(Duration::from_secs(1));
    rig.handler.set(first, Behavior::Complete);
    scheduler.run_round(rig.binding).await.unwrap();
    assert_eq!(scheduler.status(rig.binding).await.unwrap()[0].op, child);
    assert!(
        scheduler
            .ports
            .intents
            .pending_intents(rig.binding)
            .await
            .unwrap()
            .is_empty()
    );
    scheduler.run_round(rig.binding).await.unwrap();
    assert!(scheduler.status(rig.binding).await.unwrap().is_empty());
}

#[tokio::test]
async fn cancel_active_transfer_is_persisted_and_survives_restart() {
    let rig = Rig::new().await;
    let op = rig.enqueue(1, "apply_remote", 1, None).await;
    rig.handler.set(op, Behavior::Wait);
    let scheduler = rig.scheduler();
    let round = scheduler.run_round(rig.binding);
    tokio::pin!(round);
    poll_fn(|cx| {
        assert!(round.as_mut().poll(cx).is_pending());
        if rig.handler.calls().contains(&op) {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
    scheduler.cancel(op).await.unwrap();
    rig.handler.release();
    assert_eq!(round.await.unwrap().tasks, vec![(op, Outcome::Cancelled)]);
    let restarted = rig.scheduler();
    assert!(restarted.status(rig.binding).await.unwrap()[0].cancelled);
    assert!(
        restarted
            .run_round(rig.binding)
            .await
            .unwrap()
            .tasks
            .is_empty()
    );
    restarted.resume(op).await.unwrap();
    rig.handler.set(op, Behavior::Complete);
    restarted.run_round(rig.binding).await.unwrap();
    assert!(restarted.status(rig.binding).await.unwrap().is_empty());
}

#[tokio::test]
async fn gate_change_during_intent_planning_never_starts_platform_action() {
    let rig = Rig::new().await;
    let op = rig.enqueue(1, "apply_remote", 1, None).await;
    rig.handler.0.intents.lock().unwrap().insert(op);
    *rig.handler.0.gate_on_plan.lock().unwrap() = Some(rig.gate.clone());
    let scheduler = rig.scheduler();
    scheduler.run_round(rig.binding).await.unwrap();
    assert!(rig.handler.calls().is_empty());
    assert_eq!(scheduler.status(rig.binding).await.unwrap().len(), 1);
    assert_eq!(
        scheduler
            .ports
            .intents
            .pending_intents(rig.binding)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn full_disk_notifies_and_rechecks_without_deleting_content_or_history() {
    let rig = Rig::new().await;
    let op = rig.enqueue(1, "apply_remote", 1, None).await;
    rig.handler
        .set(op, Behavior::Fail(failure(ErrorCode::InsufficientStorage)));
    let scheduler = rig.scheduler();
    scheduler.run_round(rig.binding).await.unwrap();
    let status = scheduler.status(rig.binding).await.unwrap().remove(0);
    assert!(status.needs_attention());
    assert!(!status.halted);
    assert_eq!(status.due.as_unix_seconds(), 1060);
    rig.clock.advance(Duration::from_secs(60));
    rig.handler.set(op, Behavior::Complete);
    scheduler.run_round(rig.binding).await.unwrap();
    assert!(scheduler.status(rig.binding).await.unwrap().is_empty());
}

#[tokio::test]
async fn local_permission_rechecks_but_http_403_stops_without_retry() {
    let rig = Rig::new().await;
    let local = rig.enqueue(1, "apply_remote", 1, None).await;
    let remote = rig.enqueue(2, "apply_remote", 2, None).await;
    rig.handler.set(
        local,
        Behavior::Fail(TaskError::LocalPermission(ActionFailure::new(
            ErrorCode::AccessDenied,
            "local",
        ))),
    );
    rig.handler
        .set(remote, Behavior::Fail(failure(ErrorCode::AccessDenied)));
    let scheduler = rig.scheduler();
    scheduler.run_round(rig.binding).await.unwrap();
    scheduler.run_round(rig.binding).await.unwrap();
    let statuses = scheduler.status(rig.binding).await.unwrap();
    assert!(statuses.iter().all(TaskStatus::needs_attention));
    assert!(!statuses.iter().find(|s| s.op == local).unwrap().halted);
    assert!(statuses.iter().find(|s| s.op == remote).unwrap().halted);
}

// 独立进程直接exit，不依靠Drop/池close落盘。仅用本测试的临时SQLite/WAL和原OpId。
#[tokio::test]
async fn durable_scheduler_child() {
    let Ok(path) = std::env::var("VAULTSYNCD_S062_CHILD_DB") else {
        return;
    };
    let point = std::env::var("VAULTSYNCD_S062_CHILD_POINT").unwrap();
    let db = Arc::new(Database::open(path).await.unwrap());
    let handler = Handler::default();
    let op = OpId::from_bytes([1; 16]);
    let binding = BindingId::from_bytes([0x62; 16]);
    let scheduler = build_scheduler(db, FakeClock::at(1000), handler.clone(), Arc::default());
    match point.as_str() {
        "cancel" => {
            scheduler.cancel(op).await.unwrap();
            std::process::exit(93);
        }
        "failure" => {
            handler.set(op, Behavior::Fail(failure(ErrorCode::PlaceholderOffline)));
            scheduler.run_round(binding).await.unwrap();
            std::process::exit(92);
        }
        "pending" => {
            handler.0.intents.lock().unwrap().insert(op);
            handler.set(op, Behavior::Pending);
            scheduler.run_round(binding).await.unwrap();
            std::process::exit(94);
        }
        "inflight" => {
            handler.0.intents.lock().unwrap().insert(op);
            handler.set(op, Behavior::Wait);
            let round = scheduler.run_round(binding);
            tokio::pin!(round);
            poll_fn(|cx| {
                assert!(round.as_mut().poll(cx).is_pending());
                if handler.calls().contains(&op) {
                    std::process::exit(91);
                }
                cx.waker().wake_by_ref();
                Poll::<()>::Pending
            })
            .await;
        }
        _ => panic!("未知测试边界"),
    }
}

#[tokio::test]
async fn process_exit_at_inflight_failure_cancel_and_pending_never_completes_queue() {
    for (point, code) in [
        ("inflight", 91),
        ("failure", 92),
        ("cancel", 93),
        ("pending", 94),
    ] {
        let rig = Rig::new().await;
        let op = rig.enqueue(1, "apply_remote", 1, None).await;
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "engine::scheduler::tests::durable_scheduler_child",
                "--nocapture",
            ])
            .env("VAULTSYNCD_S062_CHILD_DB", rig.dir.database())
            .env("VAULTSYNCD_S062_CHILD_POINT", point)
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(code),
            "{point}: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let fresh = Arc::new(Database::open(rig.dir.database()).await.unwrap());
        let scheduler = build_scheduler(
            fresh.clone(),
            rig.clock.clone(),
            rig.handler.clone(),
            rig.gate.clone(),
        );
        let status = scheduler.status(rig.binding).await.unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].op, op);
        assert_eq!(status[0].attempts, u32::from(point == "failure"));
        assert_eq!(status[0].cancelled, point == "cancel");
        let intents = scheduler
            .ports
            .intents
            .pending_intents(rig.binding)
            .await
            .unwrap();
        assert_eq!(
            intents.len(),
            usize::from(matches!(point, "inflight" | "pending"))
        );
        if matches!(point, "failure" | "pending") {
            assert!(
                scheduler
                    .run_round(rig.binding)
                    .await
                    .unwrap()
                    .tasks
                    .is_empty()
            );
            rig.clock.advance(Duration::from_secs(60));
        }
        if point == "cancel" {
            assert!(
                scheduler
                    .run_round(rig.binding)
                    .await
                    .unwrap()
                    .tasks
                    .is_empty()
            );
            scheduler.resume(op).await.unwrap();
        }
        if !intents.is_empty() {
            rig.handler.0.intents.lock().unwrap().insert(op);
        }
        scheduler.run_round(rig.binding).await.unwrap();
        assert!(scheduler.status(rig.binding).await.unwrap().is_empty());
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(fresh.pool())
            .await
            .unwrap();
        assert_eq!(integrity, "ok");
    }
}

#[derive(Clone)]
struct AdapterOnly(Handler);
impl ActionHandler for AdapterOnly {
    fn plan_intent<'a>(
        &'a self,
        op: &'a QueuedOperation,
    ) -> PortFuture<'a, Result<Option<ActionIntent>, ActionFailure>> {
        self.0.plan_intent(op)
    }
    fn execute<'a>(
        &'a self,
        ports: &'a Ports,
        op: &'a QueuedOperation,
        intent: Option<&'a ActionIntent>,
    ) -> PortFuture<'a, Result<ActionOutcome, ActionFailure>> {
        Box::pin(async move {
            assert!(intent.is_some());
            assert_eq!(
                ports
                    .intents
                    .pending_intents(op.binding)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            self.0.0.calls.lock().unwrap().push(op.op);
            Ok(ActionOutcome::Pending {
                poll_after: Some(Duration::from_secs(30)),
            })
        })
    }
}
#[tokio::test]
async fn action_adapter_preserves_plan_intent_before_action_and_pending_is_not_complete() {
    let rig = Rig::new().await;
    let op = rig.enqueue(1, "apply_remote", 1, None).await;
    rig.handler.0.intents.lock().unwrap().insert(op);
    let state = Arc::new(ClientStorePorts::new(rig.db.clone()));
    let ports = Ports::new(
        Arc::new(FakeLocal),
        Arc::new(rig.clock.clone()),
        Arc::new(FakeRemote),
        state.clone(),
        state.clone(),
        state,
    );
    let scheduler = Scheduler::new(
        rig.db.clone(),
        ports,
        ActionAdapter(AdapterOnly(rig.handler.clone())),
        rig.gate.clone(),
        Arc::new(Samples(AtomicU64::new(0))),
        Limits::default(),
    )
    .unwrap();
    let report = scheduler.run_round(rig.binding).await.unwrap();
    assert_eq!(report.tasks, vec![(op, Outcome::Pending)]);
    assert_eq!(scheduler.status(rig.binding).await.unwrap().len(), 1);
    assert_eq!(rig.handler.calls(), vec![op]);
}

#[tokio::test]
async fn already_committed_action_settles_despite_concurrent_cancel() {
    let rig = Rig::new().await;
    let op = rig.enqueue(1, "apply_remote", 1, None).await;
    rig.handler.set(op, Behavior::CommittedWait);
    let scheduler = rig.scheduler();
    let round = scheduler.run_round(rig.binding);
    tokio::pin!(round);
    poll_fn(|cx| {
        assert!(round.as_mut().poll(cx).is_pending());
        if rig.handler.calls().contains(&op) {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
    scheduler.cancel(op).await.unwrap();
    rig.handler.release();
    assert_eq!(round.await.unwrap().tasks, vec![(op, Outcome::Completed)]);
    assert!(scheduler.status(rig.binding).await.unwrap().is_empty());
}

#[path = "consumer_tests.rs"]
mod consumer_tests;

#[tokio::test]
async fn failure_due_and_attempt_count_roll_back_together_when_schedule_write_fails() {
    let rig = Rig::new().await;
    let op = rig.enqueue(1, "apply_remote", 1, None).await;
    rig.handler
        .set(op, Behavior::Fail(failure(ErrorCode::RateLimited)));
    let scheduler = rig.scheduler();
    scheduler.status(rig.binding).await.unwrap();
    sqlx::query("CREATE TRIGGER abort_schedule BEFORE UPDATE OF due_at ON operation_schedule BEGIN SELECT RAISE(ABORT,'injected schedule failure'); END")
        .execute(rig.db.pool()).await.unwrap();
    assert!(scheduler.run_round(rig.binding).await.is_err());
    let state = scheduler.status(rig.binding).await.unwrap().remove(0);
    assert_eq!(state.attempts, 0);
    assert_eq!(state.error, None);
    assert_eq!(state.due.as_unix_seconds(), 0);
}

#[tokio::test]
async fn cancelling_before_first_dispatch_preserves_metadata_class_and_unknown_op_is_rejected() {
    let rig = Rig::new().await;
    let op = rig.enqueue(1, "merge_baseline", 1, None).await;
    let scheduler = rig.scheduler();
    scheduler.cancel(op).await.unwrap();
    let state = scheduler.status(rig.binding).await.unwrap().remove(0);
    assert_eq!(state.priority, Priority::Metadata);
    assert!(state.cancelled);
    assert!(
        scheduler
            .cancel(OpId::from_bytes([0xff; 16]))
            .await
            .is_err()
    );
    assert!(
        scheduler
            .run_round(rig.binding)
            .await
            .unwrap()
            .tasks
            .is_empty()
    );
}

#[tokio::test]
async fn native_reads_share_content_slots_entry_exclusion_and_control_gate() {
    let rig = Rig::new().await;
    let mut scheduler = rig.scheduler();
    scheduler.limits.content = 4;
    let reads = scheduler.user_reads();
    let background = QueuedOperation {
        op: OpId::from_bytes([1; 16]),
        binding: rig.binding,
        entry: Some(EntryId::from_bytes([1; 16])),
        kind: "apply_remote".into(),
        baseline_revision: None,
        baseline_version: None,
        depends_on: None,
        pinned_version: None,
        attempts: 0,
        last_error: None,
    };
    let slot = scheduler.claim(&background, Priority::Background).unwrap();
    let a = reads
        .try_acquire(
            OpId::from_bytes([2; 16]),
            rig.binding,
            EntryId::from_bytes([2; 16]),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        reads
            .try_acquire(
                OpId::from_bytes([3; 16]),
                rig.binding,
                EntryId::from_bytes([2; 16])
            )
            .await
            .unwrap()
            .is_none()
    );
    let b = reads
        .try_acquire(
            OpId::from_bytes([3; 16]),
            rig.binding,
            EntryId::from_bytes([3; 16]),
        )
        .await
        .unwrap()
        .unwrap();
    let c = reads
        .try_acquire(
            OpId::from_bytes([4; 16]),
            rig.binding,
            EntryId::from_bytes([4; 16]),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        reads
            .try_acquire(
                OpId::from_bytes([5; 16]),
                rig.binding,
                EntryId::from_bytes([5; 16])
            )
            .await
            .unwrap()
            .is_none()
    );
    let cancel = a.cancellation().clone();
    drop(a);
    assert!(cancel.is_cancelled());
    let d = reads
        .try_acquire(
            OpId::from_bytes([5; 16]),
            rig.binding,
            EntryId::from_bytes([5; 16]),
        )
        .await
        .unwrap()
        .unwrap();
    drop((slot, b, c, d));
    assert!(scheduler.active.lock().unwrap().tasks.is_empty());
    assert!(rig.db.pool().acquire().await.is_ok());
    rig.gate.closed.store(true, Ordering::SeqCst);
    assert!(
        reads
            .try_acquire(
                OpId::from_bytes([6; 16]),
                rig.binding,
                EntryId::from_bytes([6; 16])
            )
            .await
            .is_err()
    );
    assert!(scheduler.status(rig.binding).await.unwrap().is_empty());
}
