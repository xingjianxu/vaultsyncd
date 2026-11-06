//! 需要关注的问题：去重、已读与恢复状态（`docs/design.md` §7.3、§18.2）。
//!
//! 边界：本模块只保存“这个问题存在过、出现过几次、用户是否已读、是否已恢复”。它不决定何时
//! 通知、不读时钟、不合并文本：什么算同一个问题由调用方按“原因 + Root”组成去重键，是否进入
//! 系统通知由界面层按 design §18.2 判断。
//!
//! 不变量：
//!
//! - **同一去重键只有一行**：同一原因重复报告只累加 `occurrences`，不刷屏（design §18.2）；
//!   同键更换错误码是调用方组键不一致，报 [`NotificationError::CodeMismatch`]，不静默改原因。
//! - **已读不因重复报告被重置**：问题仍在发生时再次上报只加次数，用户已读的事实保留。
//! - **恢复后复发要重新提醒**：`resolved` 的问题再次出现时回到 `active` 并置为未读——不反复
//!   弹窗不等于把复发的问题藏起来（design §18.2）。
//! - **问题可以不属于某个绑定**：`binding` 为 `None` 表示缓存盘满这类全局问题；绑定被解绑时
//!   该绑定的通知随外键级联删除。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{BindingId, EntryId, ID_LEN};
pub mod delivery;

/// 问题的生命周期（`notifications.state`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationState {
    /// 仍需要关注。
    Active,
    /// 已恢复。
    Resolved,
}

impl NotificationState {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Resolved => "resolved",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "active" => Some(Self::Active),
            "resolved" => Some(Self::Resolved),
            _ => None,
        }
    }
}

/// 上报一个问题的输入。
///
/// 不变量（违反时 [`raise`] 报错且不写入）：`dedupe_key` 非空、无 NUL；同一键已存在的错误码
/// 必须相同。
#[derive(Clone, Debug)]
pub struct NewNotification {
    /// 去重键（调用方按“原因 + Root”组成）。
    pub dedupe_key: String,
    /// 相关绑定；`None` 表示与具体绑定无关的全局问题。
    pub binding: Option<BindingId>,
    /// 相关条目。
    pub entry: Option<EntryId>,
    /// 问题原因（design §15.4 错误码）。
    pub code: ErrorCode,
}

/// 一条需要关注的问题（`notifications` 一行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// 去重键。
    pub dedupe_key: String,
    /// 相关绑定。
    pub binding: Option<BindingId>,
    /// 相关条目。
    pub entry: Option<EntryId>,
    /// 问题原因。
    pub code: ErrorCode,
    /// 是否已恢复。
    pub state: NotificationState,
    /// 用户是否已读。
    pub is_read: bool,
    /// 累计上报次数。
    pub occurrences: u32,
}

/// 通知读写的失败。
#[derive(Debug)]
pub enum NotificationError {
    /// SQLite 层面失败（含外键与 `CHECK` 拒绝）。
    Db(sqlx::Error),
    /// 去重键不合格（空或含 NUL）。
    InvalidDedupeKey {
        /// 违反的具体规则。
        reason: &'static str,
    },
    /// 同一去重键已有不同的错误码：调用方的组键规则不一致。
    CodeMismatch {
        /// 冲突的去重键。
        dedupe_key: String,
        /// 库里已记录的代码。
        existing: ErrorCode,
    },
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for NotificationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::InvalidDedupeKey { reason } => write!(f, "去重键不合法：{reason}"),
            Self::CodeMismatch {
                dedupe_key,
                existing,
            } => write!(
                f,
                "去重键 {dedupe_key} 已记录为 {}，不能用另一个原因复用",
                existing.as_str()
            ),
            Self::MalformedRow { column } => {
                write!(f, "数据库列 {column} 的值不符合类型前提")
            }
        }
    }
}

impl std::error::Error for NotificationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::InvalidDedupeKey { .. }
            | Self::CodeMismatch { .. }
            | Self::MalformedRow { .. } => None,
        }
    }
}

/// `notifications` 的列顺序。
const NOTIFICATION_COLUMNS: &str =
    "dedupe_key, binding_id, entry_id, code, state, is_read, occurrences";

