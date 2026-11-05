//! 登录会话表（`sessions`）的事务内读写（`docs/design.md` §7.2、§17.1、I15、T31）。
//!
//! 边界：本模块只管 `sessions` 一张表的形态与“换摘要”的原子性，不做 HTTP、不算摘要、不判令牌是否
//! 有效（到期、撤销的权威判定在服务端用例核心 `vaultsyncd_server_core::auth::session`，那里同时
//! 决定失败语义）。本层把存储失败如实带回，不吞错。
//!
//! 与 design §7.2 字段清单的两处差异（迁移 `0006_sessions.sql` 的注释里也写了）：
//!
//! - design 写作 `device_id`：首发没有设备注册表，`device_label` 是登录时提交的人类可读标签
//!   （design §17.1 明确不额外添加设备审批），因此不虚构设备身份。
//! - design 写作一个 `token_digest`：一次登录同时签发访问凭据与刷新凭据，两者必须一起轮换，所以
//!   一行里放两个 32 字节摘要（`access_digest`/`refresh_digest`），避免“刷新凭据换掉了、访问凭据
//!   还是旧的”这种跨行不一致。
//!
//! 不变量：
//!
//! - **只存摘要**：本模块的输入是摘要，不接收也不保存令牌明文；同一摘要只允许一行（迁移的唯一索引）。
//! - **轮换在一个写事务里完成**：`rotate_refresh` 先用带 `revoked_at IS NULL` 条件的单条 UPDATE 抢占
//!   旧行，再插入后继行；抢占失败说明这个刷新凭据已经被用过（并发刷新或重放），调用方必须拒绝而
//!   不是“让它也成功”。`BEGIN IMMEDIATE`（[`crate::db::Database::begin_write`]）负责把并发写者串行化。
//! - **撤销是幂等的**：`revoke_session` 返回“本次是否真的改了行”，重复撤销不报错、不覆盖原时间。
//! - **时间是自洽的整数秒**：`created_at ≤ access_expires_at ≤ refresh_expires_at`（迁移里也有 CHECK）。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{ID_LEN, SessionId, UserId};
use vaultsyncd_domain::version::UtcTimestamp;

use crate::db::DbError;

/// 令牌摘要长度（SHA-256），与内容哈希同一算法。
pub const DIGEST_LEN: usize = 32;

/// 一个令牌摘要。
pub type TokenDigest = [u8; DIGEST_LEN];

/// 要写入的一条会话。
///
/// 摘要由用例核心算好后传入；`id` 由用例核心生成（同一个 `IdSource`，见 design §7.1）。
#[derive(Clone, Debug)]
pub struct NewSession {
    /// 会话身份（design §7.2 `sessions.id`）。
    pub id: SessionId,
    /// 会话所属账号。
    pub user_id: UserId,
    /// 登录时提交的设备标签（非空文本）。
    pub device_label: String,
    /// 访问凭据摘要。
    pub access_digest: TokenDigest,
    /// 刷新凭据摘要。
    pub refresh_digest: TokenDigest,
    /// 签发时间。
    pub created_at: UtcTimestamp,
    /// 访问凭据到期时间。
    pub access_expires_at: UtcTimestamp,
    /// 刷新凭据到期时间。
    pub refresh_expires_at: UtcTimestamp,
}

/// 从数据库读出的一条 `sessions` 记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecord {
    /// 会话身份。
    pub id: SessionId,
    /// 会话所属账号。
    pub user_id: UserId,
    /// 登录时提交的设备标签。
    pub device_label: String,
    /// 访问凭据摘要。
    pub access_digest: TokenDigest,
    /// 刷新凭据摘要。
    pub refresh_digest: TokenDigest,
    /// 签发时间。
    pub created_at: UtcTimestamp,
    /// 访问凭据到期时间。
    pub access_expires_at: UtcTimestamp,
    /// 刷新凭据到期时间。
    pub refresh_expires_at: UtcTimestamp,
    /// 撤销时间；`None` 表示尚未撤销。
    pub revoked_at: Option<UtcTimestamp>,
    /// 由本会话刷新出的后继会话（撤销时写入）。
    pub replaced_by: Option<SessionId>,
}

