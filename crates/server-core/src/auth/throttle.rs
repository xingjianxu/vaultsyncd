//! 登录失败限流（design §17.1“登录失败限流”、§15.4 的 429 `RATE_LIMITED`）。
//!
//! 纯内存、按登录名结算、按服务端时间判断：窗口内连续失败达到阈值后，该登录名在锁定期内不再参与
//! 比对（[`crate::auth::login`] 因此连长什么样都不查）。这样做的代价与边界都写清楚：
//!
//! - **进程内状态**：重启清零。限流是“降低暴力破解与放大攻击的速率”，不是持久封禁；需要跨重启的
//!   封禁要走持久化与运维介入，属另一件事。
//! - **按登录名而不是按来源地址**：来源地址（IP、代理头）只有在 HTTP 层才拿得到，且 design §17.2
//!   明确“不盲目信任任意 `X-Forwarded-*`”；按来源地址的限流属接入层/后续卡，本模块不猜客户端地址。
//! - **对不存在的登录名同样计数**：否则“被限流”本身就变成账号存在的信号。
//! - **表有上限**：只按登录名累积会让任意登录名把内存撑大，因此超过 [`MAX_TRACKED_LOGINS`] 时先清理
//!   窗口已过的条目；清不干净的极坏情况下继续按窗口淘汰最久未出现的条目（不会静默放行：条目被淘汰
//!   后计数从 0 开始，这是内存上限换来的已知取舍，不假装是完整的持久限流）。
//!
//! 时间由调用方给出（[`UtcTimestamp`]，服务端时钟），本模块不读时钟，也不使用单调时钟差值——同一
//! 输入必须得到同一结论，测试才能锁定行为。

use vaultsyncd_domain::version::UtcTimestamp;

/// 限流参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThrottlePolicy {
    /// 窗口内允许的连续失败次数。
    pub max_failures: u32,
    /// 失败计数窗口（秒）。
    pub window_seconds: u64,
    /// 达到阈值后的锁定时长（秒）。
    pub lockout_seconds: u64,
}

impl Default for ThrottlePolicy {
    /// 5 次失败 / 5 分钟窗口 / 锁定 5 分钟：单用户部署下“手误够用、暴力破解很慢”。
    fn default() -> Self {
        Self {
            max_failures: 5,
            window_seconds: 300,
            lockout_seconds: 300,
        }
    }
}

/// 同时跟踪的登录名上限。
pub const MAX_TRACKED_LOGINS: usize = 1024;

/// 被限流的结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThrottleDenied {
    /// 建议的重试间隔（秒，至少 1）。
    pub retry_after_seconds: u64,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    failures: u32,
    window_start: UtcTimestamp,
    locked_until: Option<UtcTimestamp>,
    last_seen: UtcTimestamp,
}

/// 登录失败限流器。
#[derive(Debug)]
pub struct LoginThrottle {
    policy: ThrottlePolicy,
    entries: std::collections::HashMap<String, Entry>,
}

impl LoginThrottle {
    /// 用给定参数构造空的限流器。
    #[must_use]
    pub fn new(policy: ThrottlePolicy) -> Self {
        Self {
            policy,
            entries: std::collections::HashMap::new(),
        }
    }

    /// 当前参数。
    #[must_use]
    pub fn policy(&self) -> ThrottlePolicy {
        self.policy
    }

