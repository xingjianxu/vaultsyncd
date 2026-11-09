//! 确定性的调度循环（design §8.4、§13.3；I01、I02、I06、I13）。
//!
//! 一轮 `run` 的固定顺序：
//!
//! 1. 暂停检查：暂停只返回原因，不读队列、不改任何状态；
//! 2. 读队列并按插入顺序处理：同一输入永远得到同一份 [`RunReport`]；
//! 3. 每个操作依次过“环检测 → 种类解析 → 依赖就绪 → 尝试次数 → 到期时间”五道闸；
//! 4. 就绪操作先经 [`IntentStore::begin`] 落本地写入意图，再交给 [`crate::executor::ActionHandler`]；
//! 5. 按结果出队、留 pending、退避或阻塞；**任何失败都不删除操作与其依赖方**。
//!
//! 依赖用单指针表达（`pending_ops.depends_on`），缺失的依赖视为“已出队（完成）”：队列在成功时删行，
//! 因此不在队列里意味着前置已经完成；外部删除前置属于数据损坏，执行器不做猜测性重放。
//!
//! # 为什么这里不 sleep
//!
//! 执行器只算“最早什么时候可以再试”（[`RunReport::next_due`]），等待与随机抖动由调度器负责。这样
//! 重试边界在测试里可以用固定时钟精确验证，也不会出现“执行器自己睡住、暂停请求进不来”的状态。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use vaultsyncd_domain::error::{ErrorClass, ErrorCode};
use vaultsyncd_domain::ids::{BindingId, OpId};
use vaultsyncd_domain::version::UtcTimestamp;

use crate::executor::retry::RetryPolicy;
use crate::executor::{ActionFailure, ActionHandler, ActionOutcome, OperationKind, PauseReason};
use crate::ports::clock::now;
use crate::ports::state::{ActionIntent, QueuedOperation};
use crate::ports::{PortError, Ports};

/// 服务端返回 `pending` 且没给 `Retry-After` 时的默认轮询间隔。
pub const DEFAULT_PENDING_POLL: Duration = Duration::from_secs(30);

/// 一轮执行的输入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRequest {
    /// 要执行的绑定。
    pub binding: BindingId,
    /// 暂停原因；`Some` 时本轮不启动任何动作。
    pub pause: Option<PauseReason>,
    /// 本轮最多启动多少个动作（`None` = 不限制）；到上限后剩余操作原样保留。
    pub max_ops: Option<usize>,
    /// `pending` 结果的默认轮询间隔。
    pub pending_poll_interval: Duration,
}

impl RunRequest {
    /// 该绑定的无限制一轮。
    #[must_use]
    pub fn new(binding: BindingId) -> Self {
        Self {
            binding,
            pause: None,
            max_ops: None,
            pending_poll_interval: DEFAULT_PENDING_POLL,
        }
    }

    /// 设置暂停原因。
    #[must_use]
    pub fn paused(mut self, reason: PauseReason) -> Self {
        self.pause = Some(reason);
        self
    }

    /// 限制本轮启动的动作数。
    #[must_use]
    pub fn with_max_ops(mut self, max_ops: usize) -> Self {
        self.max_ops = Some(max_ops);
        self
    }

    /// 设置 pending 轮询间隔。
    #[must_use]
    pub fn with_pending_poll(mut self, interval: Duration) -> Self {
        self.pending_poll_interval = interval;
        self
    }
}

/// 一个操作在本轮的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpReport {
    /// 操作身份。
    pub op: OpId,
    /// 解析出的种类；未知种类时为 `None`。
    pub kind: Option<OperationKind>,
    /// 结果。
    pub outcome: OpOutcome,
}

/// 操作在本轮走到了哪一步。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpOutcome {
    /// 已完成并出队（服务器 committed 或本地基线已落库）。
    Completed,
    /// 服务器已受理但未提交：不出队，也不是已同步（I01）。
    Pending {
        /// 实际采用的下次轮询间隔。
        poll_after: Duration,
    },
    /// 可重试失败：已累加尝试次数并退避，操作仍留在队列里。
    Retrying {
        /// 失败原因。
        code: ErrorCode,
        /// 累计尝试次数（含本次）。
        attempts: u32,
        /// 最早可以再试的时间。
        due: UtcTimestamp,
    },
    /// 停止自动推进（操作仍留在队列里）。
    Blocked(BlockReason),
}