impl SessionRecord {
    /// 是否已撤销（注销或已用于刷新）。
    #[must_use]
    pub const fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
}

/// `sessions` 表读写的失败。
#[derive(Debug)]
pub enum SessionError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// 设备标签为空（登录输入的问题，不该写进库）。
    EmptyDeviceLabel,
    /// 时间不自洽：`created_at ≤ access_expires_at ≤ refresh_expires_at` 不成立。
    InconsistentValidity,
    /// 会话身份已被占用。
    DuplicateId {
        /// 冲突的会话身份。
        id: SessionId,
    },
    /// 摘要已被另一条会话占用（只可能来自身份/摘要生成重复或外部写入）。
    DuplicateDigest,
    /// 数据库行不满足本模块的类型前提（身份长度、摘要长度）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl SessionError {
    /// 稳定错误码（日志与验收脚本共用）。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Db(_) => "session_storage_failed",
            Self::EmptyDeviceLabel => "empty_device_label",
            Self::InconsistentValidity => "inconsistent_session_validity",
            Self::DuplicateId { .. } => "duplicate_session_id",
            Self::DuplicateDigest => "duplicate_session_digest",
            Self::MalformedRow { .. } => "malformed_session_row",
        }
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::EmptyDeviceLabel => f.write_str("设备标签不能为空"),
            Self::InconsistentValidity => {
                f.write_str("会话有效期不自洽：创建时间 ≤ 访问到期 ≤ 刷新到期 不成立")
            }
            Self::DuplicateId { id } => write!(f, "会话身份 {id} 已被占用"),
            Self::DuplicateDigest => f.write_str("令牌摘要已被另一条会话占用"),
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for SessionError {
    fn from(source: sqlx::Error) -> Self {
        Self::Db(source)
    }
}

impl From<DbError> for SessionError {
    fn from(source: DbError) -> Self {
        match source {
            DbError::Sqlite(error) => Self::Db(error),
            other => Self::Db(sqlx::Error::Configuration(Box::new(other))),
        }
    }
}

/// 刷新轮换的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RotationOutcome {
    /// 抢占成功并写入了后继会话。
    Rotated,
    /// 旧会话已经被撤销过（同一刷新凭据被并发提交或重放）：没有写入任何行。
    Consumed,
    /// 没有这个会话身份（调用方通常先按摘要查到行，因此这属于并发删除或调用方错误）。
    Missing,
}

/// `sessions` 表的列顺序，多处查询共用。
const SESSION_COLUMNS: &str = "id, user_id, device_label, access_digest, refresh_digest, created_at, \
     access_expires_at, refresh_expires_at, revoked_at, replaced_by";

/// 写入一条会话；摘要或身份重复时报错且不写入。
pub async fn insert_session(
    tx: &mut Transaction<'_, Sqlite>,
    new: &NewSession,
) -> Result<(), SessionError> {
    validate_shape(new)?;
    if session_id_exists(&mut *tx, new.id).await? {
        return Err(SessionError::DuplicateId { id: new.id });
    }
    sqlx::query(
        "INSERT INTO sessions (id, user_id, device_label, access_digest, refresh_digest, \
         created_at, access_expires_at, refresh_expires_at, revoked_at, replaced_by) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL)",
    )
    .bind(new.id.as_bytes().as_slice())
    .bind(new.user_id.as_bytes().as_slice())
    .bind(&new.device_label)
    .bind(new.access_digest.as_slice())
    .bind(new.refresh_digest.as_slice())
    .bind(new.created_at.to_db_integer())
    .bind(new.access_expires_at.to_db_integer())
    .bind(new.refresh_expires_at.to_db_integer())
    .execute(&mut **tx)
    .await
    .map_err(map_write_error)?;
    Ok(())
}

/// 按访问凭据摘要读会话（唯一索引保证至多一行）。
pub async fn find_by_access_digest(
    conn: &mut SqliteConnection,
    digest: &TokenDigest,
) -> Result<Option<SessionRecord>, SessionError> {
    find_by_digest(conn, "access_digest", digest).await
}

