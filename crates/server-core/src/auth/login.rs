//! 登录校验用例（design §15.2 `POST /v1/auth/login`、§17.1、C27）：凭据比对 + 限流 + 统一失败。
//!
//! 判定顺序固定：**先限流、再查账号、再比密码**。先限流是因为“这个登录名到底存不存在”本身就是要
//! 保护的信息；先查账号再比密码会给出一个可以按响应时间量出来的旁路。
//!
//! 不变量：
//!
//! - **对外的失败是统一的**：账号不存在、密码错、账号未启用都返回
//!   [`LoginError::InvalidCredentials`]；不存在账号时也做一次等代价的散列计算
//!   （[`PasswordHasher::dummy_verify`]），不让响应时间泄露“这个登录名存在”。
//! - **限流在比对之前**：被限流的请求不查账号、不做散列，因此限流本身也不提供账号存在性信号。
//! - **散列坏掉不是密码错**：库里存的散列解析不出或不是 Argon2id 时返回
//!   [`LoginError::AccountBroken`]（内部错误），而不是让用户一直重试密码。它是唯一可区分账号存在
//!   的失败，且只能由数据损坏触发，调用方必须把它记进日志而不是透传成“凭据无效”。
//! - **成功不发放凭据**：会话与刷新凭据属于 S043；本用例只回答“这组凭据属于哪个账号”。

use vaultsyncd_domain::version::UtcTimestamp;

use crate::auth::account::{AccountError, OwnerAccounts};
use crate::auth::password::{PasswordHasher, PasswordVerdict};
use crate::auth::throttle::LoginThrottle;

/// 一次登录尝试的输入。
#[derive(Clone, Copy, Debug)]
pub struct LoginAttempt<'a> {
    /// 提交的登录名（原样比较，不做大小写折叠）。
    pub login: &'a str,
    /// 提交的明文密码；只在本次调用内存在。
    pub password: &'a str,
    /// 服务端当前时间（限流窗口按服务端时间计算，不信任客户端时钟）。
    pub now: UtcTimestamp,
}

/// 校验通过后交给调用方的账号事实（不含任何凭据）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedAccount {
    /// 账号身份。
    pub id: vaultsyncd_domain::ids::UserId,
    /// 登录名（数据库原文）。
    pub login: String,
}

/// 登录的失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginError {
    /// 凭据无效（不存在账号、密码错、账号未启用三者不区分）。
    InvalidCredentials,
    /// 触发限流：在 `retry_after_seconds` 之前同一登录名不再比对。
    RateLimited {
        /// 建议的重试间隔（秒）。
        retry_after_seconds: u64,
    },
    /// 账号端口失败（存储读不出来）。
    Accounts(AccountError),
    /// 已存散列坏了：数据问题，不是用户输入问题。
    AccountBroken {
        /// 原因文本（不含散列本身）。
        detail: String,
    },
}

impl LoginError {
    /// 稳定错误码（日志、API 错误映射与验收脚本共用）。
    ///
    /// `invalid_credentials` 映射 401，`rate_limited` 映射 429 `RATE_LIMITED`（design §15.4）。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidCredentials => "invalid_credentials",
            Self::RateLimited { .. } => "rate_limited",
            Self::Accounts(error) => error.code(),
            Self::AccountBroken { .. } => "account_hash_broken",
        }
    }

    /// 对外可安全返回给未认证调用方的重试建议（秒）。
    #[must_use]
    pub fn retry_after_seconds(&self) -> Option<u64> {
        match self {
            Self::RateLimited {
                retry_after_seconds,
            } => Some(*retry_after_seconds),
            _ => None,
        }
    }
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCredentials => f.write_str("登录名或密码不正确"),
            Self::RateLimited {
                retry_after_seconds,
            } => write!(f, "登录尝试过于频繁，请 {retry_after_seconds} 秒后再试"),
            Self::Accounts(error) => write!(f, "{error}"),
            Self::AccountBroken { detail } => write!(f, "账号的密码散列无法使用：{detail}"),
        }
    }
}

impl std::error::Error for LoginError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Accounts(error) => Some(error),
            _ => None,
        }
    }
}