/// 停止自动推进的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockReason {
    /// 前置操作本轮没有完成（含失败、重试中、被阻塞）：依赖方不启动。
    DependencyIncomplete {
        /// 未完成的前置操作。
        on: OpId,
    },
    /// 可重试错误的尝试次数已经用尽。
    RetriesExhausted {
        /// 最近一次的错误码。
        code: Option<ErrorCode>,
        /// 累计尝试次数。
        attempts: u32,
    },
    /// 错误需要用户或管理员处理（权限、挂载、额度等）。
    NeedsUserAction {
        /// 错误码。
        code: ErrorCode,
    },
    /// 完整性失败（摘要不符等）：保留证据，不自动重试。
    Corrupt {
        /// 错误码。
        code: ErrorCode,
    },
    /// 队列里的 `kind` 不是已定义的操作种类：不猜语义。
    UnknownKind {
        /// 原样的种类文本。
        kind: String,
    },
    /// 依赖关系成环（正常入队路径不会产生，来自坏数据或外部导入）。
    CyclicDependency,
    /// 退避还没到期：操作仍在队列里，到期后自动再试。
    BackoffPending {
        /// 最早可以再试的时间。
        due: UtcTimestamp,
    },
    /// 本轮动作数已达上限（`RunRequest::max_ops`）。
    RunLimitReached,
}

impl BlockReason {
    /// 相关的错误码（没有具体错误时返回 `None`）。
    #[must_use]
    pub const fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::RetriesExhausted { code, .. } => *code,
            Self::NeedsUserAction { code } | Self::Corrupt { code } => Some(*code),
            Self::DependencyIncomplete { .. }
            | Self::UnknownKind { .. }
            | Self::CyclicDependency
            | Self::BackoffPending { .. }
            | Self::RunLimitReached => None,
        }
    }

    /// 稳定短标签，供日志与状态展示。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::DependencyIncomplete { .. } => "dependency_incomplete",
            Self::RetriesExhausted { .. } => "retries_exhausted",
            Self::NeedsUserAction { .. } => "needs_user_action",
            Self::Corrupt { .. } => "corrupt",
            Self::UnknownKind { .. } => "unknown_kind",
            Self::CyclicDependency => "cyclic_dependency",
            Self::BackoffPending { .. } => "backoff_pending",
            Self::RunLimitReached => "run_limit_reached",
        }
    }
}

/// 一轮执行的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    /// 本轮处理的绑定。
    pub binding: BindingId,
    /// 暂停原因；暂停时 [`RunReport::ops`] 为空。
    pub paused: Option<PauseReason>,
    /// 队列里每个操作的结果，顺序 = 队列插入顺序。
    pub ops: Vec<OpReport>,
    /// 最早需要再来看一眼的时间（退避与 pending 轮询）。
    pub next_due: Option<UtcTimestamp>,
}

impl RunReport {
    /// 本轮是否暂停。
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.is_some()
    }

    /// 指定操作的结果。
    #[must_use]
    pub fn outcome(&self, op: OpId) -> Option<&OpOutcome> {
        self.ops
            .iter()
            .find(|report| report.op == op)
            .map(|report| &report.outcome)
    }

    /// 本轮启动的动作数（完成、pending 与重试中的操作）。
    #[must_use]
    pub fn started(&self) -> usize {
        self.ops
            .iter()
            .filter(|report| {
                matches!(
                    report.outcome,
                    OpOutcome::Completed | OpOutcome::Pending { .. } | OpOutcome::Retrying { .. }
                )
            })
            .count()
    }

    /// 本轮被阻塞（仍留在队列里）的操作。
    pub fn blocked(&self) -> impl Iterator<Item = (&OpId, &BlockReason)> {
        self.ops.iter().filter_map(|report| match &report.outcome {
            OpOutcome::Blocked(reason) => Some((&report.op, reason)),
            _ => None,
        })
    }
}

/// 执行器自身的失败（不是某个操作失败）。
#[derive(Debug)]
pub enum ExecutorError {
    /// 端口读写失败：本轮中止，队列保持原样。
    Port(PortError),
    /// 队列里有重复的操作身份：数据损坏，不执行任何一个操作。
    DuplicateOp {
        /// 重复的身份。
        op: OpId,
    },
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Port(error) => write!(f, "端口失败：{error}"),
            Self::DuplicateOp { op } => write!(f, "队列里有重复的操作 {op}"),
        }
    }
}

impl std::error::Error for ExecutorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Port(error) => Some(error),
            Self::DuplicateOp { .. } => None,
        }
    }
}

impl From<PortError> for ExecutorError {
    fn from(error: PortError) -> Self {
        Self::Port(error)
    }
}

/// 确定性操作执行器。
pub struct Executor<H> {
    ports: Ports,
    handler: H,
    retry: RetryPolicy,
    /// 本轮进程内的退避到期时间。
    ///
    /// 到期时间只跨“同一次进程内的多次 `run`”，不写数据库：跨重启持久化由 `engine::scheduler::Scheduler` 提供
    /// （design §13.3“退避带 jitter，上限和取消持久化”）。这里保留它，避免调用方每轮立刻重试导致
    /// 在极短时间内把有界的尝试次数烧光。
    not_before: Mutex<HashMap<OpId, UtcTimestamp>>,
}