/// 按刷新凭据摘要读会话（唯一索引保证至多一行）。
pub async fn find_by_refresh_digest(
    conn: &mut SqliteConnection,
    digest: &TokenDigest,
) -> Result<Option<SessionRecord>, SessionError> {
    find_by_digest(conn, "refresh_digest", digest).await
}

/// 按会话身份读会话。
pub async fn find_session(
    conn: &mut SqliteConnection,
    id: SessionId,
) -> Result<Option<SessionRecord>, SessionError> {
    let row = sqlx::query(&format!(
        "SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"
    ))
    .bind(id.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(decode_row).transpose()
}

async fn find_by_digest(
    conn: &mut SqliteConnection,
    column: &'static str,
    digest: &TokenDigest,
) -> Result<Option<SessionRecord>, SessionError> {
    let row = sqlx::query(&format!(
        "SELECT {SESSION_COLUMNS} FROM sessions WHERE {column} = ?1"
    ))
    .bind(digest.as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(decode_row).transpose()
}

/// 撤销一条会话（注销）。
///
/// 返回“本次是否真的改了行”：已撤销的会话再撤销返回 `false`，且**不覆盖**原来的撤销时间——重复注销
/// 是幂等的，但证据不被篡改。
pub async fn revoke_session(
    tx: &mut Transaction<'_, Sqlite>,
    id: SessionId,
    at: UtcTimestamp,
) -> Result<bool, SessionError> {
    let affected =
        sqlx::query("UPDATE sessions SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL")
            .bind(id.as_bytes().as_slice())
            .bind(at.to_db_integer())
            .execute(&mut **tx)
            .await?
            .rows_affected();
    Ok(affected > 0)
}

/// 在同一个写事务里撤销旧会话并写入后继会话（刷新轮换）。
///
/// 抢占靠一条带 `revoked_at IS NULL` 条件的 UPDATE：并发的第二次刷新影响 0 行，得到
/// [`RotationOutcome::Consumed`]，因此不会出现两个都成功、两个都拿到新凭据的“重复刷新”。
///
/// 顺序必须是“抢占旧行 → 插入后继行 → 回填 `replaced_by`”：`replaced_by` 有指向 `sessions.id`
/// 的外键，先写它就会引用一行还不存在的后继。三步都在同一个写事务里，即使是先抢占再插入，
/// 也不会让第三个写入者看到中间态。
pub async fn rotate_refresh(
    tx: &mut Transaction<'_, Sqlite>,
    current: SessionId,
    successor: &NewSession,
    at: UtcTimestamp,
) -> Result<RotationOutcome, SessionError> {
    validate_shape(successor)?;
    if session_id_exists(&mut *tx, successor.id).await? {
        return Err(SessionError::DuplicateId { id: successor.id });
    }
    let affected =
        sqlx::query("UPDATE sessions SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL")
            .bind(current.as_bytes().as_slice())
            .bind(at.to_db_integer())
            .execute(&mut **tx)
            .await
            .map_err(map_write_error)?
            .rows_affected();
    if affected == 0 {
        return Ok(if session_id_exists(&mut *tx, current).await? {
            RotationOutcome::Consumed
        } else {
            RotationOutcome::Missing
        });
    }
    sqlx::query(
        "INSERT INTO sessions (id, user_id, device_label, access_digest, refresh_digest, \
         created_at, access_expires_at, refresh_expires_at, revoked_at, replaced_by) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL)",
    )
    .bind(successor.id.as_bytes().as_slice())
    .bind(successor.user_id.as_bytes().as_slice())
    .bind(&successor.device_label)
    .bind(successor.access_digest.as_slice())
    .bind(successor.refresh_digest.as_slice())
    .bind(successor.created_at.to_db_integer())
    .bind(successor.access_expires_at.to_db_integer())
    .bind(successor.refresh_expires_at.to_db_integer())
    .execute(&mut **tx)
    .await
    .map_err(map_write_error)?;
    sqlx::query("UPDATE sessions SET replaced_by = ?2 WHERE id = ?1")
        .bind(current.as_bytes().as_slice())
        .bind(successor.id.as_bytes().as_slice())
        .execute(&mut **tx)
        .await?;
    Ok(RotationOutcome::Rotated)
}

/// 会话身份是否已存在（写事务内的唯一性预检）。
pub async fn session_id_exists(
    conn: &mut SqliteConnection,
    id: SessionId,
) -> Result<bool, SessionError> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM sessions WHERE id = ?1")
        .bind(id.as_bytes().as_slice())
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.is_some())
}

