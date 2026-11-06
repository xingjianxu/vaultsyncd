//! S062：用户读取优先、元数据独立槽、同文件串行、持久退避和取消。
//!
//! 一个客户端进程复用一个 Scheduler（跨绑定共享资源上限）。run_round 可并发调用：后台内容
//! 最多占 content-1 槽，用户打开和元数据无需等待后台批次结束。调用方依据 next_due 或外部唤醒
//! 再调度；本模块不 sleep、不驱逐缓存。重启只重建活动槽，原 OpId/意图/续传和退避仍在SQLite。
mod model;
mod store;
#[cfg(test)]
mod tests;
pub use model::*;

use crate::{
    executor::{ActionOutcome, OperationKind, RetryPolicy},
    ports::{PortError, Ports, state::QueuedOperation},
};
use std::{
    collections::{HashMap, HashSet},
    future::{Future, poll_fn},
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::{
    error::{ErrorClass, ErrorCode},
    ids::{BindingId, EntryId, OpId},
    version::UtcTimestamp,
};

struct ActiveTask {
    binding: BindingId,
    entry: Option<EntryId>,
    priority: Priority,
    cancel: Cancellation,
}
#[derive(Default)]
struct Active {
    tasks: HashMap<OpId, ActiveTask>,
}
struct Slot {
    active: Arc<Mutex<Active>>,
    op: OpId,
    cancel: Cancellation,
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.active.lock().expect("调度槽锁").tasks.remove(&self.op);
    }
}
/// A user read shares the scheduler's content limit and per-entry exclusion, but cannot
/// complete a synchronization operation or change a baseline. Durable transfer progress
/// belongs to the transfer store; an OS read request itself is short-lived.
pub struct UserReadGuard(Slot);
impl UserReadGuard {
    /// Cancellation shared with the active slot. Dropping the guard releases admission.
    pub fn cancellation(&self) -> &Cancellation {
        &self.0.cancel
    }
}
/// Cloneable admission handle for CFAPI workers. It must come from the SAME scheduler
/// that runs background work, rather than a second independently constructed scheduler.
#[derive(Clone)]
pub struct UserReads {
    active: Arc<Mutex<Active>>,
    gate: Arc<dyn DispatchGate>,
    limits: Limits,
}
impl UserReads {
    /// Try once without waiting or holding a database transaction. None means capacity
    /// or this entry is busy; the runtime may wait with a bounded timeout/cancellation.
    pub async fn try_acquire(
        &self,
        request: OpId,
        binding: BindingId,
        entry: EntryId,
    ) -> Result<Option<UserReadGuard>, PortError> {
        let op = QueuedOperation {
            op: request,
            binding,
            entry: Some(entry),
            kind: "user_read".into(),
            baseline_revision: None,
            baseline_version: None,
            depends_on: None,
            pinned_version: None,
            attempts: 0,
            last_error: None,
        };
        if !self.gate.allowed(&op).await? {
            return Err(PortError::rejected(
                "BINDING_NOT_READY",
                "用户读取被绑定控制拒绝",
            ));
        }
        let slot = claim(&self.active, self.limits, &op, Priority::UserRead);
        if slot.is_some() && !self.gate.allowed(&op).await? {
            return Err(PortError::rejected(
                "BINDING_NOT_READY",
                "用户读取前绑定控制改变",
            ));
        }
        Ok(slot.map(UserReadGuard))
    }
}
/// 持久调度器。数据库与 Ports.operations 必须来自同一客户端库。
pub struct Scheduler<H> {
    db: Arc<Database>,
    ports: Ports,
    handler: H,
    gate: Arc<dyn DispatchGate>,
    jitter: Arc<dyn Jitter>,
    limits: Limits,
    retry: RetryPolicy,
    active: Arc<Mutex<Active>>,
}
impl<H: ScheduledHandler> Scheduler<H> {
    /// Share admission with native user reads without allowing them to dequeue sync work.
    pub fn user_reads(&self) -> UserReads {
        UserReads {
            active: self.active.clone(),
            gate: self.gate.clone(),
            limits: self.limits,
        }
    }
    /// 构造；参数错误不退化成无界并发或零延迟。
    pub fn new(
        db: Arc<Database>,
        ports: Ports,
        handler: H,
        gate: Arc<dyn DispatchGate>,
        jitter: Arc<dyn Jitter>,
        limits: Limits,
    ) -> Result<Self, PortError> {
        if limits.content < 2 || limits.metadata == 0 || limits.hash == 0 {
            return Err(PortError::protocol("内容并发至少2、元数据和哈希并发至少1"));
        }
        Ok(Self {
            db,
            ports,
            handler,
            gate,
            jitter,
            limits,
            retry: RetryPolicy::default(),
            active: Arc::default(),
        })
    }
    /// 配置有限重试；不重置已持久的次数。
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }
    /// 提升正在等待的用户读取或标记哈希任务；不越过依赖、退避和绑定控制。
    pub async fn prioritize(&self, op: OpId, priority: Priority) -> Result<(), PortError> {
        let mut conn = self.db.pool().acquire().await.map_err(store::error)?;
        let queued = vaultsyncd_client_store::queue::pending_op(&mut conn, op)
            .await
            .map_err(store::error)?
            .ok_or_else(|| PortError::protocol("优先级操作不存在"))?;
        let default = default_priority(&queued)?;
        let read = matches!(
            OperationKind::from_db(&queued.kind),
            Some(OperationKind::ApplyRemote | OperationKind::CreateFromRemote)
        );
        if priority != default && !(priority == Priority::UserRead && read) {
            return Err(PortError::protocol(
                "优先级不能改变资源类别，用户读取仅限下载",
            ));
        }
        store::priority(&self.db, op, priority).await
    }
    /// 先持久取消，再通知活动传输；保留队列、块进度与本地内容。
    pub async fn cancel(&self, op: OpId) -> Result<(), PortError> {
        let mut conn = self.db.pool().acquire().await.map_err(store::error)?;
        let queued = vaultsyncd_client_store::queue::pending_op(&mut conn, op)
            .await
            .map_err(store::error)?
            .ok_or_else(|| PortError::protocol("取消操作不存在"))?;
        store::ensure(&self.db, &queued, default_priority(&queued)?).await?;
        store::cancel(&self.db, op).await?;
        if let Some(task) = self.active.lock().expect("调度槽锁").tasks.get(&op) {
            task.cancel.cancel();
        }
        Ok(())
    }
    /// 显式恢复取消/停止的任务；重试次数仍累计，不隐藏持续失败。
    /// 次数用尽的操作需重新规划或更高的合法 retry policy，不能靠恢复按钮无限重试。
    pub async fn resume(&self, op: OpId) -> Result<(), PortError> {
        let result =
            sqlx::query("UPDATE operation_schedule SET cancelled=0,halted=0 WHERE op_id=?1")
                .bind(op.as_bytes().as_slice())
                .execute(self.db.pool())
                .await
                .map_err(store::error)?;
        if result.rows_affected() != 1 {
            return Err(PortError::protocol("恢复操作不存在"));
        }
        Ok(())
    }
    /// 持久队列视图；pending、退避和取消都仍在其中，普通断网不会生成系统通知。
    pub async fn status(&self, binding: BindingId) -> Result<Vec<TaskStatus>, PortError> {
        let queue = self.ports.operations.pending_ops(binding).await?;
        let mut result = Vec::with_capacity(queue.len());
        for op in queue {
            store::ensure(&self.db, &op, default_priority(&op)?).await?;
            result.push(store::status(&self.db, &op).await?);
        }
        Ok(result)
    }
    /// 启动当前可运行的一批并异步推进。中断本 future 会丢弃活动 I/O，持久意图和任务照旧保留。
    /// 暂停/排除/解绑每个动作前重新核对；控制变更后调用 cancel 终止已有传输。
    pub async fn run_round(&self, binding: BindingId) -> Result<RoundReport, PortError> {
        let current = crate::ports::clock::now(&*self.ports.clock)?;
        let queue = self.ports.operations.pending_ops(binding).await?;
        let ids: HashSet<_> = queue.iter().map(|op| op.op).collect();
        if ids.len() != queue.len() {
            return Err(PortError::protocol("队列含重复OpId"));
        }
        let mut candidates = Vec::new();
        let mut report = RoundReport::default();
        for op in queue {
            store::ensure(&self.db, &op, default_priority(&op)?).await?;
            let status = store::status(&self.db, &op).await?;
            if status.cancelled || status.halted || self.retry.exhausted(op.attempts) {
                continue;
            }
            if status.due > current {
                report.next_due = minimum(report.next_due, status.due);
                continue;
            }
            if let Some(dependency) = op.depends_on {
                let pending = if ids.contains(&dependency) {
                    true
                } else {
                    sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS(SELECT 1 FROM pending_ops WHERE op_id=?1)",
                    )
                    .bind(dependency.as_bytes().as_slice())
                    .fetch_one(self.db.pool())
                    .await
                    .map_err(store::error)?
                };
                if pending {
                    continue;
                }
            }
            candidates.push((op, status.priority));
        }
        candidates.sort_by_key(|(_, priority)| *priority);
        let mut selected = Vec::new();
        for (op, priority) in candidates {
            if !self.gate.allowed(&op).await? {
                continue;
            }
            if let Some(slot) = self.claim(&op, priority) {
                selected.push((op, slot));
            }
        }
        let mut running: Vec<_> = selected
            .into_iter()
            .map(|(op, slot)| Box::pin(self.run_one(op, slot)))
            .collect();
        while !running.is_empty() {
            let (index, result) = poll_fn(|cx| {
                for (index, future) in running.iter_mut().enumerate() {
                    if let Poll::Ready(result) = future.as_mut().poll(cx) {
                        return Poll::Ready((index, result));
                    }
                }
                Poll::Pending
            })
            .await;
            drop(running.swap_remove(index));
            report.tasks.push(result?);
        }
        for status in self.status(binding).await? {
            if !status.cancelled
                && !status.halted
                && status.due > current
                && !self.retry.exhausted(status.attempts)
            {
                report.next_due = minimum(report.next_due, status.due);
            }
        }
        Ok(report)
    }
    fn claim(&self, op: &QueuedOperation, priority: Priority) -> Option<Slot> {
        claim(&self.active, self.limits, op, priority)
    }
    async fn run_one(&self, op: QueuedOperation, slot: Slot) -> Result<(OpId, Outcome), PortError> {
        // claim之后再次过持久取消和控制围栏；排队期间发生的取消不能丢。
        let status = store::status(&self.db, &op).await?;
        if status.cancelled {
            return Ok((op.op, Outcome::Cancelled));
        }
        if !self.gate.allowed(&op).await? {
            return Ok((op.op, Outcome::Deferred));
        }
        let intent = match self.handler.plan_intent(&op).await {
            Ok(intent) => intent,
            Err(failure) => return self.failed(&op, failure.into(), None).await,
        };
        let intent_id = match &intent {
            Some(value) => Some(self.ports.intents.begin_intent(value).await?),
            None => None,
        };
        if slot.cancel.is_cancelled() {
            return Ok((op.op, Outcome::Cancelled));
        }
        if !self.gate.allowed(&op).await? {
            return Ok((op.op, Outcome::Deferred));
        }
        let result = self
            .handler
            .execute_scheduled(&self.ports, &op, intent.as_ref(), &slot.cancel)
            .await;
        match result {
            Ok(ActionOutcome::Completed) => {
                if let Some(id) = intent_id {
                    self.ports.intents.complete_intent(id).await?;
                }
                store::complete(&self.db, &op).await?;
                Ok((op.op, Outcome::Completed))
            }
            Ok(ActionOutcome::Pending { poll_after }) => {
                let now = crate::ports::clock::now(&*self.ports.clock)?;
                let due = advance(
                    now,
                    poll_after
                        .unwrap_or(Duration::from_secs(30))
                        .max(Duration::from_secs(1)),
                )?;
                store::pending(&self.db, op.op, due).await?;
                Ok((op.op, Outcome::Pending))
            }
            Err(TaskError::Cancelled) => {
                store::cancel(&self.db, op.op).await?;
                Ok((op.op, Outcome::Cancelled))
            }
            Err(error) => self.failed(&op, error, intent_id).await,
        }
    }
    async fn failed(
        &self,
        op: &QueuedOperation,
        error: TaskError,
        intent: Option<i64>,
    ) -> Result<(OpId, Outcome), PortError> {
        let (failure, retry_after, offline, permission) = match error {
            TaskError::Failed {
                failure,
                retry_after,
                offline,
            } => (failure, retry_after, offline, false),
            TaskError::LocalPermission(failure) => (failure, None, false, true),
            TaskError::Cancelled => {
                store::cancel(&self.db, op.op).await?;
                return Ok((op.op, Outcome::Cancelled));
            }
        };
        let attempts = op.attempts.saturating_add(1);
        // 空间/本机占用允许低频复查；403/认证不盲目重试。任何失败均不驱逐缓存。
        let periodic = permission || matches!(failure.code, ErrorCode::InsufficientStorage);
        let halted = self.retry.exhausted(attempts)
            || (failure.code.class() != ErrorClass::Retryable && !periodic);
        let cap = self.retry.delay_after(attempts).as_secs().max(1);
        let low = cap.div_ceil(2).max(1);
        let jitter = low + self.jitter.sample() % (cap - low + 1);
        let delay = if periodic {
            Duration::from_secs(60)
        } else {
            Duration::from_secs(jitter)
        };
        let delay = delay.max(retry_after.unwrap_or_default());
        let due = advance(crate::ports::clock::now(&*self.ports.clock)?, delay)?;
        store::failure(&self.db, op, failure.code, due, halted, offline).await?;
        if halted {
            if let Some(id) = intent {
                self.ports.intents.fail_intent(id, failure.code).await?;
            }
        }
        Ok((op.op, Outcome::Failed))
    }
}
fn claim(
    shared: &Arc<Mutex<Active>>,
    limits: Limits,
    op: &QueuedOperation,
    priority: Priority,
) -> Option<Slot> {
    let mut active = shared.lock().expect("调度槽锁");
    if active.tasks.contains_key(&op.op)
        || active.tasks.values().any(|t| {
            t.binding == op.binding
                && (t.entry == op.entry || t.entry.is_none() || op.entry.is_none())
        })
    {
        return None;
    }
    let count = |p| active.tasks.values().filter(|t| t.priority == p).count();
    let content = count(Priority::UserRead) + count(Priority::Background);
    let full = match priority {
        Priority::UserRead => content >= limits.content,
        Priority::Background => {
            content >= limits.content || count(Priority::Background) >= limits.content - 1
        }
        Priority::Metadata => count(priority) >= limits.metadata,
        Priority::Hash => count(priority) >= limits.hash,
    };
    if full {
        return None;
    }
    let cancel = Cancellation::new();
    active.tasks.insert(
        op.op,
        ActiveTask {
            binding: op.binding,
            entry: op.entry,
            priority,
            cancel: cancel.clone(),
        },
    );
    Some(Slot {
        active: shared.clone(),
        op: op.op,
        cancel,
    })
}
fn default_priority(op: &QueuedOperation) -> Result<Priority, PortError> {
    if op.kind == "hash_content" {
        return Ok(Priority::Hash);
    }
    match OperationKind::from_db(&op.kind) {
        Some(
            OperationKind::UploadContent
            | OperationKind::UploadNewEntry
            | OperationKind::UploadMoveWithContent
            | OperationKind::CreateConflictCopy
            | OperationKind::ApplyRemote
            | OperationKind::CreateFromRemote,
        ) => Ok(Priority::Background),
        Some(_) => Ok(Priority::Metadata),
        None => Err(PortError::protocol("未知操作种类，拒绝启动")),
    }
}
fn minimum(current: Option<UtcTimestamp>, value: UtcTimestamp) -> Option<UtcTimestamp> {
    Some(current.map_or(value, |old| old.min(value)))
}
fn advance(now: UtcTimestamp, delay: Duration) -> Result<UtcTimestamp, PortError> {
    // UTC按秒保存，非整数Retry-After向上取整，不能早于服务端下界。
    let seconds = delay
        .as_secs()
        .saturating_add(u64::from(delay.subsec_nanos() != 0));
    let seconds = i64::try_from(seconds).map_err(store::error)?;
    let due = now
        .as_unix_seconds()
        .checked_add(seconds)
        .ok_or_else(|| PortError::protocol("退避时间溢出"))?;
    UtcTimestamp::from_unix_seconds(due).map_err(store::error)
}
