//! 服务端账号表（`users`）的事务内读写（`docs/design.md` §7.2、§17.1、C27）。
//!
//! 边界：本模块只管 `users` 一张表的形态与约束，不判密码强度、不做散列、不发放会话，也不写
//! `sessions`。这些规则属于服务端用例核心（`vaultsyncd_server_core::auth`）；本层只保证“写进去的
//! 行是什么、读出来的行是什么”，并把存储层失败如实带回，不吞错。
//!
//! 不变量：
//!
//! - **没有半初始化账号**：`password_hash` 是非空 TEXT（迁移 0002 已要求 NOT NULL）。首次 owner
//!   初始化是唯一写入路径（design §17.1：没有内置默认密码、没有公开注册），因此本模块不提供
//!   “建一个没密码的账号以后再补”的接口。
//! - **登录名唯一且原样保存**：迁移 0002 的 `login` 唯一索引保证全库唯一。本模块**不**做大小写折叠
//!   或 Unicode 规范化——design §7.2 的教训是“不偷偷规范化”；登录比较用数据库里的原始文本。
//! - **状态是闭集**：数据库里只接受 [`UserStatus::as_text`] 给出的取值；读到未知文本按坏行报
//!   [`UserError::MalformedRow`]，不猜默认值（未知状态可能就是“已失效”，猜错等于放行）。
//! - **身份是不透明字节**：`id` 是 16 字节 BLOB（design §7.1）。长度不对的行按坏行报错。
//!
//! 初始化“空状态”判断（[`count_users`]）与写账号必须在**同一个写事务**里，否则两个 init 进程可以
//! 都看到空表再各自写一行；跨进程互斥另由状态目录锁（[`crate::state_lock`]）提供。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{ID_LEN, UserId};

/// 账号可登录的状态文本。
///
/// 迁移 0002 把 `status` 的取值集合留给本卡（S042）定义。首发只有一位可登录的 owner，因此当前
/// 闭集只有 [`UserStatus::Active`]；出现新状态（停用、待重置密码）时**追加**取值，并同时决定登录
/// 路径如何处理它——在这里猜到“别的文本也许能登录”会直接变成越权。
pub const STATUS_ACTIVE: &str = "active";

/// 账号状态（数据库 `users.status` 的闭集）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserStatus {
    /// 可以登录。
    Active,
}

impl UserStatus {
    /// 数据库里的规范文本。
    #[must_use]
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Active => STATUS_ACTIVE,
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理。
    #[must_use]
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            STATUS_ACTIVE => Some(Self::Active),
            _ => None,
        }
    }
}

/// 写入一个新账号的输入。
///
/// `password_hash` 是调用方（用例核心）已经算好的 PHC 文本；本层不解析它，也不检查强度——但要求
/// 非空，因为“空散列”等于让任何密码都通过。
#[derive(Clone, Debug)]
pub struct NewUser {
    /// 账号身份（由用例核心经 `IdSource` 生成）。
    pub id: UserId,
    /// 登录名（原样保存，唯一）。
    pub login: String,
    /// 密码散列（Argon2id 的 PHC 文本）。
    pub password_hash: String,
}

/// 从数据库读出的一条 `users` 记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserRecord {
    /// 账号身份。
    pub id: UserId,
    /// 登录名。
    pub login: String,
    /// 密码散列（PHC 文本）。
    pub password_hash: String,
    /// 账号状态。
    pub status: UserStatus,
}

/// `users` 表读写的失败。
#[derive(Debug)]
pub enum UserError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// 登录名为空。
    EmptyLogin,
    /// 密码散列为空。
    EmptyPasswordHash,
    /// 同一登录名已有账号。
    DuplicateLogin {
        /// 冲突的登录名。
        login: String,
    },
    /// 账号身份已被占用。
    DuplicateId {
        /// 冲突的账号身份。
        id: UserId,
    },
    /// 数据库行不满足本模块的类型前提（身份长度、状态取值）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl UserError {
    /// 稳定错误码（日志与验收脚本共用）。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Db(_) => "user_storage_failed",
            Self::EmptyLogin => "empty_login",
            Self::EmptyPasswordHash => "empty_password_hash",
            Self::DuplicateLogin { .. } => "duplicate_login",
            Self::DuplicateId { .. } => "duplicate_user_id",
            Self::MalformedRow { .. } => "malformed_user_row",
        }
    }
}