impl<H: ActionHandler> Executor<H> {
    /// 用默认重试策略构造。
    #[must_use]
    pub fn new(ports: Ports, handler: H) -> Self {
        Self {
            ports,
            handler,
            retry: RetryPolicy::default_policy(),
            not_before: Mutex::new(HashMap::new()),
        }
    }

    /// 替换重试策略。
    #[must_use]
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// 共享的端口（处理器与测试用）。
    #[must_use]
    pub fn ports(&self) -> &Ports {
        &self.ports
    }

    /// 执行一轮。
    ///
    /// 暂停时只返回原因，不读队列（暂停不应产生任何副作用）。
    ///
    /// # Errors
    ///
    /// 端口读写失败或队列身份重复时中止本轮：已完成的动作保持完成，未开始的保持原样。
    pub async fn run(&self, request: &RunRequest) -> Result<RunReport, ExecutorError> {
        if let Some(reason) = request.pause {
            return Ok(RunReport {
                binding: request.binding,
                paused: Some(reason),
                ops: Vec::new(),
                next_due: None,
            });
        }

        let current = now(&*self.ports.clock)?;
        let queue = self.ports.operations.pending_ops(request.binding).await?;
        let index = index_queue(&queue)?;

        let kinds: Vec<Option<OperationKind>> = queue
            .iter()
            .map(|op| OperationKind::from_db(&op.kind))
            .collect();
        let cyclic = detect_cycles(&queue, &index);

        let mut outcomes: Vec<Option<OpOutcome>> = vec![None; queue.len()];
        let mut completed: HashSet<OpId> = HashSet::new();
        let mut next_due: Option<UtcTimestamp> = None;
        let mut intents: HashMap<OpId, i64> = HashMap::new();
        let mut started = 0usize;

        for (position, op) in queue.iter().enumerate() {
            if request.max_ops.is_some_and(|limit| started >= limit) {
                continue;
            }
            if cyclic.contains(&op.op) {
                outcomes[position] = Some(OpOutcome::Blocked(BlockReason::CyclicDependency));
                continue;
            }
            if kinds[position].is_none() {
                outcomes[position] = Some(OpOutcome::Blocked(BlockReason::UnknownKind {
                    kind: op.kind.clone(),
                }));
                continue;
            }
            if let Some(dependency) = op.depends_on {
                if index.contains_key(&dependency) && !completed.contains(&dependency) {
                    outcomes[position] =
                        Some(OpOutcome::Blocked(BlockReason::DependencyIncomplete {
                            on: dependency,
                        }));
                    continue;
                }
            }
            if self.retry.exhausted(op.attempts) {
                outcomes[position] = Some(OpOutcome::Blocked(BlockReason::RetriesExhausted {
                    code: op.last_error,
                    attempts: op.attempts,
                }));
                continue;
            }
            // 先取出到期时间：`if let` 条件里的临时 guard 会活到整个分支结束，重入会自锁。
            let pending_due = self
                .not_before
                .lock()
                .expect("退避表锁")
                .get(&op.op)
                .copied();
            if let Some(due) = pending_due {
                if due > current {
                    next_due = earliest(next_due, due);
                    outcomes[position] =
                        Some(OpOutcome::Blocked(BlockReason::BackoffPending { due }));
                    continue;
                }
                self.not_before.lock().expect("退避表锁").remove(&op.op);
            }

            started += 1;
            outcomes[position] = Some(
                self.execute_one(op, &mut intents, &mut completed, current, &mut next_due)
                    .await?,
            );
        }

        // 未执行的操作只可能是被本轮上限挡住；显式报告，避免“看起来像被丢弃”。
        for outcome in &mut outcomes {
            if outcome.is_none() {
                *outcome = Some(OpOutcome::Blocked(BlockReason::RunLimitReached));
            }
        }

        Ok(RunReport {
            binding: request.binding,
            paused: None,
            ops: queue
                .iter()
                .zip(kinds)
                .zip(outcomes)
                .map(|((op, kind), outcome)| OpReport {
                    op: op.op,
                    kind,
                    outcome: outcome.expect("每个操作都有结果"),
                })
                .collect(),
            next_due,
        })
    }

