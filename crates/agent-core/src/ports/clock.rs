//! 时钟端口（design §5.4：领域层不读时钟；§13.3 退避由调度器决定）。
//!
//! 时间点的唯一表示是 [`vaultsyncd_domain::version::UtcTimestamp`]（Unix 秒），本模块不另造一个
//! “客户端时间”类型。客户端时钟只用于**调度**（退避是否到期、下一次轮询时间），从不用于判断
//! 内容新旧或冲突胜负（design I04：时间戳只用于展示与检测候选变化）。
//!
//! 端口自身不 sleep：执行器只报告“现在能不能重试、最早什么时候到期”，真正等待属 S062 的调度器。
//! 这样退避规则可以在测试里用固定时钟完整验证，不依赖真实等待。

use vaultsyncd_domain::version::{BoundError, Clock as DomainClock, UtcTimestamp};

/// 当前时间来源端口。
///
/// 这是对 [`vaultsyncd_domain::version::Clock`] 的 `Send + Sync` 包装：领域 trait 只要求“能读
/// 时间”，执行器还要把它放进 `Arc<dyn Clock>` 跨任务共享，因此需要线程安全约束。实现任意领域
/// 时钟并满足 `Send + Sync` 即自动成为本端口，不需要第二套 `now()` 方法。
pub trait Clock: DomainClock + Send + Sync {}

impl<T: DomainClock + Send + Sync> Clock for T {}

/// 读当前时间，把领域错误转成端口错误（系统时钟早于 1970 是环境问题，不是可重试的网络故障）。
///
/// # Errors
///
/// 时钟不可信时返回 [`vaultsyncd_transfer::ports::PortError::Local`]（读失败），调用方按“本机状态
/// 暂不可用”处理，不编造时刻。
pub fn now(clock: &dyn Clock) -> Result<UtcTimestamp, crate::ports::PortError> {
    clock.now_utc().map_err(|error| {
        crate::ports::PortError::local(
            crate::ports::LocalErrorKind::Read,
            format!("读取系统时钟失败：{error}"),
        )
    })
}

/// 系统时钟错误在端口层的统一文案（避免每个适配器各写一份）。
#[must_use]
pub fn describe(error: &BoundError) -> String {
    format!("读取系统时钟失败：{error}")
}