/// 上报一个问题（按去重键合并）并返回读回记录。
///
/// - 首次出现：写入 `active`、未读、`occurrences = 1`。
/// - 仍 `active` 时重复出现：只累加次数，保留已读状态（不刷屏）。
/// - 已 `resolved` 后复发：回到 `active`、置为未读并累加次数（复发需要重新提醒）。
pub async fn raise(
    tx: &mut Transaction<'_, Sqlite>,
    new: &NewNotification,
) -> Result<Notification, NotificationError> {
    if new.dedupe_key.is_empty() {
        return Err(NotificationError::InvalidDedupeKey {
            reason: "不能为空"
        });
    }
    if new.dedupe_key.contains('\0') {
        return Err(NotificationError::InvalidDedupeKey {
            reason: "不能包含 NUL",
        });
    }

    if let Some(existing) = notification(tx, &new.dedupe_key).await? {
        if existing.code != new.code {
            return Err(NotificationError::CodeMismatch {
                dedupe_key: new.dedupe_key.clone(),
                existing: existing.code,
            });
        }
        // 仍在发生时只加次数，已读保留；恢复后复发要重新成为未读的 active。
        let update = match existing.state {
            NotificationState::Active => sqlx::query(
                "UPDATE notifications SET occurrences = occurrences + 1 WHERE dedupe_key = ?1",
            )
            .bind(&new.dedupe_key),
            NotificationState::Resolved => sqlx::query(
                "UPDATE notifications SET state = ?1, is_read = 0, \
                 occurrences = occurrences + 1, generation = generation + 1, delivery_claimed = 0 WHERE dedupe_key = ?2",
            )
            .bind(NotificationState::Active.as_text())
            .bind(&new.dedupe_key),
        };
        update
            .execute(&mut **tx)
            .await
            .map_err(NotificationError::Db)?;
    } else {
        sqlx::query(
            "INSERT INTO notifications (dedupe_key, binding_id, entry_id, code) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(&new.dedupe_key)
        .bind(binding_bytes(new.binding))
        .bind(entry_bytes(new.entry))
        .bind(new.code.as_str())
        .execute(&mut **tx)
        .await
        .map_err(NotificationError::Db)?;
    }

    notification(tx, &new.dedupe_key)
        .await?
        .ok_or(NotificationError::MalformedRow {
            column: "dedupe_key",
        })
}

/// 读一条通知；不存在返回 `None`。
pub async fn notification(
    conn: &mut SqliteConnection,
    dedupe_key: &str,
) -> Result<Option<Notification>, NotificationError> {
    let row = sqlx::query_as::<_, NotificationRow>(&format!(
        "SELECT {NOTIFICATION_COLUMNS} FROM notifications WHERE dedupe_key = ?1"
    ))
    .bind(dedupe_key)
    .fetch_optional(conn)
    .await
    .map_err(NotificationError::Db)?;
    row.map(NotificationRow::into_record).transpose()
}

/// 列出全部通知（写入序，界面按它展示）。
pub async fn notifications(
    conn: &mut SqliteConnection,
) -> Result<Vec<Notification>, NotificationError> {
    let rows = sqlx::query_as::<_, NotificationRow>(&format!(
        "SELECT {NOTIFICATION_COLUMNS} FROM notifications ORDER BY rowid"
    ))
    .fetch_all(conn)
    .await
    .map_err(NotificationError::Db)?;
    rows.into_iter()
        .map(NotificationRow::into_record)
        .collect::<Result<Vec<_>, _>>()
}

/// 列出仍未恢复的通知（问题页的内容）。
pub async fn active_notifications(
    conn: &mut SqliteConnection,
) -> Result<Vec<Notification>, NotificationError> {
    let rows = sqlx::query_as::<_, NotificationRow>(&format!(
        "SELECT {NOTIFICATION_COLUMNS} FROM notifications WHERE state = ?1 ORDER BY rowid"
    ))
    .bind(NotificationState::Active.as_text())
    .fetch_all(conn)
    .await
    .map_err(NotificationError::Db)?;
    rows.into_iter()
        .map(NotificationRow::into_record)
        .collect::<Result<Vec<_>, _>>()
}

/// 标记已读（`is_read = 1`）；返回是否命中。
pub async fn mark_read(
    tx: &mut Transaction<'_, Sqlite>,
    dedupe_key: &str,
) -> Result<bool, NotificationError> {
    let updated = sqlx::query("UPDATE notifications SET is_read = 1 WHERE dedupe_key = ?1")
        .bind(dedupe_key)
        .execute(&mut **tx)
        .await
        .map_err(NotificationError::Db)?;
    Ok(updated.rows_affected() > 0)
}

/// 标记问题已恢复（`state = resolved`）；返回是否命中。
pub async fn resolve(
    tx: &mut Transaction<'_, Sqlite>,
    dedupe_key: &str,
) -> Result<bool, NotificationError> {
    let updated = sqlx::query("UPDATE notifications SET state = ?1 WHERE dedupe_key = ?2")
        .bind(NotificationState::Resolved.as_text())
        .bind(dedupe_key)
        .execute(&mut **tx)
        .await
        .map_err(NotificationError::Db)?;
    Ok(updated.rows_affected() > 0)
}

/// 未读且未恢复的通知条数（状态页角标）。
pub async fn unread_count(conn: &mut SqliteConnection) -> Result<u64, NotificationError> {
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notifications WHERE state = ?1 AND is_read = 0")
            .bind(NotificationState::Active.as_text())
            .fetch_one(conn)
            .await
            .map_err(NotificationError::Db)?;
    Ok(count as u64)
}

fn binding_bytes(binding: Option<BindingId>) -> Option<Vec<u8>> {
    binding.map(|binding| binding.as_bytes().to_vec())
}

fn entry_bytes(entry: Option<EntryId>) -> Option<Vec<u8>> {
    entry.map(|entry| entry.as_bytes().to_vec())
}

/// 数据库行到 [`Notification`] 的中间形态。
#[derive(sqlx::FromRow)]
struct NotificationRow {
    dedupe_key: String,
    binding_id: Option<Vec<u8>>,
    entry_id: Option<Vec<u8>>,
    code: String,
    state: String,
    is_read: i64,
    occurrences: i64,
}

impl NotificationRow {
    fn into_record(self) -> Result<Notification, NotificationError> {
        let occurrences =
            u32::try_from(self.occurrences).map_err(|_| NotificationError::MalformedRow {
                column: "occurrences",
            })?;
        Ok(Notification {
            dedupe_key: self.dedupe_key,
            binding: self
                .binding_id
                .map(|bytes| identity(bytes, "binding_id", BindingId::from_bytes))
                .transpose()?,
            entry: self
                .entry_id
                .map(|bytes| identity(bytes, "entry_id", EntryId::from_bytes))
                .transpose()?,
            code: ErrorCode::from_code(&self.code)
                .ok_or(NotificationError::MalformedRow { column: "code" })?,
            state: NotificationState::from_text(&self.state)
                .ok_or(NotificationError::MalformedRow { column: "state" })?,
            is_read: self.is_read != 0,
            occurrences,
        })
    }
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; ID_LEN]) -> T,
) -> Result<T, NotificationError> {
    let array: [u8; ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| NotificationError::MalformedRow { column })?;
    Ok(wrap(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{TestDir, entry_id, open_database, seed_binding};

    fn new_notification(binding: BindingId) -> NewNotification {
        NewNotification {
            dedupe_key: "INSUFFICIENT_STORAGE:root-1".to_owned(),
            binding: Some(binding),
            entry: Some(entry_id(0x11)),
            code: ErrorCode::InsufficientStorage,
        }
    }

    #[tokio::test]
    async fn repeated_reports_dedupe_by_key_and_keep_the_read_flag() {
        let dir = TestDir::new("notifications-dedupe");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x21).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let first = raise(&mut tx, &new_notification(binding))
            .await
            .expect("首次上报");
        assert_eq!(first.state, NotificationState::Active);
        assert!(!first.is_read);
        assert_eq!(first.occurrences, 1);
        assert!(
            mark_read(&mut tx, &first.dedupe_key)
                .await
                .expect("标记已读"),
            "应命中该通知"
        );

        // 仍在发生：只累加次数，已读状态保留，不刷屏。
        let again = raise(&mut tx, &new_notification(binding))
            .await
            .expect("重复上报");
        assert_eq!(again.occurrences, 2);
        assert!(again.is_read, "重复上报不得把已读重置为未读");
        assert_eq!(again.state, NotificationState::Active);
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            notifications(&mut conn).await.expect("列通知"),
            vec![again],
            "同一去重键只有一行"
        );
        assert_eq!(unread_count(&mut conn).await.expect("未读计数"), 0);
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn recurrence_after_recovery_becomes_unread_again() {
        let dir = TestDir::new("notifications-recur");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x22).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let raised = raise(&mut tx, &new_notification(binding))
            .await
            .expect("首次上报");
        mark_read(&mut tx, &raised.dedupe_key)
            .await
            .expect("标记已读");
        assert!(
            resolve(&mut tx, &raised.dedupe_key)
                .await
                .expect("标记恢复"),
            "应命中该通知"
        );
        let mut conn = database.pool().acquire().await.expect("取连接");
        assert!(
            active_notifications(&mut conn)
                .await
                .expect("列活动通知")
                .is_empty(),
            "恢复后不在问题页"
        );
        drop(conn);

        // 恢复后复发：重新成为未读的 active，次数继续累加。
        let recurring = raise(&mut tx, &new_notification(binding))
            .await
            .expect("复发上报");
        assert_eq!(recurring.state, NotificationState::Active);
        assert!(!recurring.is_read, "复发需要重新提醒");
        assert_eq!(recurring.occurrences, 2);
        assert!(
            !resolve(&mut tx, "MISSING:root").await.expect("标记恢复"),
            "不存在的键不命中"
        );
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            active_notifications(&mut conn).await.expect("列活动通知"),
            vec![recurring.clone()]
        );
        assert_eq!(unread_count(&mut conn).await.expect("未读计数"), 1);
        drop(conn);

        // 重启保留问题与已读状态。
        database.close().await;
        let reopened = open_database(&dir).await;
        let mut conn = reopened.pool().acquire().await.expect("取连接");
        assert_eq!(
            notification(&mut conn, &recurring.dedupe_key)
                .await
                .expect("读通知"),
            Some(recurring)
        );
        drop(conn);
        reopened.close().await;
    }

    #[tokio::test]
    async fn global_problems_inconsistent_keys_and_malformed_rows_are_handled() {
        let dir = TestDir::new("notifications-reject");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x23).await;

        let mut tx = database.begin_write().await.expect("开始写事务");

        // 不属于某个绑定的全局问题（例如缓存盘满）可以没有 binding。
        let global = NewNotification {
            dedupe_key: "INSUFFICIENT_STORAGE:cache".to_owned(),
            binding: None,
            entry: None,
            code: ErrorCode::InsufficientStorage,
        };
        let raised = raise(&mut tx, &global).await.expect("上报全局问题");
        assert_eq!(raised.binding, None);
        assert_eq!(raised.entry, None);

        let mut empty_key = new_notification(binding);
        empty_key.dedupe_key = String::new();
        assert!(matches!(
            raise(&mut tx, &empty_key)
                .await
                .expect_err("空去重键必须被拒绝"),
            NotificationError::InvalidDedupeKey { .. }
        ));

        let mut nul_key = new_notification(binding);
        nul_key.dedupe_key = "a\0b".to_owned();
        assert!(matches!(
            raise(&mut tx, &nul_key)
                .await
                .expect_err("含 NUL 的去重键必须被拒绝"),
            NotificationError::InvalidDedupeKey { .. }
        ));

        // 同一去重键换了原因：调用方的组键规则不一致，不静默改原因。
        raise(&mut tx, &new_notification(binding))
            .await
            .expect("先记下该键");
        let mut mismatched = new_notification(binding);
        mismatched.code = ErrorCode::AccessDenied;
        assert!(matches!(
            raise(&mut tx, &mismatched)
                .await
                .expect_err("同键不同错误码必须被拒绝"),
            NotificationError::CodeMismatch { .. }
        ));
        tx.commit().await.expect("提交");

        // 未知错误码的行（绕过 Rust 层写入）：读取时按坏行拒绝。
        let mut tx = database.begin_write().await.expect("开始写事务");
        sqlx::query(
            "INSERT INTO notifications (dedupe_key, code) VALUES ('NOT_A_CODE:root', 'NOT_A_CODE')",
        )
        .execute(&mut *tx)
        .await
        .expect("写坏行");
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let error = notifications(&mut conn).await.expect_err("坏行必须被拒绝");
        assert!(
            matches!(error, NotificationError::MalformedRow { column: "code" }),
            "实际错误 {error:?}"
        );
        drop(conn);
        database.close().await;
    }
}
