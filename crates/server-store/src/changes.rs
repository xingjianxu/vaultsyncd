//! RootSeq 事件批次（`changes` 表）：一个事务批次一行的追加与分页读取
//! （`docs/design.md` §7.2、§9.3、I09）。
//!
//! 边界：本模块只写 `changes` 与 `roots.seq`，不写条目/版本，不生成事件内容（事件由提交协调
//! 逻辑给出），不做 HTTP 或 SSE。写函数接收调用方已开的 `BEGIN IMMEDIATE` 事务
//! （[`Database::begin_write`](crate::db::Database::begin_write)），因此“读当前 seq → 推进 seq →
//! 写批次行”在同一次持锁事务里完成，两个写者不会拿到同一个 seq。
//!
//! 不变量：
//!
//! - **RootSeq 严格单调**：批次序号是 `roots.seq` 的下一个值，只有本模块推进它。推进用
//!   `UPDATE … WHERE seq = <读到的值>` 的比较并写（compare-and-set），受影响行数不是 1 就报
//!   [`ChangeError::SeqConflict`]，而不是盲目自增；`(root_id, seq)` 的主键是最后一道防线。
//! - **一个批次不可拆分**：`changes` 一行就是一条事务批次（design §7.2、§9.3），多条事件在
//!   `events_json` 数组里。分页只切行，不切事件，所以目录级事务不会被分页拆成一半。
//! - **回滚不暴露事件**：批次行与 `roots.seq` 的推进都在调用方事务内；事务回滚时两者一起消失，
//!   外部读取（另一个连接）看不到未提交批次，也不会看到一个“跳号”的 seq。
//! - **批次非空**：`events_json` 必须是非空 JSON 数组，空批次不允许占用一个 seq。
//! - **owner scope**：写入先核对 Root 属于调用方；读取时 Root 不在 scope 内返回
//!   [`ChangeError::OutOfScope`] 而不是空列表——空列表会让客户端误以为“已追平”，把权限失败当成
//!   没有新事件（design §6 I08 的同类错误在游标上的版本）。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{RootId, TxId, UserId};
use vaultsyncd_domain::version::{RootSeq, ServerEpoch, UtcTimestamp};

/// 一次拉取的批次数上限（design §15.2：所有清单分页有上限）。
pub const MAX_CHANGE_PAGE: u32 = 500;

/// 一条 RootSeq 事务批次（`changes` 的一行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeBatch {
    /// 所属 Root。
    pub root: RootId,
    /// 批次序号：该 Root 的第 `seq` 个事务批次，从 1 起。
    pub seq: RootSeq,
    /// 产生这批事件的服务端事务身份；同一次提交的多个 Root 批次带同一个值。
    pub tx: TxId,
    /// 事件数组（JSON 文本，调用方在同一个事务里生成）。
    pub events_json: String,
    /// 批次提交时间（UTC Unix 秒）。
    pub created_at: UtcTimestamp,
}

/// 事件批次的写入与读取失败。
#[derive(Debug)]
pub enum ChangeError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// Root 不存在或不属于调用方的 owner scope（两种情况不区分）。
    OutOfScope,
    /// 当前 seq 已到数据库有符号 64 位上限，不能再分配；不回绕。
    SeqExhausted,
    /// 推进 seq 时发现当前值已经变了（本应在 `BEGIN IMMEDIATE` 里不可能发生，仍明确报错）。
    SeqConflict {
        /// 所属 Root。
        root: RootId,
        /// 本事务读到的旧序号。
        expected: RootSeq,
    },
    /// `events_json` 不是非空 JSON 数组。
    InvalidEvents,
    /// 分页大小不在 `1..=MAX_CHANGE_PAGE`。
    InvalidLimit,
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for ChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::OutOfScope => f.write_str("Root 不存在或不在调用方的 owner scope 内"),
            Self::SeqExhausted => f.write_str("RootSeq 已到数据库有符号 64 位上限"),
            Self::SeqConflict { root, expected } => {
                write!(f, "Root {root} 的 seq 已不是 {expected}，批次写入被放弃")
            }
            Self::InvalidEvents => f.write_str("事件批次必须是非空 JSON 数组"),
            Self::InvalidLimit => write!(f, "分页大小必须在 1..={MAX_CHANGE_PAGE} 之内"),
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
        }
    }
}

