//! 账号端口：账号表在用例核心这一侧的最小接口（design §5.4 的端口边界、§17.1、C27）。
//!
//! 端口只暴露“数一数有多少账号、按登录名读一条、写一条”：首次初始化的空状态判断、登录时的比对，
//! 都由 [`crate::auth::init`] 与 [`crate::auth::login`] 决定顺序。适配器（服务端存储层的
//! `users` 表读写）只负责把它做到一张表上，并把存储失败如实带回。
//!
//! 不变量：
//!
//! - **初始化与写账号必须能被放进同一个事务**：端口方法接收 `&mut self`，适配器可以是“持有一个
//!   写事务”的短生命周期对象；空表检查与插入之间不能出现别的写入者，否则两个 init 可以都看到空表。
//! - **不区分“账号不存在”与“账号不可用”之外的细节**：端口只交出 [`StoredAccount`]，是否启用由
//!   `active` 表达；登录对外的结论一律统一（design §15.2：错误不暴露账号是否存在）。
//! - **坏行不猜默认值**：状态文本、散列形态不合规的部分读不出来就报 [`AccountError::Malformed`]，
//!   不允许“猜成可登录”。

use std::fmt;

use vaultsyncd_domain::ids::UserId;

/// 服务端读出的账号（端口形态，不含数据库细节）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredAccount {
    /// 账号身份。
    pub id: UserId,
    /// 登录名（数据库中保存的原始文本）。
    pub login: String,
    /// 密码散列（Argon2id 的 PHC 文本）。
    pub password_hash: String,
    /// 是否允许登录。
    pub active: bool,
}

/// 要写入的新账号。
#[derive(Clone, Debug)]
pub struct NewAccount {
    /// 账号身份。
    pub id: UserId,
    /// 登录名。
    pub login: String,
    /// 密码散列（PHC 文本）。
    pub password_hash: String,
}

/// 账号端口的失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountError {
    /// 存储层失败（SQLite、编码等），保留原因文本。
    Storage {
        /// 原始原因。
        detail: String,
    },
    /// 登录名已被占用（并发初始化或调用方没做空表检查）。
    DuplicateLogin {
        /// 冲突的登录名。
        login: String,
    },
    /// 读到的行无法还原成账号（状态取值或身份长度不合规）。
    Malformed {
        /// 原因文本。
        detail: String,
    },
}

impl AccountError {
    /// 稳定错误码（日志与验收脚本共用）。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Storage { .. } => "account_storage_failed",
            Self::DuplicateLogin { .. } => "duplicate_login",
            Self::Malformed { .. } => "malformed_account",
        }
    }
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage { detail } => write!(f, "账号存储失败：{detail}"),
            Self::DuplicateLogin { login } => write!(f, "登录名 {login:?} 已被占用"),
            Self::Malformed { detail } => write!(f, "账号行无法还原：{detail}"),
        }
    }
}

impl std::error::Error for AccountError {}

/// 账号表的端口契约。
///
/// 三个方法都是 `&mut self`：适配器实例可以持有一次写事务，[`Self::count`] 与 [`Self::insert`] 因此
/// 处在同一事务里（见模块文档的不变量）。
// `#[allow(async_fn_in_trait)]`：与 `ports/` 的端口一样，只在编排函数里以泛型参数使用，不外发、不做 dyn 对象。
#[allow(async_fn_in_trait)]
pub trait OwnerAccounts {
    /// 账号总数（首次初始化判断“空状态”）。
    ///
    /// # Errors
    ///
    /// 存储层失败。
    async fn count(&mut self) -> Result<u64, AccountError>;

    /// 按登录名读账号；没有时返回 `None`（不是错误）。
    ///
    /// # Errors
    ///
    /// 存储层失败，或行无法还原（[`AccountError::Malformed`]）。
    async fn find_by_login(&mut self, login: &str) -> Result<Option<StoredAccount>, AccountError>;

    /// 写入新账号。
    ///
    /// # Errors
    ///
    /// 登录名已被占用，或存储层失败。
    async fn insert(&mut self, new: NewAccount) -> Result<(), AccountError>;
}