impl std::fmt::Display for UserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::EmptyLogin => f.write_str("登录名不能为空"),
            Self::EmptyPasswordHash => f.write_str("密码散列不能为空"),
            Self::DuplicateLogin { login } => write!(f, "登录名 {login:?} 已被占用"),
            Self::DuplicateId { id } => write!(f, "账号身份 {id} 已被占用"),
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
        }
    }
}

impl std::error::Error for UserError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for UserError {
    fn from(source: sqlx::Error) -> Self {
        Self::Db(source)
    }
}

/// `users` 表的列顺序，多处查询共用。
const USER_COLUMNS: &str = "id, login, password_hash, status";

/// 写入一个新账号；登录名重复或身份被占用时报错且不写入。
///
/// 输入检查在写入前完成，因此在调用方的写事务（`BEGIN IMMEDIATE`，见 [`crate::db`]）里对唯一性
/// 的判断是权威的：同一事务里的第二次插入会看到第一次写入的行。
pub async fn insert_user(tx: &mut Transaction<'_, Sqlite>, new: &NewUser) -> Result<(), UserError> {
    if new.login.is_empty() {
        return Err(UserError::EmptyLogin);
    }
    if new.password_hash.is_empty() {
        return Err(UserError::EmptyPasswordHash);
    }
    if login_exists(&mut *tx, &new.login).await? {
        return Err(UserError::DuplicateLogin {
            login: new.login.clone(),
        });
    }
    if user_exists(&mut *tx, new.id).await? {
        return Err(UserError::DuplicateId { id: new.id });
    }
    sqlx::query("INSERT INTO users (id, login, password_hash, status) VALUES (?1, ?2, ?3, ?4)")
        .bind(new.id.as_bytes().as_slice())
        .bind(&new.login)
        .bind(&new.password_hash)
        .bind(STATUS_ACTIVE)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// 按登录名读账号（原样比较，不做大小写折叠）。
///
/// 返回 `None` 表示没有这个登录名——调用方必须把“没有账号”和“密码错”对外表现为同一个失败
/// （design §15.2：错误不暴露账号是否存在）。
pub async fn find_by_login(
    conn: &mut SqliteConnection,
    login: &str,
) -> Result<Option<UserRecord>, UserError> {
    let row = sqlx::query(&format!(
        "SELECT {USER_COLUMNS} FROM users WHERE login = ?1"
    ))
    .bind(login)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        Some(row) => decode_row(&row).map(Some),
        None => Ok(None),
    }
}

/// 按身份读账号。
pub async fn find_by_id(
    conn: &mut SqliteConnection,
    id: UserId,
) -> Result<Option<UserRecord>, UserError> {
    let row = sqlx::query(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?1"))
        .bind(id.as_bytes().as_slice())
        .fetch_optional(&mut *conn)
        .await?;
    match row {
        Some(row) => decode_row(&row).map(Some),
        None => Ok(None),
    }
}

/// 账号总数。
///
/// 首次 owner 初始化用它判断“空状态”；与 [`insert_user`] 同事务调用才可靠（见模块文档）。
pub async fn count_users(conn: &mut SqliteConnection) -> Result<u64, UserError> {
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM users")
        .fetch_one(&mut *conn)
        .await?;
    u64::try_from(count).map_err(|source| {
        UserError::Db(sqlx::Error::Decode(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("账号计数 {count} 超出 u64：{source}"),
        ))))
    })
}

/// 登录名是否已存在（写事务内的唯一性预检）。
pub async fn login_exists(conn: &mut SqliteConnection, login: &str) -> Result<bool, UserError> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM users WHERE login = ?1")
        .bind(login)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.is_some())
}

/// 账号身份是否已存在。
pub async fn user_exists(conn: &mut SqliteConnection, id: UserId) -> Result<bool, UserError> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM users WHERE id = ?1")
        .bind(id.as_bytes().as_slice())
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.is_some())
}