impl std::error::Error for ChangeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            _ => None,
        }
    }
}

/// `changes` 表的列顺序，读取点共用。
const CHANGE_COLUMNS: &str = "c.root_id, c.seq, c.tx_id, c.events_json, c.created_at";

/// 追加一条事务批次：推进 `roots.seq` 并写入 `changes` 一行。
///
/// 返回写入后的批次（`seq` 是本事务分配到的值）。调用方通常在同一个事务里接着写条目、版本与
/// 操作结果，使“事件与最终逻辑状态”一起提交（design §11.2 步骤 7）。
pub async fn append_change(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    root: RootId,
    tx_id: TxId,
    events_json: &str,
    at: UtcTimestamp,
) -> Result<ChangeBatch, ChangeError> {
    let current: Option<i64> =
        sqlx::query_scalar("SELECT seq FROM roots WHERE id = ?1 AND owner_id = ?2")
            .bind(root.as_bytes().as_slice())
            .bind(owner.as_bytes().as_slice())
            .fetch_optional(&mut **tx)
            .await
            .map_err(ChangeError::Db)?;
    let Some(current) = current else {
        return Err(ChangeError::OutOfScope);
    };
    let current = RootSeq::from_db_integer(current)
        .map_err(|_| ChangeError::MalformedRow { column: "seq" })?;
    let next = current.next().map_err(|_| ChangeError::SeqExhausted)?;

    let advanced =
        sqlx::query("UPDATE roots SET seq = ?1 WHERE id = ?2 AND owner_id = ?3 AND seq = ?4")
            .bind(next.to_db_integer())
            .bind(root.as_bytes().as_slice())
            .bind(owner.as_bytes().as_slice())
            .bind(current.to_db_integer())
            .execute(&mut **tx)
            .await
            .map_err(ChangeError::Db)?;
    if advanced.rows_affected() != 1 {
        return Err(ChangeError::SeqConflict {
            root,
            expected: current,
        });
    }

    let inserted = sqlx::query(
        "INSERT INTO changes (root_id, seq, tx_id, events_json, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(root.as_bytes().as_slice())
    .bind(next.to_db_integer())
    .bind(tx_id.as_bytes().as_slice())
    .bind(events_json)
    .bind(at.to_db_integer())
    .execute(&mut **tx)
    .await;
    if let Err(error) = inserted {
        return Err(classify_insert_error(error));
    }

    Ok(ChangeBatch {
        root,
        seq: next,
        tx: tx_id,
        events_json: events_json.to_owned(),
        created_at: at,
    })
}

/// 拉取 `after` 之后（不含 `after`）的批次，按 seq 升序，最多 `limit` 条。
///
/// `after` 用 [`RootSeq::ZERO`] 表示从头开始。每条返回的批次都是完整的一行，因此一条目录级事务
/// 不会被切开；`limit` 数的是批次数，不是事件数。
pub async fn changes_after(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
    after: RootSeq,
    limit: u32,
) -> Result<Vec<ChangeBatch>, ChangeError> {
    if limit == 0 || limit > MAX_CHANGE_PAGE {
        return Err(ChangeError::InvalidLimit);
    }
    require_root_owner(conn, owner, root).await?;
    let rows = sqlx::query_as::<_, ChangeRow>(&format!(
        "SELECT {CHANGE_COLUMNS} FROM changes c WHERE c.root_id = ?1 AND c.seq > ?2 \
         ORDER BY c.seq LIMIT ?3"
    ))
    .bind(root.as_bytes().as_slice())
    .bind(after.to_db_integer())
    .bind(i64::from(limit))
    .fetch_all(conn)
    .await
    .map_err(ChangeError::Db)?;
    rows.into_iter().map(ChangeRow::into_batch).collect()
}

/// 拉取 `after` 之后的批次，**不把同一个事务切到两页之间**。
///
/// 与 [`changes_after`] 相同，但 `limit` 只是页的最小批次数：如果第 `limit` 行与紧随其后的行属于
/// 同一个 `tx_id` 的连续 seq，页会延伸到把这个事务的批次取完。分页因此只决定“在哪个事务边界停”，
/// 不会让客户端看到半个事务（design §9.3：目录级事务不能被分页拆成一半）。
pub async fn changes_after_whole_transactions(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
    after: RootSeq,
    limit: u32,
) -> Result<Vec<ChangeBatch>, ChangeError> {
    let mut batches = changes_after(conn, owner, root, after, limit).await?;
    if batches.len() < usize::try_from(limit).unwrap_or(usize::MAX) {
        return Ok(batches);
    }
    loop {
        let Some(last) = batches.last() else {
            return Ok(batches);
        };
        let next_seq = last.seq.next().map_err(|_| ChangeError::SeqExhausted)?;
        let row = sqlx::query_as::<_, ChangeRow>(&format!(
            "SELECT {CHANGE_COLUMNS} FROM changes c \
             WHERE c.root_id = ?1 AND c.seq = ?2 AND c.tx_id = ?3"
        ))
        .bind(root.as_bytes().as_slice())
        .bind(next_seq.to_db_integer())
        .bind(last.tx.as_bytes().as_slice())
        .fetch_optional(&mut *conn)
        .await
        .map_err(ChangeError::Db)?;
        match row {
            Some(row) => batches.push(row.into_batch()?),
            None => return Ok(batches),
        }
    }
}

/// 一个 Root 的事件窗口事实：当前头、epoch 与已出保留窗口的下沿。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChangeWindow {
    /// Root 当前 `RootSeq`（最后一个已提交批次；没有批次为 0）。
    pub head: RootSeq,
    /// Root 当前 epoch。
    pub epoch: ServerEpoch,
    /// 已出保留窗口的最大 seq：`<= expired_through` 的批次不再保证可拉取，游标早于它必须失效。
    pub expired_through: RootSeq,
}