fn validate_shape(new: &NewSession) -> Result<(), SessionError> {
    if new.device_label.is_empty() {
        return Err(SessionError::EmptyDeviceLabel);
    }
    if new.access_expires_at.as_unix_seconds() < new.created_at.as_unix_seconds()
        || new.refresh_expires_at.as_unix_seconds() < new.access_expires_at.as_unix_seconds()
    {
        return Err(SessionError::InconsistentValidity);
    }
    Ok(())
}

/// 把摘要的唯一约束冲突翻译成本模块的错误码，其余错误原样带回。
///
/// 只有摘要冲突能在这里翻译：会话身份的冲突由 [`session_id_exists`] 预检发现（能带回真实身份），
/// 因此不需要从 SQLite 的消息文本里猜关涉哪一行。
fn map_write_error(error: sqlx::Error) -> SessionError {
    if let sqlx::Error::Database(database) = &error {
        let message = database.message();
        if message.contains("sessions.access_digest") || message.contains("sessions.refresh_digest")
        {
            return SessionError::DuplicateDigest;
        }
    }
    SessionError::Db(error)
}

fn decode_row(row: &sqlx::sqlite::SqliteRow) -> Result<SessionRecord, SessionError> {
    use sqlx::Row as _;
    let id = identity(row, "id").map(SessionId::from_bytes)?;
    let user_id = identity(row, "user_id").map(UserId::from_bytes)?;
    let device_label: String =
        row.try_get("device_label")
            .map_err(|_| SessionError::MalformedRow {
                column: "device_label",
            })?;
    let access_digest = digest(row, "access_digest")?;
    let refresh_digest = digest(row, "refresh_digest")?;
    let created_at = timestamp(row, "created_at")?;
    let access_expires_at = timestamp(row, "access_expires_at")?;
    let refresh_expires_at = timestamp(row, "refresh_expires_at")?;
    let revoked_at: Option<i64> =
        row.try_get("revoked_at")
            .map_err(|_| SessionError::MalformedRow {
                column: "revoked_at",
            })?;
    let revoked_at = revoked_at
        .map(|value| {
            UtcTimestamp::from_db_integer(value).map_err(|_| SessionError::MalformedRow {
                column: "revoked_at",
            })
        })
        .transpose()?;
    let replaced_by: Option<Vec<u8>> =
        row.try_get("replaced_by")
            .map_err(|_| SessionError::MalformedRow {
                column: "replaced_by",
            })?;
    let replaced_by = replaced_by
        .map(|raw| {
            let bytes: [u8; ID_LEN] =
                raw.as_slice()
                    .try_into()
                    .map_err(|_| SessionError::MalformedRow {
                        column: "replaced_by",
                    })?;
            Ok::<SessionId, SessionError>(SessionId::from_bytes(bytes))
        })
        .transpose()?;
    Ok(SessionRecord {
        id,
        user_id,
        device_label,
        access_digest,
        refresh_digest,
        created_at,
        access_expires_at,
        refresh_expires_at,
        revoked_at,
        replaced_by,
    })
}

fn identity(
    row: &sqlx::sqlite::SqliteRow,
    column: &'static str,
) -> Result<[u8; ID_LEN], SessionError> {
    use sqlx::Row as _;
    let raw: Vec<u8> = row
        .try_get(column)
        .map_err(|_| SessionError::MalformedRow { column })?;
    raw.as_slice()
        .try_into()
        .map_err(|_| SessionError::MalformedRow { column })
}

fn digest(
    row: &sqlx::sqlite::SqliteRow,
    column: &'static str,
) -> Result<TokenDigest, SessionError> {
    use sqlx::Row as _;
    let raw: Vec<u8> = row
        .try_get(column)
        .map_err(|_| SessionError::MalformedRow { column })?;
    raw.as_slice()
        .try_into()
        .map_err(|_| SessionError::MalformedRow { column })
}