/// 校验一组凭据；成功时记一次成功（清掉该登录名的失败计数）。
///
/// # Errors
///
/// 见 [`LoginError`]：限流、凭据无效、账号端口失败或散列损坏时返回错误，且不返回任何账号事实。
pub async fn authenticate(
    accounts: &mut impl OwnerAccounts,
    hasher: &PasswordHasher,
    throttle: &mut LoginThrottle,
    attempt: LoginAttempt<'_>,
) -> Result<AuthenticatedAccount, LoginError> {
    if let Err(denied) = throttle.check(attempt.login, attempt.now) {
        return Err(LoginError::RateLimited {
            retry_after_seconds: denied.retry_after_seconds,
        });
    }

    let stored = accounts
        .find_by_login(attempt.login)
        .await
        .map_err(|error| match error {
            AccountError::Malformed { detail } => LoginError::AccountBroken { detail },
            other => LoginError::Accounts(other),
        })?;

    let Some(account) = stored else {
        // 不存在账号也付一次同等代价，避免按响应时间区分“没这个登录名”和“密码错”。
        hasher.dummy_verify(attempt.password);
        throttle.record_failure(attempt.login, attempt.now);
        return Err(LoginError::InvalidCredentials);
    };

    match hasher.verify(&account.password_hash, attempt.password) {
        PasswordVerdict::Matched if account.active => {
            throttle.record_success(attempt.login);
            Ok(AuthenticatedAccount {
                id: account.id,
                login: account.login,
            })
        }
        PasswordVerdict::Matched => {
            // 密码对但账号不可用：对外仍是统一失败，不透露“密码是对的”。
            throttle.record_failure(attempt.login, attempt.now);
            Err(LoginError::InvalidCredentials)
        }
        PasswordVerdict::Mismatched => {
            throttle.record_failure(attempt.login, attempt.now);
            Err(LoginError::InvalidCredentials)
        }
        PasswordVerdict::Malformed { detail } => {
            throttle.record_failure(attempt.login, attempt.now);
            Err(LoginError::AccountBroken { detail })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::auth::throttle::ThrottlePolicy;
    use crate::test_support::{FakeAccounts, cheap_hasher, stored_owner, user_time as seconds};

    fn attempt<'a>(login: &'a str, password: &'a str, at: i64) -> LoginAttempt<'a> {
        LoginAttempt {
            login,
            password,
            now: seconds(at),
        }
    }

    #[tokio::test]
    async fn correct_password_returns_the_account_and_clears_failures() {
        let hasher = cheap_hasher();
        let mut accounts = FakeAccounts::default();
        accounts.insert_stored(stored_owner(
            &hasher,
            0x11,
            "owner",
            "correct horse battery staple",
        ));
        let mut throttle = LoginThrottle::new(ThrottlePolicy::default());

        // 先失败一次，再用正确密码：成功必须清掉失败计数。
        assert_eq!(
            authenticate(
                &mut accounts,
                &hasher,
                &mut throttle,
                attempt("owner", "wrong password here", 100),
            )
            .await
            .expect_err("错误密码必须失败"),
            LoginError::InvalidCredentials
        );
        let authenticated = authenticate(
            &mut accounts,
            &hasher,
            &mut throttle,
            attempt("owner", "correct horse battery staple", 101),
        )
        .await
        .expect("正确密码应通过");
        assert_eq!(authenticated.login, "owner");
        assert_eq!(throttle.tracked_logins(), 0);
    }

    #[tokio::test]
    async fn unknown_login_and_wrong_password_are_indistinguishable() {
        let hasher = cheap_hasher();
        let mut accounts = FakeAccounts::default();
        accounts.insert_stored(stored_owner(
            &hasher,
            0x11,
            "owner",
            "correct horse battery staple",
        ));

        let unknown = authenticate(
            &mut accounts,
            &hasher,
            &mut LoginThrottle::new(ThrottlePolicy::default()),
            attempt("nobody", "correct horse battery staple", 100),
        )
        .await
        .expect_err("不存在账号必须失败");
        let wrong = authenticate(
            &mut accounts,
            &hasher,
            &mut LoginThrottle::new(ThrottlePolicy::default()),
            attempt("owner", "wrong password here", 100),
        )
        .await
        .expect_err("错误密码必须失败");
        assert_eq!(unknown, LoginError::InvalidCredentials);
        assert_eq!(wrong, LoginError::InvalidCredentials);
        assert_ne!(unknown.code(), "account_hash_broken");
    }

    #[tokio::test]
    async fn inactive_account_fails_with_the_same_error_as_a_wrong_password() {
        let hasher = cheap_hasher();
        let mut accounts = FakeAccounts::default();
        let mut owner = stored_owner(&hasher, 0x11, "owner", "correct horse battery staple");
        owner.active = false;
        accounts.insert_stored(owner);

        let error = authenticate(
            &mut accounts,
            &hasher,
            &mut LoginThrottle::new(ThrottlePolicy::default()),
            attempt("owner", "correct horse battery staple", 100),
        )
        .await
        .expect_err("未启用账号不得登录");
        assert_eq!(error, LoginError::InvalidCredentials);
    }

    #[tokio::test]
    async fn throttled_attempts_do_not_look_up_the_account() {
        let hasher = cheap_hasher();
        let mut accounts = FakeAccounts::default();
        accounts.insert_stored(stored_owner(
            &hasher,
            0x11,
            "owner",
            "correct horse battery staple",
        ));
        let mut throttle = LoginThrottle::new(ThrottlePolicy::default());

        for index in 0..5 {
            let _ = authenticate(
                &mut accounts,
                &hasher,
                &mut throttle,
                attempt("owner", "wrong password here", 100 + index),
            )
            .await;
        }
        let lookups_before = accounts.lookups();
        let error = authenticate(
            &mut accounts,
            &hasher,
            &mut throttle,
            attempt("owner", "correct horse battery staple", 106),
        )
        .await
        .expect_err("被限流时必须拒绝，即使密码正确");
        // 第五次失败发生在第 104 秒，锁定 300 秒 → 第 106 秒还剩 298 秒。
        assert_eq!(
            error,
            LoginError::RateLimited {
                retry_after_seconds: 298
            }
        );
        assert_eq!(
            accounts.lookups(),
            lookups_before,
            "限流判定必须在查账号之前"
        );
    }

    #[tokio::test]
    async fn broken_hash_is_reported_as_data_damage_not_a_wrong_password() {
        let hasher = cheap_hasher();
        let mut accounts = FakeAccounts::default();
        let mut owner = stored_owner(&hasher, 0x11, "owner", "correct horse battery staple");
        owner.password_hash = "not-a-phc-string".to_owned();
        accounts.insert_stored(owner);

        let error = authenticate(
            &mut accounts,
            &hasher,
            &mut LoginThrottle::new(ThrottlePolicy::default()),
            attempt("owner", "correct horse battery staple", 100),
        )
        .await
        .expect_err("坏散列必须报内部错误");
        assert_eq!(error.code(), "account_hash_broken");
    }
}