/// 读取事件窗口事实。`retention_cutoff` 之前创建的批次视为已出窗口（design §9.4）。
///
/// 下沿取“创建时间早于截止的最新批次”的 seq：一次索引寻址。它与 `head` 在同一条语句里读出，
/// 所以两者互相一致；调用方应在同一个读事务里接着读批次。
pub async fn change_window(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
    retention_cutoff: UtcTimestamp,
) -> Result<ChangeWindow, ChangeError> {
    let row: Option<(i64, i64, Option<i64>)> = sqlx::query_as(
        "SELECT r.seq, r.epoch, \
                (SELECT c.seq FROM changes c WHERE c.root_id = r.id AND c.created_at < ?3 \
                 ORDER BY c.created_at DESC, c.seq DESC LIMIT 1) \
         FROM roots r WHERE r.id = ?1 AND r.owner_id = ?2",
    )
    .bind(root.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .bind(retention_cutoff.to_db_integer())
    .fetch_optional(conn)
    .await
    .map_err(ChangeError::Db)?;
    let Some((head, epoch, expired)) = row else {
        return Err(ChangeError::OutOfScope);
    };
    Ok(ChangeWindow {
        head: RootSeq::from_db_integer(head)
            .map_err(|_| ChangeError::MalformedRow { column: "seq" })?,
        epoch: ServerEpoch::from_db_integer(epoch)
            .map_err(|_| ChangeError::MalformedRow { column: "epoch" })?,
        expired_through: match expired {
            Some(seq) => RootSeq::from_db_integer(seq)
                .map_err(|_| ChangeError::MalformedRow { column: "seq" })?,
            None => RootSeq::ZERO,
        },
    })
}

/// 确认 Root 属于调用方的 owner scope；不区分“不存在”与“别人的”。
async fn require_root_owner(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
) -> Result<(), ChangeError> {
    let found: Option<Vec<u8>> = sqlx::query_scalar("SELECT owner_id FROM roots WHERE id = ?1")
        .bind(root.as_bytes().as_slice())
        .fetch_optional(conn)
        .await
        .map_err(ChangeError::Db)?;
    match found {
        Some(bytes) if bytes.as_slice() == owner.as_bytes() => Ok(()),
        _ => Err(ChangeError::OutOfScope),
    }
}

/// 数据库行到 [`ChangeBatch`] 的中间形态：序号先读成整数，再按类型检查范围。
#[derive(sqlx::FromRow)]
struct ChangeRow {
    root_id: Vec<u8>,
    seq: i64,
    tx_id: Vec<u8>,
    events_json: String,
    created_at: i64,
}

impl ChangeRow {
    fn into_batch(self) -> Result<ChangeBatch, ChangeError> {
        Ok(ChangeBatch {
            root: identity(self.root_id, "root_id", RootId::from_bytes)?,
            seq: RootSeq::from_db_integer(self.seq)
                .map_err(|_| ChangeError::MalformedRow { column: "seq" })?,
            tx: identity(self.tx_id, "tx_id", TxId::from_bytes)?,
            events_json: self.events_json,
            created_at: UtcTimestamp::from_db_integer(self.created_at).map_err(|_| {
                ChangeError::MalformedRow {
                    column: "created_at",
                }
            })?,
        })
    }
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; vaultsyncd_domain::ids::ID_LEN]) -> T,
) -> Result<T, ChangeError> {
    let array: [u8; vaultsyncd_domain::ids::ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| ChangeError::MalformedRow { column })?;
    Ok(wrap(array))
}