    /// 执行一个已经通过全部闸门的操作，返回它在报告里的结果。
    async fn execute_one(
        &self,
        op: &QueuedOperation,
        intents: &mut HashMap<OpId, i64>,
        completed: &mut HashSet<OpId>,
        current: UtcTimestamp,
        next_due: &mut Option<UtcTimestamp>,
    ) -> Result<OpOutcome, ExecutorError> {
        // 1) 先确定并持久化本地写入意图（design §8.4：意图先于磁盘改动）。
        let intent: Option<ActionIntent> = match self.handler.plan_intent(op).await {
            Ok(intent) => intent,
            Err(failure) => {
                return self
                    .settle_failure(op, None, &failure, current, next_due)
                    .await;
            }
        };
        let mut intent_id = intents.get(&op.op).copied();
        if intent_id.is_none() {
            if let Some(intent) = intent.as_ref() {
                let id = self.ports.intents.begin_intent(intent).await?;
                intents.insert(op.op, id);
                intent_id = Some(id);
            }
        }

        // 2) 执行动作。
        match self.handler.execute(&self.ports, op, intent.as_ref()).await {
            Ok(ActionOutcome::Completed) => {
                if let Some(id) = intent_id {
                    self.ports.intents.complete_intent(id).await?;
                }
                self.ports.operations.complete(op.op).await?;
                self.not_before.lock().expect("退避表锁").remove(&op.op);
                completed.insert(op.op);
                Ok(OpOutcome::Completed)
            }
            Ok(ActionOutcome::Pending { poll_after }) => {
                // pending 不是 committed（I01）：操作留在队列里，意图也保持 pending 供恢复核对。
                let poll = poll_after.unwrap_or(DEFAULT_PENDING_POLL);
                let due = advance(current, poll);
                self.not_before.lock().expect("退避表锁").insert(op.op, due);
                *next_due = earliest(*next_due, due);
                Ok(OpOutcome::Pending { poll_after: poll })
            }
            Err(failure) => {
                self.settle_failure(op, intent_id, &failure, current, next_due)
                    .await
            }
        }
    }

    /// 记录一次失败并按错误分类决定“退避重试”还是“停止自动推进”。
    async fn settle_failure(
        &self,
        op: &QueuedOperation,
        intent: Option<i64>,
        failure: &ActionFailure,
        current: UtcTimestamp,
        next_due: &mut Option<UtcTimestamp>,
    ) -> Result<OpOutcome, ExecutorError> {
        // 先落失败记录：即使后面判定为终态，队列表上的尝试次数与错误码也已经持久化。
        self.ports
            .operations
            .record_failure(op.op, failure.code)
            .await?;
        let attempts = op.attempts.saturating_add(1);

        match failure.code.class() {
            ErrorClass::Retryable if !self.retry.exhausted(attempts) => {
                let due = advance(current, self.retry.delay_after(attempts));
                self.not_before.lock().expect("退避表锁").insert(op.op, due);
                *next_due = earliest(*next_due, due);
                Ok(OpOutcome::Retrying {
                    code: failure.code,
                    attempts,
                    due,
                })
            }
            ErrorClass::Retryable => {
                // 次数用尽：意图失败保留现场，操作不出队，等重新规划或人工处理。
                self.not_before.lock().expect("退避表锁").remove(&op.op);
                if let Some(id) = intent {
                    self.ports.intents.fail_intent(id, failure.code).await?;
                }
                Ok(OpOutcome::Blocked(BlockReason::RetriesExhausted {
                    code: Some(failure.code),
                    attempts,
                }))
            }
            ErrorClass::NeedsUserAction => {
                self.not_before.lock().expect("退避表锁").remove(&op.op);
                if let Some(id) = intent {
                    self.ports.intents.fail_intent(id, failure.code).await?;
                }
                Ok(OpOutcome::Blocked(BlockReason::NeedsUserAction {
                    code: failure.code,
                }))
            }
            ErrorClass::Corrupt => {
                self.not_before.lock().expect("退避表锁").remove(&op.op);
                if let Some(id) = intent {
                    self.ports.intents.fail_intent(id, failure.code).await?;
                }
                Ok(OpOutcome::Blocked(BlockReason::Corrupt {
                    code: failure.code,
                }))
            }
        }
    }
}

/// 建操作身份索引；重复身份直接中止（不执行任何一个操作）。
fn index_queue(queue: &[QueuedOperation]) -> Result<HashMap<OpId, usize>, ExecutorError> {
    let mut index = HashMap::with_capacity(queue.len());
    for (position, op) in queue.iter().enumerate() {
        if index.insert(op.op, position).is_some() {
            return Err(ExecutorError::DuplicateOp { op: op.op });
        }
    }
    Ok(index)
}