fn timestamp(
    row: &sqlx::sqlite::SqliteRow,
    column: &'static str,
) -> Result<UtcTimestamp, SessionError> {
    use sqlx::Row as _;
    let value: i64 = row
        .try_get(column)
        .map_err(|_| SessionError::MalformedRow { column })?;
    UtcTimestamp::from_db_integer(value).map_err(|_| SessionError::MalformedRow { column })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{TestDir, at, open_database, session, user};

    fn new_session(marker: u8, digest_byte: u8) -> NewSession {
        NewSession {
            id: session(marker),
            user_id: user(0x11),
            device_label: "Windows 11 桌面".to_owned(),
            access_digest: [digest_byte; DIGEST_LEN],
            refresh_digest: [digest_byte.wrapping_add(1); DIGEST_LEN],
            created_at: at(1_000),
            access_expires_at: at(1_900),
            refresh_expires_at: at(1_000_000),
        }
    }

    async fn seed_owner(database: &crate::db::Database) {
        let mut tx = database.begin_write().await.expect("写事务");
        crate::users::insert_user(
            &mut tx,
            &crate::users::NewUser {
                id: user(0x11),
                login: "owner".to_owned(),
                password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".to_owned(),
            },
        )
        .await
        .expect("写 owner");
        tx.commit().await.expect("提交");
    }

    #[tokio::test]
    async fn insert_then_find_by_both_digests_and_id() {
        let dir = TestDir::new("sessions-roundtrip");
        let database = open_database(&dir).await;
        seed_owner(&database).await;
        let inserted = new_session(0x01, 0x40);
        let mut tx = database.begin_write().await.expect("写事务");
        insert_session(&mut tx, &inserted).await.expect("写会话");
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let by_id = find_session(&mut conn, inserted.id)
            .await
            .expect("按身份读")
            .expect("会话应存在");
        assert_eq!(by_id.user_id, user(0x11));
        assert_eq!(by_id.device_label, "Windows 11 桌面");
        assert_eq!(by_id.access_expires_at, at(1_900));
        assert!(!by_id.is_revoked());
        assert_eq!(by_id.replaced_by, None);
        assert_eq!(
            find_by_access_digest(&mut conn, &inserted.access_digest)
                .await
                .expect("按访问摘要读"),
            Some(by_id.clone())
        );
        assert_eq!(
            find_by_refresh_digest(&mut conn, &inserted.refresh_digest)
                .await
                .expect("按刷新摘要读"),
            Some(by_id)
        );
        assert!(
            find_by_access_digest(&mut conn, &[0x7f; DIGEST_LEN])
                .await
                .expect("按摘要读")
                .is_none()
        );
    }

    #[tokio::test]
    async fn duplicate_digest_and_unselfconsistent_validity_are_rejected() {
        let dir = TestDir::new("sessions-reject");
        let database = open_database(&dir).await;
        seed_owner(&database).await;
        let mut tx = database.begin_write().await.expect("写事务");
        insert_session(&mut tx, &new_session(0x01, 0x50))
            .await
            .expect("写会话");

        // 同一访问摘要的第二行：唯一索引必须拒绝，而不是让“用哪个会话”取决于行顺序。
        let mut reused = new_session(0x02, 0x50);
        reused.refresh_digest = [0x99; DIGEST_LEN];
        assert!(matches!(
            insert_session(&mut tx, &reused).await,
            Err(SessionError::DuplicateDigest)
        ));

        let mut backwards = new_session(0x03, 0x60);
        backwards.access_expires_at = at(999);
        assert!(matches!(
            insert_session(&mut tx, &backwards).await,
            Err(SessionError::InconsistentValidity)
        ));

        let mut unbounded = new_session(0x04, 0x70);
        unbounded.refresh_expires_at = at(1_800);
        assert!(matches!(
            insert_session(&mut tx, &unbounded).await,
            Err(SessionError::InconsistentValidity)
        ));

        let mut anonymous = new_session(0x05, 0x80);
        anonymous.device_label.clear();
        assert!(matches!(
            insert_session(&mut tx, &anonymous).await,
            Err(SessionError::EmptyDeviceLabel)
        ));
        tx.rollback().await.expect("回滚");
    }

    #[tokio::test]
    async fn revoke_is_idempotent_and_keeps_the_first_timestamp() {
        let dir = TestDir::new("sessions-revoke");
        let database = open_database(&dir).await;
        seed_owner(&database).await;
        let inserted = new_session(0x01, 0x90);
        let mut tx = database.begin_write().await.expect("写事务");
        insert_session(&mut tx, &inserted).await.expect("写会话");
        tx.commit().await.expect("提交");

        let mut tx = database.begin_write().await.expect("写事务");
        assert!(
            revoke_session(&mut tx, inserted.id, at(1_200))
                .await
                .expect("撤销"),
            "第一次撤销必须改到行"
        );
        assert!(
            !revoke_session(&mut tx, inserted.id, at(1_500))
                .await
                .expect("重复撤销"),
            "重复撤销不得报告改行"
        );
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let record = find_session(&mut conn, inserted.id)
            .await
            .expect("按身份读")
            .expect("会话应存在");
        assert_eq!(record.revoked_at, Some(at(1_200)), "重复撤销不覆盖原时间");
        assert!(record.is_revoked());
    }

    #[tokio::test]
    async fn rotation_consumes_the_old_session_and_only_succeeds_once() {
        let dir = TestDir::new("sessions-rotate");
        let database = open_database(&dir).await;
        seed_owner(&database).await;
        let first = new_session(0x01, 0xa0);
        let mut tx = database.begin_write().await.expect("写事务");
        insert_session(&mut tx, &first).await.expect("写会话");
        tx.commit().await.expect("提交");

        let mut successor = new_session(0x02, 0xb0);
        successor.created_at = at(1_100);
        successor.access_expires_at = at(2_000);
        successor.refresh_expires_at = at(1_100_000);
        let mut tx = database.begin_write().await.expect("写事务");
        assert_eq!(
            rotate_refresh(&mut tx, first.id, &successor, at(1_100))
                .await
                .expect("轮换"),
            RotationOutcome::Rotated
        );
        // 同一旧会话的第二轮换（并发刷新的失败者、或旧凭据重放）必须被拒绝且不再写行。
        let mut loser = new_session(0x03, 0xc0);
        loser.created_at = at(1_101);
        loser.access_expires_at = at(2_001);
        loser.refresh_expires_at = at(1_101_000);
        assert_eq!(
            rotate_refresh(&mut tx, first.id, &loser, at(1_101))
                .await
                .expect("第二轮换"),
            RotationOutcome::Consumed
        );
        assert!(
            !session_id_exists(&mut tx, loser.id).await.expect("查身份"),
            "被拒绝的轮换不得留下后继行"
        );
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let consumed = find_session(&mut conn, first.id)
            .await
            .expect("按身份读")
            .expect("旧会话应存在");
        assert_eq!(consumed.revoked_at, Some(at(1_100)));
        assert_eq!(consumed.replaced_by, Some(successor.id));
        assert!(
            find_by_refresh_digest(&mut conn, &first.refresh_digest)
                .await
                .expect("按旧刷新摘要读")
                .expect("旧行仍在")
                .is_revoked(),
            "旧刷新摘要对应的行必须已撤销，不能因为“行还在”就放行"
        );
        assert_eq!(
            rotate_refresh_missing(&database, session(0x0e)).await,
            RotationOutcome::Missing
        );
    }

    async fn rotate_refresh_missing(
        database: &crate::db::Database,
        missing: vaultsyncd_domain::ids::SessionId,
    ) -> RotationOutcome {
        let mut tx = database.begin_write().await.expect("写事务");
        let mut ghost = new_session(0x0f, 0xd0);
        ghost.created_at = at(1_200);
        ghost.access_expires_at = at(2_100);
        ghost.refresh_expires_at = at(1_200_000);
        let outcome = rotate_refresh(&mut tx, missing, &ghost, at(1_200))
            .await
            .expect("轮换不存在会话");
        tx.rollback().await.expect("回滚");
        outcome
    }
}