/// 把批次插入语句的约束错误翻译成带上下文的类型化错误。
///
/// `events_json` 的 JSON 形状由迁移里的 CHECK 判定（SQLite 自带 JSON 函数），这里按报错文本里的
/// 列名映射；`(root_id, seq)` 撞车说明 seq 推进与批次行不一致，按坏数据报错而不是猜一个序号。
fn classify_insert_error(error: sqlx::Error) -> ChangeError {
    let Some(database_error) = error.as_database_error() else {
        return ChangeError::Db(error);
    };
    let message = database_error.message();
    if message.contains("events_json") {
        return ChangeError::InvalidEvents;
    }
    ChangeError::Db(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::db::Database;
    use crate::roots;
    use crate::test_support::{
        TestDir, open_database, register_root, seed_owner, tx as tx_id, user,
    };

    fn at(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_seconds(seconds).expect("固定时间戳必须合法")
    }

    const EVENTS_ONE: &str =
        r#"[{"kind":"entry_created","entry":"01"},{"kind":"entry_created","entry":"02"}]"#;
    const EVENTS_TWO: &str = r#"[{"kind":"entry_moved","entry":"01"}]"#;

    async fn root_seq(database: &Database, owner: UserId, root: RootId) -> RootSeq {
        let mut conn = database.pool().acquire().await.expect("取连接");
        roots::root_in_scope(&mut conn, owner, root)
            .await
            .expect("读 Root")
            .expect("Root 必须存在")
            .seq
    }

    #[tokio::test]
    async fn appended_batches_get_increasing_seq_and_are_read_back_whole() {
        let dir = TestDir::new("changes-append");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, _root_entry) = register_root(&database, owner, 0x21).await;
        let (other_root, _other_entry) = register_root(&database, owner, 0x22).await;
        assert_eq!(root_seq(&database, owner, root).await, RootSeq::ZERO);

        let mut tx = database.begin_write().await.expect("开始写事务");
        let first = append_change(&mut tx, owner, root, tx_id(0x31), EVENTS_ONE, at(10))
            .await
            .expect("追加第一批");
        let second = append_change(&mut tx, owner, root, tx_id(0x32), EVENTS_TWO, at(11))
            .await
            .expect("追加第二批");
        let elsewhere = append_change(&mut tx, owner, other_root, tx_id(0x33), EVENTS_TWO, at(12))
            .await
            .expect("给另一个 Root 追加");
        tx.commit().await.expect("提交");

        // 每个 Root 各自从 1 起递增。
        assert_eq!(first.seq, RootSeq::new(1).expect("seq"));
        assert_eq!(second.seq, RootSeq::new(2).expect("seq"));
        assert_eq!(elsewhere.seq, RootSeq::new(1).expect("seq"));
        assert_eq!(root_seq(&database, owner, root).await, second.seq);

        let mut conn = database.pool().acquire().await.expect("取连接");
        let all = changes_after(&mut conn, owner, root, RootSeq::ZERO, 10)
            .await
            .expect("拉取全部批次");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], first, "批次内容与写入时一致（一条记录是一批）");
        assert_eq!(all[1], second);
        assert_eq!(
            all[0].events_json, EVENTS_ONE,
            "多条事件在同一批里，不被拆分"
        );
        assert_eq!(all[0].tx, tx_id(0x31));
        assert_eq!(all[0].created_at, at(10));

        // 分页切的是行：limit=1 拿到的是一条完整批次，不是半批。
        let one = changes_after(&mut conn, owner, root, RootSeq::ZERO, 1)
            .await
            .expect("拉取一页");
        assert_eq!(one, vec![first.clone()]);
        let next = changes_after(&mut conn, owner, root, first.seq, 10)
            .await
            .expect("从 after 之后继续拉");
        assert_eq!(next, vec![second.clone()]);
        assert!(
            changes_after(&mut conn, owner, root, second.seq, 10)
                .await
                .expect("拉取尾部")
                .is_empty()
        );

        // 另一个 Root 的序号独立，不会串到本 Root 的游标上。
        let other = changes_after(&mut conn, owner, other_root, RootSeq::ZERO, 10)
            .await
            .expect("拉取另一个 Root");
        assert_eq!(other, vec![elsewhere]);
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn rolled_back_batches_are_invisible_and_do_not_burn_a_seq() {
        let dir = TestDir::new("changes-rollback");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, _root_entry) = register_root(&database, owner, 0x21).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let rolled_back = append_change(&mut tx, owner, root, tx_id(0x31), EVENTS_ONE, at(10))
            .await
            .expect("追加批次");
        assert_eq!(rolled_back.seq, RootSeq::new(1).expect("seq"));
        tx.rollback().await.expect("回滚");

        // 回滚后没有任何外部可见的事件，seq 也没有被消耗。
        let mut conn = database.pool().acquire().await.expect("取连接");
        assert!(
            changes_after(&mut conn, owner, root, RootSeq::ZERO, 10)
                .await
                .expect("拉取批次")
                .is_empty(),
            "回滚的批次不得暴露"
        );
        drop(conn);
        assert_eq!(root_seq(&database, owner, root).await, RootSeq::ZERO);

        // 下一次提交从 1 开始，不跳号。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let committed = append_change(&mut tx, owner, root, tx_id(0x32), EVENTS_TWO, at(11))
            .await
            .expect("追加批次");
        tx.commit().await.expect("提交");
        assert_eq!(committed.seq, RootSeq::new(1).expect("seq"));
        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            changes_after(&mut conn, owner, root, RootSeq::ZERO, 10)
                .await
                .expect("拉取批次"),
            vec![committed]
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn invalid_events_and_limits_are_rejected_without_advancing_seq() {
        let dir = TestDir::new("changes-invalid");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, _root_entry) = register_root(&database, owner, 0x21).await;

        for events in ["", "not json", "[]", "{}", r#"{"kind":"single"}"#] {
            let mut tx = database.begin_write().await.expect("开始写事务");
            let error = append_change(&mut tx, owner, root, tx_id(0x31), events, at(10))
                .await
                .expect_err("非空 JSON 数组以外的批次必须拒绝");
            assert!(
                matches!(error, ChangeError::InvalidEvents),
                "事件 {events:?} 的实际错误 {error:?}"
            );
            tx.rollback().await.expect("回滚");
        }
        assert_eq!(
            root_seq(&database, owner, root).await,
            RootSeq::ZERO,
            "被拒绝的批次不得占用 seq"
        );

        // owner scope：别人的账号既不能写也不能读。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = append_change(&mut tx, user(0x12), root, tx_id(0x31), EVENTS_ONE, at(10))
            .await
            .expect_err("别人的账号不能追加批次");
        assert!(
            matches!(error, ChangeError::OutOfScope),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        let mut conn = database.pool().acquire().await.expect("取连接");
        for limit in [0, MAX_CHANGE_PAGE + 1] {
            let error = changes_after(&mut conn, owner, root, RootSeq::ZERO, limit)
                .await
                .expect_err("越界的分页大小必须拒绝");
            assert!(matches!(error, ChangeError::InvalidLimit), "limit={limit}");
        }
        let error = changes_after(&mut conn, user(0x12), root, RootSeq::ZERO, 10)
            .await
            .expect_err("别人的账号不得读批次（空列表会被误当成已追平）");
        assert!(
            matches!(error, ChangeError::OutOfScope),
            "实际错误 {error:?}"
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn pages_never_split_a_transaction_that_spans_consecutive_batches() {
        let dir = TestDir::new("changes-whole-tx");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, _root_entry) = register_root(&database, owner, 0x21).await;

        // seq 1：独立事务；seq 2、3、4：同一个事务 0x32 的三个连续批次；seq 5：另一个事务。
        let mut tx = database.begin_write().await.expect("开始写事务");
        for (marker, seconds) in [(0x31, 10), (0x32, 11), (0x32, 11), (0x32, 11), (0x33, 12)] {
            append_change(&mut tx, owner, root, tx_id(marker), EVENTS_TWO, at(seconds))
                .await
                .expect("追加批次");
        }
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let seqs = |batches: &[ChangeBatch]| -> Vec<u64> {
            batches.iter().map(|batch| batch.seq.get()).collect()
        };
        // limit=2 的朴素切法会停在 seq 2，把事务 0x32 切成两半；整事务切法要延伸到 seq 4。
        let page = changes_after_whole_transactions(&mut conn, owner, root, RootSeq::ZERO, 2)
            .await
            .expect("取页");
        assert_eq!(seqs(&page), vec![1, 2, 3, 4], "事务 0x32 的三批同在一页");
        let rest = changes_after_whole_transactions(&mut conn, owner, root, page[3].seq, 2)
            .await
            .expect("取余下");
        assert_eq!(seqs(&rest), vec![5]);
        // 限额正好落在事务边界上：不多取。
        let exact = changes_after_whole_transactions(&mut conn, owner, root, RootSeq::ZERO, 1)
            .await
            .expect("取页");
        assert_eq!(seqs(&exact), vec![1]);
        // 每页拼起来恰好是全部批次，没有丢也没有重。
        let mut cursor = RootSeq::ZERO;
        let mut all = Vec::new();
        loop {
            let page = changes_after_whole_transactions(&mut conn, owner, root, cursor, 1)
                .await
                .expect("逐页取");
            let Some(last) = page.last() else { break };
            cursor = last.seq;
            all.extend(seqs(&page));
        }
        assert_eq!(all, vec![1, 2, 3, 4, 5]);
    }

    #[tokio::test]
    async fn window_reports_head_epoch_and_retention_floor() {
        let dir = TestDir::new("changes-window");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, _root_entry) = register_root(&database, owner, 0x21).await;
        let mut conn = database.pool().acquire().await.expect("取连接");

        let empty = change_window(&mut conn, owner, root, at(1_000))
            .await
            .expect("空窗口");
        assert_eq!(empty.head, RootSeq::ZERO);
        assert_eq!(empty.expired_through, RootSeq::ZERO);
        drop(conn);

        let mut tx = database.begin_write().await.expect("开始写事务");
        for (marker, seconds) in [(0x31, 10), (0x32, 20), (0x33, 30)] {
            append_change(&mut tx, owner, root, tx_id(marker), EVENTS_TWO, at(seconds))
                .await
                .expect("追加批次");
        }
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let cases = [(5, 0), (10, 0), (11, 1), (25, 2), (31, 3), (1_000, 3)];
        for (cutoff, expired) in cases {
            let window = change_window(&mut conn, owner, root, at(cutoff))
                .await
                .expect("窗口");
            assert_eq!(window.head.get(), 3, "cutoff={cutoff}");
            assert_eq!(window.expired_through.get(), expired, "cutoff={cutoff}");
        }
        let error = change_window(&mut conn, user(0x12), root, at(0))
            .await
            .expect_err("别人的账号不得读窗口");
        assert!(matches!(error, ChangeError::OutOfScope), "{error:?}");
    }
}