/// 依赖成环的操作集合。
///
/// 正常入队路径不会产生环（依赖必须已存在、自依赖被拒绝、没有更新依赖的入口），但外部写入或导入
/// 可能带进来。这里沿 `depends_on` 单链走，走到已访问节点即成环，不会无限循环。
fn detect_cycles(queue: &[QueuedOperation], index: &HashMap<OpId, usize>) -> HashSet<OpId> {
    let mut cyclic = HashSet::new();
    for op in queue {
        let mut path: Vec<OpId> = Vec::new();
        let mut cursor = Some(op.op);
        while let Some(current) = cursor {
            if let Some(position) = path.iter().position(|seen| *seen == current) {
                // 只标记真正成环的那一段：指向环的前驱不是环成员，应报告为“依赖未完成”。
                cyclic.extend(path[position..].iter().copied());
                break;
            }
            path.push(current);
            cursor = index
                .get(&current)
                .and_then(|position| queue[*position].depends_on);
        }
    }
    cyclic
}

/// `base + delta`（秒，向上取整），饱和到 `i64` 上限。
fn advance(base: UtcTimestamp, delta: Duration) -> UtcTimestamp {
    let seconds = i64::try_from(delta.as_secs().max(1)).unwrap_or(i64::MAX);
    let sum = base.as_unix_seconds().saturating_add(seconds);
    UtcTimestamp::from_unix_seconds(sum).unwrap_or(base)
}

