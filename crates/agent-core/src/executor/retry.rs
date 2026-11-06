//! 有界重试策略（design §13.3）。
//!
//! 执行器只决定“还要不要再试、最早何时再试”；真正的等待与随机抖动属 S062 的调度器。因此这里的
//! 规则可以在固定时钟下完整验证，不需要真实等待。
//!
//! 不变量：
//!
//! - **次数有界**：`max_attempts` 必须 ≥ 1 且不超过 [`MAX_ATTEMPTS_CAP`]；执行器从不重试超过这个
//!   次数，跨重启的累计次数来自队列的 `attempts` 列。
//! - **退避只增不减**：第 n 次失败后的等待是 `base * 2^(n-1)`，封顶 `max_delay`；不加随机抖动
//!   （S062 负责），因此同一输入得到同一到期时间。
//! - **配置错误早失败**：0 次尝试、0 延迟或上限小于起点都在构造时拒绝，不静默改成“默认值”。

use std::time::Duration;

/// 允许配置的最大尝试次数：防止退避参数被写成“无限重试”。
pub const MAX_ATTEMPTS_CAP: u32 = 16;

/// 默认最多尝试次数（首次 + 两次重试）。
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// 默认退避起点。
pub const DEFAULT_BASE_DELAY: Duration = Duration::from_secs(1);
/// 默认退避上限（design §13.3 建议 1 秒起、60 秒封顶）。
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(60);

/// 重试策略参数不合法。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryPolicyError {
    /// 尝试次数为 0：任何操作都不会被执行。
    ZeroAttempts,
    /// 尝试次数超过 [`MAX_ATTEMPTS_CAP`]。
    AttemptsAboveCap {
        /// 允许的上限。
        cap: u32,
    },
    /// 退避起点为 0：会变成忙循环。
    ZeroBaseDelay,
    /// 退避上限小于起点。
    MaxBelowBase,
}

impl std::fmt::Display for RetryPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroAttempts => f.write_str("重试次数至少为 1"),
            Self::AttemptsAboveCap { cap } => {
                write!(f, "重试次数不能超过 {cap}")
            }
            Self::ZeroBaseDelay => f.write_str("退避起点必须大于 0"),
            Self::MaxBelowBase => f.write_str("退避上限不能小于退避起点"),
        }
    }
}

impl std::error::Error for RetryPolicyError {}

/// 有界重试策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
}

impl RetryPolicy {
    /// 默认策略：最多 3 次，1 秒起、60 秒封顶。
    #[must_use]
    pub const fn default_policy() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            base_delay: DEFAULT_BASE_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
        }
    }

    /// 构造并校验参数。
    ///
    /// # Errors
    ///
    /// 见 [`RetryPolicyError`]；非法参数不会退化成“默认值”。
    pub fn new(
        max_attempts: u32,
        base_delay: Duration,
        max_delay: Duration,
    ) -> Result<Self, RetryPolicyError> {
        if max_attempts == 0 {
            return Err(RetryPolicyError::ZeroAttempts);
        }
        if max_attempts > MAX_ATTEMPTS_CAP {
            return Err(RetryPolicyError::AttemptsAboveCap {
                cap: MAX_ATTEMPTS_CAP,
            });
        }
        if base_delay.is_zero() {
            return Err(RetryPolicyError::ZeroBaseDelay);
        }
        if max_delay < base_delay {
            return Err(RetryPolicyError::MaxBelowBase);
        }
        Ok(Self {
            max_attempts,
            base_delay,
            max_delay,
        })
    }

    /// 每个操作最多执行的次数（含首次）。
    #[must_use]
    pub const fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// 已失败 `attempts` 次后是否不再自动重试。
    #[must_use]
    pub const fn exhausted(&self, attempts: u32) -> bool {
        attempts >= self.max_attempts
    }

    /// 第 `failures` 次失败后的等待时间；`failures` 为 0 时按 1 次处理。
    #[must_use]
    pub fn delay_after(&self, failures: u32) -> Duration {
        let exponent = failures.max(1).saturating_sub(1).min(31);
        let factor = 1u32 << exponent;
        let delay = self.base_delay.saturating_mul(factor);
        if delay > self.max_delay {
            self.max_delay
        } else {
            delay
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::default_policy()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempts_are_bounded_by_construction() {
        assert_eq!(
            RetryPolicy::new(0, DEFAULT_BASE_DELAY, DEFAULT_MAX_DELAY),
            Err(RetryPolicyError::ZeroAttempts)
        );
        assert_eq!(
            RetryPolicy::new(MAX_ATTEMPTS_CAP + 1, DEFAULT_BASE_DELAY, DEFAULT_MAX_DELAY),
            Err(RetryPolicyError::AttemptsAboveCap {
                cap: MAX_ATTEMPTS_CAP
            })
        );
        assert_eq!(
            RetryPolicy::new(2, Duration::ZERO, DEFAULT_MAX_DELAY),
            Err(RetryPolicyError::ZeroBaseDelay)
        );
        assert_eq!(
            RetryPolicy::new(2, Duration::from_secs(5), Duration::from_secs(1)),
            Err(RetryPolicyError::MaxBelowBase)
        );
        let policy =
            RetryPolicy::new(2, Duration::from_secs(5), Duration::from_secs(5)).expect("合法策略");
        assert_eq!(policy.max_attempts(), 2);
    }

    #[test]
    fn backoff_doubles_and_stops_at_the_cap() {
        let policy = RetryPolicy::default_policy();
        assert_eq!(policy.delay_after(0), Duration::from_secs(1));
        assert_eq!(policy.delay_after(1), Duration::from_secs(1));
        assert_eq!(policy.delay_after(2), Duration::from_secs(2));
        assert_eq!(policy.delay_after(3), Duration::from_secs(4));
        assert_eq!(policy.delay_after(6), Duration::from_secs(32));
        assert_eq!(policy.delay_after(7), Duration::from_secs(60), "封顶");
        assert_eq!(policy.delay_after(60), Duration::from_secs(60), "不溢出");
        assert_eq!(policy.delay_after(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn exhaustion_is_a_function_of_the_persisted_attempt_count() {
        let policy =
            RetryPolicy::new(3, Duration::from_secs(1), Duration::from_secs(10)).expect("合法策略");
        assert!(!policy.exhausted(0));
        assert!(!policy.exhausted(1));
        assert!(!policy.exhausted(2));
        assert!(policy.exhausted(3));
        assert!(policy.exhausted(4), "跨重启累计的次数同样有界");
    }
}
