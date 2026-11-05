//! 首次 owner 初始化用例（design §17.1、C27）：一次性、离线、空状态、无默认密码。
//!
//! 顺序固定，任何一步失败都不写库：
//!
//! 1. **空状态**：账号数为 0。已有账号时一律 [`InitError::AlreadyInitialized`]——重复 init 不覆盖
//!    现有账号（`password_hash` 非空约束让它不可能出现“半初始化”行，见迁移 0002）。
//! 2. **输入形态**：登录名与密码先按本文件的规则校验。没有默认密码、没有默认登录名，也没有
//!    “密码为空就随机生成”的兜底。
//! 3. **散列**：Argon2id + 独立随机盐（[`crate::auth::password`]）。
//! 4. **写入**：交出 [`NewAccount`] 由调用方在自己持有的事务里写；写失败时的错误如实返回。
//!
//! 这里不做“服务端是否在运行”的判断——那由状态目录锁（`vaultsyncd_server_store::state_lock`）在
//! 进程入口拒绝，比在用例里读一个表更像事实。

use vaultsyncd_domain::ids::{IdSource, UserId};

use crate::auth::account::{AccountError, NewAccount, OwnerAccounts};
use crate::auth::password::{PasswordError, PasswordHasher};

/// 密码的最小字符数。
///
/// design 没有给出数值，这里取工程基线：12 个字符（高于 NIST SP 800-63B 的 8 字符下限），并把它
/// 集中成常量，等实测与 ADR 结论落地时只改这里。判定按 Unicode 标量值个数，不按字节——中文
/// 口令不该因为编码变长而被要求更短。
pub const MIN_PASSWORD_CHARS: usize = 12;

/// 密码的最大字节数：Argon2 本身的限制远大于此，这里挡住明显的“把文件当密码喂进来”。
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// 登录名的最大字符数。
pub const MAX_LOGIN_CHARS: usize = 64;

/// 初始化请求（离线命令从 stdin 读到的值）。
#[derive(Clone, Copy, Debug)]
pub struct NewOwnerRequest<'a> {
    /// 登录名。
    pub login: &'a str,
    /// 明文密码；只在本次调用内存在，不写日志、不落盘。
    pub password: &'a str,
}

/// 已经算好散列、等待写入的 owner。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedOwner {
    /// 新账号身份。
    pub id: UserId,
    /// 登录名。
    pub login: String,
    /// Argon2id PHC 文本。
    pub password_hash: String,
}

impl PreparedOwner {
    /// 转成端口要求的写入形态。
    #[must_use]
    pub fn new_account(&self) -> NewAccount {
        NewAccount {
            id: self.id,
            login: self.login.clone(),
            password_hash: self.password_hash.clone(),
        }
    }
}

/// 初始化的失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitError {
    /// 已经有账号：拒绝覆盖。
    AlreadyInitialized {
        /// 已有账号数量。
        existing: u64,
    },
    /// 登录名形态不合规。
    InvalidLogin {
        /// 具体原因（稳定短名，便于命令输出与验收断言）。
        reason: &'static str,
    },
    /// 密码形态不合规。
    WeakPassword {
        /// 具体原因（稳定短名）。
        reason: &'static str,
    },
    /// 散列计算失败。
    Password(PasswordError),
    /// 账号端口失败。
    Accounts(AccountError),
}

impl InitError {
    /// 稳定错误码（命令输出与验收脚本共用）。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::AlreadyInitialized { .. } => "already_initialized",
            Self::InvalidLogin { .. } => "invalid_login",
            Self::WeakPassword { .. } => "weak_password",
            Self::Password(PasswordError::InvalidParams { .. }) => "password_params_invalid",
            Self::Password(PasswordError::HashFailed { .. }) => "password_hash_failed",
            Self::Accounts(error) => error.code(),
        }
    }
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyInitialized { existing } => write!(
                f,
                "已有 {existing} 个账号，拒绝重复初始化（本命令只用于首次 owner 初始化）"
            ),
            Self::InvalidLogin { reason } => write!(f, "登录名不合规：{reason}"),
            Self::WeakPassword { reason } => write!(f, "密码不合规：{reason}"),
            Self::Password(error) => write!(f, "{error}"),
            Self::Accounts(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for InitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Password(error) => Some(error),
            Self::Accounts(error) => Some(error),
            _ => None,
        }
    }
}

/// 校验登录名形态：非空、不超过 [`MAX_LOGIN_CHARS`]、不含控制字符。
///
/// 不做大小写折叠或 Unicode 规范化（design §7.2 的教训：不偷偷规范化），也不限制字符集——登录名
/// 是数据，按原样保存与比较。
pub fn validate_login(login: &str) -> Result<(), &'static str> {
    if login.is_empty() {
        return Err("登录名不能为空");
    }
    if login.chars().count() > MAX_LOGIN_CHARS {
        return Err("登录名超过 64 个字符");
    }
    if login.chars().any(char::is_control) {
        return Err("登录名不能包含控制字符");
    }
    Ok(())
}