fn earliest(current: Option<UtcTimestamp>, candidate: UtcTimestamp) -> Option<UtcTimestamp> {
    Some(match current {
        Some(value) if value <= candidate => value,
        _ => candidate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::executor::OperationKind;
    use crate::ports::state::LocalWriteKind;
    use crate::testing::{
        FakeClock, FakeHandler, FakeLocal, FakePorts, FakeRemote, FakeResumeStore, FakeState,
        intent_for,
    };
    use vaultsyncd_domain::ids::{OpId, VersionId};
    use vaultsyncd_domain::version::{Generation, Revision};
    use vaultsyncd_fs_common::path::RelativePath;

    fn binding() -> BindingId {
        BindingId::from_bytes([0x10; 16])
    }

    fn op(marker: u8) -> OpId {
        OpId::from_bytes([marker; 16])
    }

    fn queued(marker: u8, kind: OperationKind, depends_on: Option<OpId>) -> QueuedOperation {
        QueuedOperation {
            op: op(marker),
            binding: binding(),
            kind: kind.as_str().to_owned(),
            entry: None,
            baseline_revision: None,
            baseline_version: None,
            depends_on,
            pinned_version: None,
            attempts: 0,
            last_error: None,
        }
    }

    fn executor(state: FakeState, handler: FakeHandler) -> Executor<FakeHandler> {
        executor_with(state, handler, FakeClock::at(1_700_000_000))
    }

    fn executor_with(
        state: FakeState,
        handler: FakeHandler,
        clock: FakeClock,
    ) -> Executor<FakeHandler> {
        Executor::new(
            FakePorts::assemble(
                FakeLocal::full(),
                clock,
                FakeRemote::new(),
                state.operations,
                state.intents,
                FakeResumeStore::new(),
            ),
            handler,
        )
    }

    #[tokio::test]
    async fn dependent_operations_run_only_after_their_prerequisite() {
        let mut state = FakeState::new();
        state.queue(vec![
            queued(0x01, OperationKind::UploadContent, None),
            queued(0x02, OperationKind::DeleteEntry, Some(op(0x01))),
            queued(0x03, OperationKind::MergeBaseline, Some(op(0x02))),
            queued(0x04, OperationKind::MergeBaseline, None),
        ]);
        let handler = FakeHandler::new();
        let report = executor(state.clone(), handler.clone())
            .run(&RunRequest::new(binding()))
            .await
            .expect("执行一轮");

        assert_eq!(
            handler.executed(),
            vec![op(0x01), op(0x02), op(0x03), op(0x04)],
            "按插入顺序执行，依赖先完成"
        );
        assert_eq!(
            report
                .ops
                .iter()
                .map(|report| report.outcome.clone())
                .collect::<Vec<_>>(),
            vec![
                OpOutcome::Completed,
                OpOutcome::Completed,
                OpOutcome::Completed,
                OpOutcome::Completed,
            ]
        );
        assert!(state.operations.pending().expect("读队列").is_empty());
        assert_eq!(report.started(), 4);
        assert_eq!(report.next_due, None);
    }

    #[tokio::test]
    async fn a_failed_action_and_its_dependents_stay_in_the_queue() {
        let mut state = FakeState::new();
        state.queue(vec![
            queued(0x01, OperationKind::UploadContent, None),
            queued(0x02, OperationKind::DeleteEntry, Some(op(0x01))),
            queued(0x03, OperationKind::MergeBaseline, None),
        ]);
        // 第一个操作需要用户处理（403）：立即终态，不重试。
        let handler = FakeHandler::new().fail(
            op(0x01),
            ActionFailure::new(ErrorCode::AccessDenied, "跨 Vault"),
        );
        let report = executor(state.clone(), handler.clone())
            .run(&RunRequest::new(binding()))
            .await
            .expect("执行一轮");

        assert_eq!(handler.executed(), vec![op(0x01), op(0x03)], "依赖方不启动");
        assert_eq!(
            report.outcome(op(0x01)),
            Some(&OpOutcome::Blocked(BlockReason::NeedsUserAction {
                code: ErrorCode::AccessDenied
            }))
        );
        assert_eq!(
            report.outcome(op(0x02)),
            Some(&OpOutcome::Blocked(BlockReason::DependencyIncomplete {
                on: op(0x01)
            })),
            "依赖方保留并说明原因"
        );
        assert_eq!(report.outcome(op(0x03)), Some(&OpOutcome::Completed));

        // 队列里失败动作、依赖方（以及成功出队的操作之外的）都还在；失败次数已持久化。
        let pending = state.operations.pending().expect("读队列");
        assert_eq!(
            pending.iter().map(|op| op.op).collect::<Vec<_>>(),
            vec![op(0x01), op(0x02)]
        );
        assert_eq!(pending[0].attempts, 1);
        assert_eq!(pending[0].last_error, Some(ErrorCode::AccessDenied));
        assert_eq!(pending[1].attempts, 0, "依赖方没有被执行过");
    }

    #[tokio::test]
    async fn retries_are_bounded_even_across_runs() {
        let mut state = FakeState::new();
        state.queue(vec![queued(0x01, OperationKind::UploadContent, None)]);
        let handler =
            FakeHandler::new().fail(op(0x01), ActionFailure::new(ErrorCode::RateLimited, "429"));
        let clock = FakeClock::at(1_700_000_000);
        let executor = executor_with(state.clone(), handler.clone(), clock.clone())
            .with_retry_policy(
                RetryPolicy::new(3, Duration::from_secs(1), Duration::from_secs(10)).expect("策略"),
            );

        // 每一轮之前把时钟推到“到期之后”：重试次数仍然只有 3 次。
        let mut rounds = 0;
        loop {
            let report = executor
                .run(&RunRequest::new(binding()))
                .await
                .expect("执行一轮");
            rounds += 1;
            match report.outcome(op(0x01)) {
                Some(OpOutcome::Retrying { attempts, .. }) => {
                    assert_eq!(*attempts, rounds, "尝试次数逐轮累加");
                }
                Some(OpOutcome::Blocked(BlockReason::RetriesExhausted {
                    code: Some(ErrorCode::RateLimited),
                    attempts,
                })) => {
                    assert_eq!(*attempts, 3);
                    break;
                }
                other => panic!("意外的结果 {other:?}"),
            }
            assert!(rounds < 10, "有界重试必须在 3 轮内停止");
            let pending = state.operations.pending().expect("读队列");
            assert_eq!(pending.len(), 1, "重试期间操作不出队");
            assert_eq!(pending[0].attempts, rounds);
            // 推进时钟越过退避窗口。
            clock.advance(Duration::from_secs(60));
        }

        assert_eq!(handler.executed().len(), 3, "总共只执行 3 次");
        let pending = state.operations.pending().expect("读队列");
        assert_eq!(pending.len(), 1, "用尽次数后操作仍保留");
        assert_eq!(pending[0].attempts, 3);

        // 用尽后即使再等很久也不会再执行。
        clock.advance(Duration::from_secs(3_600));
        let report = executor
            .run(&RunRequest::new(binding()))
            .await
            .expect("执行一轮");
        assert_eq!(
            report.outcome(op(0x01)),
            Some(&OpOutcome::Blocked(BlockReason::RetriesExhausted {
                code: Some(ErrorCode::RateLimited),
                attempts: 3
            }))
        );
        assert_eq!(handler.executed().len(), 3);
    }

    #[tokio::test]
    async fn a_retry_before_the_backoff_deadline_does_nothing() {
        let mut state = FakeState::new();
        state.queue(vec![queued(0x01, OperationKind::UploadContent, None)]);
        let handler = FakeHandler::new().fail(
            op(0x01),
            ActionFailure::new(ErrorCode::LocalChanged, "本地内容变化"),
        );
        let clock = FakeClock::at(1_700_000_000);
        let executor = executor_with(state.clone(), handler.clone(), clock.clone())
            .with_retry_policy(
                RetryPolicy::new(5, Duration::from_secs(10), Duration::from_secs(60))
                    .expect("策略"),
            );

        let first = executor
            .run(&RunRequest::new(binding()))
            .await
            .expect("第一轮");
        let Some(OpOutcome::Retrying { due, .. }) = first.outcome(op(0x01)) else {
            panic!("第一轮应退避");
        };
        assert_eq!(
            *due,
            UtcTimestamp::from_unix_seconds(1_700_000_010).expect("时刻")
        );
        assert_eq!(first.next_due, Some(*due));

        // 时间没到：不执行、不累加次数。
        let second = executor
            .run(&RunRequest::new(binding()))
            .await
            .expect("第二轮");
        assert_eq!(
            second.outcome(op(0x01)),
            Some(&OpOutcome::Blocked(BlockReason::BackoffPending {
                due: *due
            })),
            "退避期内的再次调用只报告到期时间"
        );
        assert_eq!(second.next_due, Some(*due));
        assert_eq!(handler.executed().len(), 1, "退避期间不重复执行");
        assert_eq!(state.operations.pending().expect("读队列")[0].attempts, 1);

        // 到期后原样重试同一个操作（OpId 不变）。
        clock.advance(Duration::from_secs(10));
        let third = executor
            .run(&RunRequest::new(binding()))
            .await
            .expect("第三轮");
        assert!(matches!(
            third.outcome(op(0x01)),
            Some(OpOutcome::Retrying { attempts: 2, .. })
        ));
        assert_eq!(handler.executed(), vec![op(0x01), op(0x01)]);
    }

    #[tokio::test]
    async fn pausing_keeps_every_operation_and_starts_nothing() {
        let mut state = FakeState::new();
        state.queue(vec![
            queued(0x01, OperationKind::UploadContent, None),
            queued(0x02, OperationKind::MergeBaseline, None),
        ]);
        let handler = FakeHandler::new();
        let report = executor(state.clone(), handler.clone())
            .run(&RunRequest::new(binding()).paused(PauseReason::Offline))
            .await
            .expect("暂停的一轮");

        assert!(report.is_paused());
        assert_eq!(report.paused, Some(PauseReason::Offline));
        assert!(report.ops.is_empty());
        assert!(handler.executed().is_empty());
        assert_eq!(state.operations.pending().expect("读队列").len(), 2);

        // 恢复后照常执行。
        let resumed = executor(state.clone(), handler.clone())
            .run(&RunRequest::new(binding()))
            .await
            .expect("恢复的一轮");
        assert_eq!(resumed.started(), 2);
        assert_eq!(handler.executed(), vec![op(0x01), op(0x02)]);
    }

    #[tokio::test]
    async fn an_intent_is_persisted_before_the_action_runs() {
        let mut state = FakeState::new();
        state.queue(vec![queued(0x01, OperationKind::ApplyRemote, None)]);
        let handler = FakeHandler::new()
            .watch_intents(state.intents.shared())
            .intent_for(
                op(0x01),
                Some(intent_for(
                    op(0x01),
                    binding(),
                    LocalWriteKind::Replace,
                    "docs/report.txt",
                )),
            );
        let executor = executor(state.clone(), handler.clone());
        let report = executor
            .run(&RunRequest::new(binding()))
            .await
            .expect("执行一轮");

        assert_eq!(report.outcome(op(0x01)), Some(&OpOutcome::Completed));
        // 执行时处理器拿到的意图与落库内容一致，且执行那一刻意图表里已经有它。
        let seen = handler.seen_intents();
        assert_eq!(seen.len(), 1, "执行前写入并传入一条意图");
        assert_eq!(seen[0].kind, LocalWriteKind::Replace);
        assert_eq!(seen[0].target_path.to_string(), "docs/report.txt");
        assert_eq!(seen[0].generation.get(), 7, "意图携带观察到的代际");
        assert!(
            handler.saw_intent_before_execute(),
            "执行时必须已经能看到意图"
        );
        assert!(state.intents.all().is_empty(), "成功后意图删除");
    }

    #[tokio::test]
    async fn a_failed_action_keeps_its_intent_and_generation() {
        let mut state = FakeState::new();
        state.queue(vec![queued(0x01, OperationKind::ApplyRemote, None)]);
        let handler = FakeHandler::new()
            .intent_for(
                op(0x01),
                Some(intent_for(
                    op(0x01),
                    binding(),
                    LocalWriteKind::Replace,
                    "docs/report.txt",
                )),
            )
            .fail(
                op(0x01),
                ActionFailure::new(ErrorCode::HashMismatch, "摘要不符"),
            );
        let executor = executor(state.clone(), handler.clone());
        executor
            .run(&RunRequest::new(binding()))
            .await
            .expect("执行一轮");

        let intents = state.intents.all();
        assert_eq!(intents.len(), 1, "失败不改删意图");
        assert!(intents[0].failed, "意图被标记失败");
        assert_eq!(intents[0].last_error, Some(ErrorCode::HashMismatch));
        assert_eq!(intents[0].generation.expect("代际").get(), 7);
    }

    #[tokio::test]
    async fn pending_results_do_not_dequeue_the_operation() {
        let mut state = FakeState::new();
        state.queue(vec![queued(0x01, OperationKind::UploadContent, None)]);
        let handler = FakeHandler::new().pending(op(0x01), Some(Duration::from_secs(5)));
        let executor = executor(state.clone(), handler.clone());
        let report = executor
            .run(&RunRequest::new(binding()))
            .await
            .expect("执行一轮");

        assert_eq!(
            report.outcome(op(0x01)),
            Some(&OpOutcome::Pending {
                poll_after: Duration::from_secs(5)
            })
        );
        assert_eq!(
            report.next_due,
            Some(UtcTimestamp::from_unix_seconds(1_700_000_005).expect("时刻"))
        );
        let pending = state.operations.pending().expect("读队列");
        assert_eq!(pending.len(), 1, "pending 不是 committed（I01）");
        assert_eq!(pending[0].attempts, 0, "pending 不算失败");
    }

    #[tokio::test]
    async fn unknown_kinds_and_cycles_are_blocked_without_guessing() {
        let mut state = FakeState::new();
        let mut unknown = queued(0x01, OperationKind::UploadContent, None);
        unknown.kind = "delete_everything".to_owned();
        state.queue(vec![
            unknown,
            queued(0x02, OperationKind::MergeBaseline, Some(op(0x03))),
            queued(0x03, OperationKind::MergeBaseline, Some(op(0x02))),
        ]);
        let handler = FakeHandler::new();
        let report = executor(state.clone(), handler.clone())
            .run(&RunRequest::new(binding()))
            .await
            .expect("执行一轮");

        assert!(handler.executed().is_empty(), "坏数据不执行");
        assert!(matches!(
            report.outcome(op(0x01)),
            Some(OpOutcome::Blocked(BlockReason::UnknownKind { kind })) if kind == "delete_everything"
        ));
        assert_eq!(
            report.outcome(op(0x02)),
            Some(&OpOutcome::Blocked(BlockReason::CyclicDependency))
        );
        assert_eq!(
            report.outcome(op(0x03)),
            Some(&OpOutcome::Blocked(BlockReason::CyclicDependency))
        );
        assert_eq!(state.operations.pending().expect("读队列").len(), 3);
    }

    #[tokio::test]
    async fn the_run_limit_defers_the_rest_instead_of_dropping_it() {
        let mut state = FakeState::new();
        state.queue(vec![
            queued(0x01, OperationKind::UploadContent, None),
            queued(0x02, OperationKind::UploadContent, None),
            queued(0x03, OperationKind::UploadContent, None),
        ]);
        let handler = FakeHandler::new();
        let report = executor(state.clone(), handler.clone())
            .run(&RunRequest::new(binding()).with_max_ops(1))
            .await
            .expect("执行一轮");

        assert_eq!(handler.executed(), vec![op(0x01)]);
        assert_eq!(
            report.outcome(op(0x02)),
            Some(&OpOutcome::Blocked(BlockReason::RunLimitReached))
        );
        assert_eq!(
            state
                .operations
                .pending()
                .expect("读队列")
                .iter()
                .map(|op| op.op)
                .collect::<Vec<_>>(),
            vec![op(0x02), op(0x03)]
        );
    }

    #[tokio::test]
    async fn duplicate_operation_identities_abort_the_run_without_executing() {
        let mut state = FakeState::new();
        let first = queued(0x01, OperationKind::UploadContent, None);
        state.queue(vec![first.clone(), first]);
        let handler = FakeHandler::new();
        let error = executor(state.clone(), handler.clone())
            .run(&RunRequest::new(binding()))
            .await
            .expect_err("重复身份必须中止");
        assert!(matches!(
            error,
            ExecutorError::DuplicateOp { op } if op == OpId::from_bytes([0x01; 16])
        ));
        assert!(handler.executed().is_empty());
    }

    #[tokio::test]
    async fn the_same_queue_produces_the_same_schedule() {
        let mut state = FakeState::new();
        state.queue(vec![
            queued(0x01, OperationKind::UploadContent, None),
            queued(0x02, OperationKind::MergeBaseline, Some(op(0x01))),
            queued(0x03, OperationKind::UploadContent, None),
        ]);
        let handler = FakeHandler::new();
        let executor = executor(state, handler.clone());

        let first = executor
            .run(&RunRequest::new(binding()))
            .await
            .expect("第一轮");
        let second = executor
            .run(&RunRequest::new(binding()))
            .await
            .expect("第二轮");
        // 第一轮全部完成后队列为空，第二轮没有任何动作：两次都不产生随机顺序。
        assert_eq!(second.ops.len(), 0);
        assert_eq!(first.started(), 3);
        assert_eq!(handler.executed(), vec![op(0x01), op(0x02), op(0x03)]);

        let _: Generation = Generation::new(7).expect("代际");
        let _: Revision = Revision::new(2).expect("修订");
        let _: Option<VersionId> = None;
        let _: Option<RelativePath> = None;
    }
}
