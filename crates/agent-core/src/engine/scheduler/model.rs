//! 调度输入、状态和端口；运行时与平台 I/O 留在调用方。
use crate::{
    executor::{ActionFailure, ActionHandler, ActionOutcome},
    ports::{
        PortError, PortFuture, Ports,
        state::{ActionIntent, QueuedOperation},
    },
};
use std::time::Duration;
use vaultsyncd_domain::{
    error::ErrorCode,
    ids::{BindingId, OpId},
    version::UtcTimestamp,
};

/// 持久优先级；同级按原队列插入序，不用文件大小或 mtime 排序。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i64)]
pub enum Priority {
    /// 用户正在打开的文件；有独立保留的内容槽。
    UserRead = 0,
    /// 小元数据动作；不占内容槽。
    Metadata = 1,
    /// 后台内容传输。
    Background = 2,
    /// 内容摘要任务；使用独立有界槽。
    Hash = 3,
}
/// 有界资源参数。
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// 内容任务总数，至少2，保留一个给用户读取。
    pub content: usize,
    /// 元数据任务数，至少1。
    pub metadata: usize,
    /// 哈希任务数，至少1。
    pub hash: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            content: 4,
            metadata: 1,
            hash: 2,
        }
    }
}
/// 随机数由应用入口提供；核心不读取平台随机源。
pub trait Jitter: Send + Sync {
    /// 返回独立随机样本；仅影响退避，不影响内容或操作身份。
    fn sample(&self) -> u64;
}
/// 每次启动前复核绑定控制与条目围栏。
pub trait DispatchGate: Send + Sync {
    /// false 保留任务且不启动；必须据持久状态检查暂停/排除/解绑。
    fn allowed<'a>(&'a self, op: &'a QueuedOperation) -> PortFuture<'a, Result<bool, PortError>>;
}
/// 与 S054 上传/下载直接共享的活动任务取消信号。
pub use vaultsyncd_transfer::concurrency::CancelToken as Cancellation;
/// 传输失败的调度证据，不把诊断字符串用作分类。
#[derive(Clone, Debug)]
pub enum TaskError {
    /// 取消保留队列与续传，不记作成功或失败尝试。
    Cancelled,
    /// 本地权限故障：通知但允许低频复查；不能用于HTTP403。
    LocalPermission(ActionFailure),
    /// 实际失败和可选服务端等待下界。
    Failed {
        /// 稳定错误码与诊断。
        failure: ActionFailure,
        /// 服务端 Retry-After，下界不受本地60秒封顶影响。
        retry_after: Option<Duration>,
        /// 普通断网持续可见但静默；用户打开仍由本轮结果即时说明。
        offline: bool,
    },
}
impl From<ActionFailure> for TaskError {
    fn from(failure: ActionFailure) -> Self {
        use crate::executor::FailureScheduling;
        let (retry_after, offline) = match failure.scheduling {
            FailureScheduling::Unavailable { retry_after } => (retry_after, true),
            FailureScheduling::Cancelled => return Self::Cancelled,
            FailureScheduling::LocalPermission => return Self::LocalPermission(failure),
            FailureScheduling::ByCode => (None, failure.code == ErrorCode::PlaceholderOffline),
        };
        Self::Failed {
            failure,
            retry_after,
            offline,
        }
    }
}
/// 支持取消与 Retry-After 的动作端口；plan_intent 沿用 S055 的先意图契约。
pub trait ScheduledHandler: ActionHandler {
    /// 平台安全点检查取消；执行已完成的不可逆提交必须返回真实 Completed。
    fn execute_scheduled<'a>(
        &'a self,
        ports: &'a Ports,
        op: &'a QueuedOperation,
        intent: Option<&'a ActionIntent>,
        cancel: &'a Cancellation,
    ) -> PortFuture<'a, Result<ActionOutcome, TaskError>>;
}
/// 将已有 ActionHandler 接入调度器；仅能在动作开始前取消。
/// 需要活动传输取消和 Retry-After 的生产处理器应直接实现 ScheduledHandler。
pub struct ActionAdapter<H>(pub H);
impl<H: ActionHandler> ActionHandler for ActionAdapter<H> {
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
        self.0.execute(ports, op, intent)
    }
}
impl<H: ActionHandler> ScheduledHandler for ActionAdapter<H> {
    fn execute_scheduled<'a>(
        &'a self,
        ports: &'a Ports,
        op: &'a QueuedOperation,
        intent: Option<&'a ActionIntent>,
        cancel: &'a Cancellation,
    ) -> PortFuture<'a, Result<ActionOutcome, TaskError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(TaskError::Cancelled);
            }
            self.0.execute(ports, op, intent).await.map_err(Into::into)
        })
    }
}
/// 队列保留时的可见调度状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskStatus {
    /// 原操作身份。
    pub op: OpId,
    /// 所属绑定。
    pub binding: BindingId,
    /// 优先级。
    pub priority: Priority,
    /// 最早执行时间，UTC秒。
    pub due: UtcTimestamp,
    /// 跨重启保留的取消。
    pub cancelled: bool,
    /// 停止自动重试，需用户恢复或重新规划。
    pub halted: bool,
    /// 网络、超时或远端服务暂不可用，不据此断言主机断网。
    pub offline: bool,
    /// 已持久失败次数。
    pub attempts: u32,
    /// 最近错误。
    pub error: Option<ErrorCode>,
}
impl TaskStatus {
    /// 严重失败进入问题页；普通断网不反复通知。
    pub fn needs_attention(&self) -> bool {
        self.error.is_some_and(|e| e.class().is_severe())
    }
}
/// 本轮动作的实际结果；没有把 pending 解释成成功。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 动作管线期间暂停/排除/解绑，不启动平台动作也不持久取消。
    Deferred,
    /// 基线或服务器已确认提交，并已出队。
    Completed,
    /// 尚未 committed，任务保留。
    Pending,
    /// 失败已原子持久化，可能退避或停止。
    Failed,
    /// 取消已持久化且保留任务。
    Cancelled,
}
/// 本轮已启动动作的报告。
#[derive(Clone, Debug, Default)]
pub struct RoundReport {
    /// 完成顺序，不是入队顺序。
    pub tasks: Vec<(OpId, Outcome)>,
    /// 尚在退避任务的最早到期；None 时等待外部唤醒而非忙循环。
    pub next_due: Option<UtcTimestamp>,
}

impl DispatchGate for crate::bindings::control::BindingControls {
    fn allowed<'a>(&'a self, op: &'a QueuedOperation) -> PortFuture<'a, Result<bool, PortError>> {
        Box::pin(async move {
            Ok(self.execution_gate(op.binding, op.entry).await?
                == crate::bindings::control::ExecutionGate::Ready)
        })
    }
}