/// 校验密码形态：字符数下限与字节数上限。
pub fn validate_password(password: &str) -> Result<(), &'static str> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err("密码至少需要 12 个字符");
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err("密码超过 1024 字节");
    }
    Ok(())
}

/// 准备一个 owner：检查空状态与输入形态，算出散列，返回待写入的账号。
///
/// 调用方必须把它返回的 [`PreparedOwner`] 写进**与 [`OwnerAccounts::count`] 同一个事务**——这样
/// “空表”的结论在写入前不会失效（适配器约定见 [`crate::auth::account`]）。
///
/// # Errors
///
/// 见 [`InitError`]：已有账号、输入不合规、散列失败或账号端口失败时返回错误，且不产生任何写入。
pub async fn prepare_owner(
    accounts: &mut impl OwnerAccounts,
    hasher: &PasswordHasher,
    ids: &mut impl IdSource,
    request: NewOwnerRequest<'_>,
) -> Result<PreparedOwner, InitError> {
    let existing = accounts.count().await.map_err(InitError::Accounts)?;
    if existing > 0 {
        return Err(InitError::AlreadyInitialized { existing });
    }
    validate_login(request.login).map_err(|reason| InitError::InvalidLogin { reason })?;
    validate_password(request.password).map_err(|reason| InitError::WeakPassword { reason })?;

    let password_hash = hasher.hash(request.password).map_err(InitError::Password)?;
    Ok(PreparedOwner {
        id: UserId::generate(ids),
        login: request.login.to_owned(),
        password_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{CountingIds, FakeAccounts, cheap_hasher};

    #[tokio::test]
    async fn first_init_produces_an_argon2id_hash_and_writes_nothing_on_its_own() {
        let mut accounts = FakeAccounts::default();
        let mut ids = CountingIds::new(0x40);
        let prepared = prepare_owner(
            &mut accounts,
            &cheap_hasher(),
            &mut ids,
            NewOwnerRequest {
                login: "owner",
                password: "correct horse battery staple",
            },
        )
        .await
        .expect("首次初始化应成功");
        assert_eq!(prepared.login, "owner");
        assert!(
            prepared.password_hash.starts_with("$argon2id$"),
            "{}",
            prepared.password_hash
        );
        assert_eq!(
            accounts.count().await.expect("计数"),
            0,
            "用例只准备，不自己写库"
        );
    }

    #[tokio::test]
    async fn second_init_is_refused_without_touching_the_existing_account() {
        let mut accounts = FakeAccounts::default();
        let mut ids = CountingIds::new(0x40);
        let first = prepare_owner(
            &mut accounts,
            &cheap_hasher(),
            &mut ids,
            NewOwnerRequest {
                login: "owner",
                password: "correct horse battery staple",
            },
        )
        .await
        .expect("首次初始化");
        accounts.insert(first.new_account()).await.expect("写账号");

        match prepare_owner(
            &mut accounts,
            &cheap_hasher(),
            &mut ids,
            NewOwnerRequest {
                login: "owner",
                password: "another long enough password",
            },
        )
        .await
        {
            Err(InitError::AlreadyInitialized { existing }) => assert_eq!(existing, 1),
            other => panic!("重复初始化必须被拒绝：{other:?}"),
        }
        assert_eq!(
            accounts
                .find_by_login("owner")
                .await
                .expect("读账号")
                .expect("账号仍在")
                .password_hash,
            first.password_hash,
            "拒绝覆盖意味着旧散列原样保留"
        );
    }

    #[tokio::test]
    async fn invalid_login_and_weak_password_are_refused_before_hashing() {
        let mut accounts = FakeAccounts::default();
        let mut ids = CountingIds::new(0x40);
        for (login, password, expected) in [
            ("", "correct horse battery staple", "invalid_login"),
            (
                "owner\nname",
                "correct horse battery staple",
                "invalid_login",
            ),
            ("owner", "short", "weak_password"),
        ] {
            let error = prepare_owner(
                &mut accounts,
                &cheap_hasher(),
                &mut ids,
                NewOwnerRequest { login, password },
            )
            .await
            .expect_err("不合规输入必须拒绝");
            assert_eq!(error.code(), expected, "{error}");
        }
    }

    #[tokio::test]
    async fn login_and_password_rules_match_the_documented_limits() {
        assert!(validate_login("owner").is_ok());
        assert!(validate_login(&"a".repeat(MAX_LOGIN_CHARS)).is_ok());
        assert!(validate_login(&"a".repeat(MAX_LOGIN_CHARS + 1)).is_err());
        assert!(validate_password(&"a".repeat(MIN_PASSWORD_CHARS)).is_ok());
        assert!(validate_password(&"a".repeat(MIN_PASSWORD_CHARS - 1)).is_err());
        // 中文口令按字符数判定，不按字节：12 个汉字通过，11 个不通过。
        assert!(validate_password("口令口令口令口令口令口令").is_ok());
        assert!(validate_password("口令口令口令口令口令").is_err());
    }
}