    /// 是否允许该登录名现在参与比对；被锁定时给出剩余秒数。
    ///
    /// # Errors
    ///
    /// 锁定未到期时返回 [`ThrottleDenied`]；锁定期满后自动放行（并把该条目重置）。
    pub fn check(&mut self, login: &str, now: UtcTimestamp) -> Result<(), ThrottleDenied> {
        let Some(entry) = self.entries.get_mut(login) else {
            return Ok(());
        };
        match entry.locked_until {
            Some(until) if until.as_unix_seconds() > now.as_unix_seconds() => {
                let remaining = until.as_unix_seconds() - now.as_unix_seconds();
                Err(ThrottleDenied {
                    retry_after_seconds: u64::try_from(remaining).unwrap_or(1).max(1),
                })
            }
            Some(_) => {
                // 锁定期满：计数从头开始，不把上一次锁定期算成新一轮失败。
                entry.locked_until = None;
                entry.failures = 0;
                entry.window_start = now;
                entry.last_seen = now;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// 记一次失败；达到阈值即进入锁定期。
    pub fn record_failure(&mut self, login: &str, now: UtcTimestamp) {
        self.prune_if_full(now);
        let policy = self.policy;
        let entry = self.entries.entry(login.to_owned()).or_insert(Entry {
            failures: 0,
            window_start: now,
            locked_until: None,
            last_seen: now,
        });
        entry.last_seen = now;
        if now.as_unix_seconds() - entry.window_start.as_unix_seconds()
            >= i64::try_from(policy.window_seconds).unwrap_or(i64::MAX)
        {
            entry.failures = 0;
            entry.window_start = now;
        }
        entry.failures = entry.failures.saturating_add(1);
        if entry.failures >= policy.max_failures {
            entry.failures = 0;
            entry.window_start = now;
            entry.locked_until = Some(offset(now, policy.lockout_seconds));
        }
    }

    /// 记一次成功：清掉该登录名的失败计数与锁定。
    pub fn record_success(&mut self, login: &str) {
        self.entries.remove(login);
    }

    /// 当前跟踪的登录名数量（测试与指标用）。
    #[must_use]
    pub fn tracked_logins(&self) -> usize {
        self.entries.len()
    }

    fn prune_if_full(&mut self, now: UtcTimestamp) {
        if self.entries.len() < MAX_TRACKED_LOGINS {
            return;
        }
        let window = i64::try_from(self.policy.window_seconds).unwrap_or(i64::MAX);
        self.entries.retain(|_, entry| {
            entry.locked_until.is_some()
                || now.as_unix_seconds() - entry.window_start.as_unix_seconds() < window
        });
        if self.entries.len() >= MAX_TRACKED_LOGINS {
            // 仍然满：淘汰最久未出现的一条。计数从 0 开始是内存上限的已知取舍（见模块文档）。
            if let Some(stale) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen.as_unix_seconds())
                .map(|(login, _)| login.clone())
            {
                self.entries.remove(&stale);
            }
        }
    }
}

fn offset(now: UtcTimestamp, seconds: u64) -> UtcTimestamp {
    let seconds = i64::try_from(seconds).unwrap_or(i64::MAX);
    UtcTimestamp::from_unix_seconds(now.as_unix_seconds().saturating_add(seconds)).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::user_time as seconds;

    #[test]
    fn threshold_locks_then_releases_after_the_lockout() {
        let mut throttle = LoginThrottle::new(ThrottlePolicy {
            max_failures: 3,
            window_seconds: 60,
            lockout_seconds: 120,
        });
        for at in [0, 1, 2] {
            assert!(
                throttle.check("owner", seconds(at)).is_ok(),
                "第 {at} 秒尚未达到阈值"
            );
            throttle.record_failure("owner", seconds(at));
        }
        // 第三次失败（at = 2）达到阈值 → 从那一刻起锁定 120 秒。
        let denied = throttle
            .check("owner", seconds(3))
            .expect_err("达到阈值后必须被限流");
        assert_eq!(denied.retry_after_seconds, 119);
        assert_eq!(
            throttle
                .check("owner", seconds(121))
                .expect_err("锁定期内")
                .retry_after_seconds,
            1
        );
        assert!(
            throttle.check("owner", seconds(122)).is_ok(),
            "锁定期满后放行"
        );
        // 放行后计数已重置：一次失败不重新锁定。
        throttle.record_failure("owner", seconds(124));
        assert!(throttle.check("owner", seconds(125)).is_ok());
    }

    #[test]
    fn failures_outside_the_window_do_not_accumulate() {
        let mut throttle = LoginThrottle::new(ThrottlePolicy {
            max_failures: 2,
            window_seconds: 60,
            lockout_seconds: 60,
        });
        throttle.record_failure("owner", seconds(0));
        throttle.record_failure("owner", seconds(61));
        assert!(
            throttle.check("owner", seconds(62)).is_ok(),
            "窗口外的失败不计入"
        );
    }

    #[test]
    fn success_clears_the_counter() {
        let mut throttle = LoginThrottle::new(ThrottlePolicy::default());
        throttle.record_failure("owner", seconds(0));
        throttle.record_success("owner");
        assert_eq!(throttle.tracked_logins(), 0);
    }

    #[test]
    fn unknown_logins_are_tracked_too() {
        let mut throttle = LoginThrottle::new(ThrottlePolicy {
            max_failures: 2,
            window_seconds: 60,
            lockout_seconds: 60,
        });
        throttle.record_failure("nobody", seconds(0));
        throttle.record_failure("nobody", seconds(1));
        assert!(
            throttle.check("nobody", seconds(2)).is_err(),
            "不存在的登录名同样被限流，避免限流本身成为账号存在性信号"
        );
    }

    #[test]
    fn the_table_does_not_grow_without_bound() {
        let mut throttle = LoginThrottle::new(ThrottlePolicy {
            max_failures: 5,
            window_seconds: 60,
            lockout_seconds: 60,
        });
        for index in 0..(MAX_TRACKED_LOGINS + 10) {
            throttle.record_failure(&format!("login-{index}"), seconds(0));
        }
        assert!(
            throttle.tracked_logins() <= MAX_TRACKED_LOGINS,
            "跟踪表必须有上限，实际 {}",
            throttle.tracked_logins()
        );
    }
}