fn decode_row(row: &sqlx::sqlite::SqliteRow) -> Result<UserRecord, UserError> {
    use sqlx::Row as _;
    let raw_id: Vec<u8> = row
        .try_get("id")
        .map_err(|_| UserError::MalformedRow { column: "id" })?;
    let bytes: [u8; ID_LEN] = raw_id
        .as_slice()
        .try_into()
        .map_err(|_| UserError::MalformedRow { column: "id" })?;
    let login: String = row
        .try_get("login")
        .map_err(|_| UserError::MalformedRow { column: "login" })?;
    let password_hash: String =
        row.try_get("password_hash")
            .map_err(|_| UserError::MalformedRow {
                column: "password_hash",
            })?;
    let status_text: String = row
        .try_get("status")
        .map_err(|_| UserError::MalformedRow { column: "status" })?;
    let status =
        UserStatus::from_text(&status_text).ok_or(UserError::MalformedRow { column: "status" })?;
    Ok(UserRecord {
        id: UserId::from_bytes(bytes),
        login,
        password_hash,
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{TestDir, open_database, user};

    fn new_user(marker: u8, login: &str) -> NewUser {
        NewUser {
            id: user(marker),
            login: login.to_owned(),
            password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".to_owned(),
        }
    }

    #[tokio::test]
    async fn insert_then_find_by_login_and_id() {
        let dir = TestDir::new("users-roundtrip");
        let database = open_database(&dir).await;
        let mut tx = database.begin_write().await.expect("写事务");
        insert_user(&mut tx, &new_user(0x11, "owner"))
            .await
            .expect("写 owner");
        assert_eq!(count_users(&mut tx).await.expect("计数"), 1);
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let record = find_by_login(&mut conn, "owner")
            .await
            .expect("按登录名读")
            .expect("账号应存在");
        assert_eq!(record.id, user(0x11));
        assert_eq!(record.login, "owner");
        assert_eq!(record.status, UserStatus::Active);
        assert_eq!(
            record.password_hash,
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA"
        );
        assert_eq!(
            find_by_id(&mut conn, user(0x11)).await.expect("按身份读"),
            Some(record)
        );
        assert!(
            find_by_login(&mut conn, "missing")
                .await
                .expect("按登录名读")
                .is_none()
        );
    }

    #[tokio::test]
    async fn duplicate_login_and_id_are_rejected_without_writing() {
        let dir = TestDir::new("users-duplicate");
        let database = open_database(&dir).await;
        let mut tx = database.begin_write().await.expect("写事务");
        insert_user(&mut tx, &new_user(0x11, "owner"))
            .await
            .expect("写 owner");

        let duplicate_login = insert_user(&mut tx, &new_user(0x22, "owner")).await;
        assert!(
            matches!(duplicate_login, Err(UserError::DuplicateLogin { ref login }) if login == "owner"),
            "同一登录名必须被拒绝：{duplicate_login:?}"
        );
        let duplicate_id = insert_user(&mut tx, &new_user(0x11, "other")).await;
        assert!(
            matches!(duplicate_id, Err(UserError::DuplicateId { id }) if id == user(0x11)),
            "同一身份必须被拒绝：{duplicate_id:?}"
        );
        assert_eq!(count_users(&mut tx).await.expect("计数"), 1);
        tx.rollback().await.expect("回滚");
    }

    #[tokio::test]
    async fn empty_login_and_empty_hash_are_rejected_before_writing() {
        let dir = TestDir::new("users-empty");
        let database = open_database(&dir).await;
        let mut tx = database.begin_write().await.expect("写事务");
        let mut empty_login = new_user(0x11, "");
        assert!(matches!(
            insert_user(&mut tx, &empty_login).await,
            Err(UserError::EmptyLogin)
        ));
        empty_login.login = "owner".to_owned();
        empty_login.password_hash.clear();
        assert!(matches!(
            insert_user(&mut tx, &empty_login).await,
            Err(UserError::EmptyPasswordHash)
        ));
        assert_eq!(count_users(&mut tx).await.expect("计数"), 0);
        tx.rollback().await.expect("回滚");
    }

    #[tokio::test]
    async fn login_comparison_is_exact_not_case_folded() {
        let dir = TestDir::new("users-login-exact");
        let database = open_database(&dir).await;
        let mut tx = database.begin_write().await.expect("写事务");
        insert_user(&mut tx, &new_user(0x11, "owner"))
            .await
            .expect("写 owner");
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert!(
            find_by_login(&mut conn, "Owner")
                .await
                .expect("按登录名读")
                .is_none(),
            "登录名不做大小写折叠"
        );
    }

    #[tokio::test]
    async fn unknown_status_text_is_a_malformed_row() {
        let dir = TestDir::new("users-status");
        let database = open_database(&dir).await;
        let mut tx = database.begin_write().await.expect("写事务");
        insert_user(&mut tx, &new_user(0x11, "owner"))
            .await
            .expect("写 owner");
        sqlx::query("UPDATE users SET status = 'disabled' WHERE login = 'owner'")
            .execute(&mut *tx)
            .await
            .expect("改状态");
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        match find_by_login(&mut conn, "owner").await {
            Err(UserError::MalformedRow { column }) => assert_eq!(column, "status"),
            other => panic!("未知状态必须按坏行报错，而不是猜默认值：{other:?}"),
        }
    }
}
