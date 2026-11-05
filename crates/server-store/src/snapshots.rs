//! 一致性快照的短期元数据清单（`snapshots`/`snapshot_entries` 表，`docs/design.md` §9.3、I09）。
//!
//! 边界：本模块只读写这两张表与读取 `roots`/`entries`/`versions` 来生成清单；不写命名空间、不分配
//! `RootSeq`、不做 HTTP。创建函数接收调用方已开的 `BEGIN IMMEDIATE` 事务，翻页函数接收连接。
//!
//! 不变量：
//!
//! - **同一时刻的清单**：[`create_snapshot`] 在同一个写事务里读 `roots.seq` 并复制活跃条目。条目变更与
//!   `RootSeq` 推进由提交协调在同一事务里完成，因此清单恰好是“应用到 `high_water_seq` 为止”的状态，
//!   客户端可以从 `changes?after=high_water_seq` 无缝追赶。
//! - **翻页只读清单**：[`snapshot_page`] 只读 `snapshot_entries`，翻页期间 live 表的新增/删除/改名都不会
//!   出现在后续页里，已出现的条目也不会消失。元数据与条目行在同一个读事务里取，不会出现
//!   “元数据在、条目已被回收”的空页。
//! - **过期是明确错误**：快照不存在、不属于该 Root/owner，或已过 `expires_at`，一律
//!   [`SnapshotError::Expired`]，不返回空页。调用方据此要求客户端重启快照。三种情形不区分，既不泄露
//!   别人的快照，也因为身份是 128 位随机值、已回收的快照与“从未存在”对客户端是同一回事。
//! - **有界**：每页至多 [`MAX_SNAPSHOT_PAGE`] 条；每个 Root 至多 [`MAX_LIVE_SNAPSHOTS_PER_ROOT`] 份未过期
//!   快照，超出时回收最旧的（它们的持有者翻页时得到 `Expired` 并重启），存储因此有上界。
//! - **owner scope**：所有读写带 [`UserId`]，Root 不在 scope 内是 [`SnapshotError::OutOfScope`]，
//!   不是空清单。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{EntryId, ID_LEN, RootId, SnapshotId, UserId, VersionId};
use vaultsyncd_domain::version::{Revision, RootSeq, ServerEpoch, UtcTimestamp};

use crate::entries::EntryKind;

/// 一页最多返回的条目数（design §15.2：所有清单分页有上限）。
pub const MAX_SNAPSHOT_PAGE: u32 = 1000;

/// 每个 Root 同时存在的未过期快照上限。
pub const MAX_LIVE_SNAPSHOTS_PER_ROOT: i64 = 8;

/// 快照的元数据（`snapshots` 的一行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRecord {
    /// 快照身份（128 位随机值，由调用方经 `IdSource` 生成）。
    pub id: SnapshotId,
    /// 所属 Root。
    pub root: RootId,
    /// 创建快照时 Root 的 `RootSeq`。
    pub high_water_seq: RootSeq,
    /// 创建时的 Root epoch。
    pub epoch: ServerEpoch,
    /// 清单里的条目总数。
    pub entry_count: u64,
    /// 创建时间。
    pub created_at: UtcTimestamp,
    /// 过期时间；此刻及之后不再提供翻页。
    pub expires_at: UtcTimestamp,
}

/// 快照清单里的一条条目（创建时的值拷贝）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotEntry {
    /// 条目身份。
    pub entry: EntryId,
    /// 父目录；合成根条目没有。
    pub parent: Option<EntryId>,
    /// 原始名称（合成根条目为空串）。
    pub name: String,
    /// 文件或目录。
    pub kind: EntryKind,
    /// 创建快照时的条目 revision。
    pub revision: Revision,
    /// 当前内容版本（目录与尚无内容的文件为 `None`）。
    pub version: Option<VersionId>,
    /// 当前内容长度。
    pub size: Option<u64>,
    /// 当前内容 SHA-256。
    pub hash: Option<[u8; 32]>,
}

/// 一页清单。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotPage {
    /// 快照元数据（与条目同一个读事务取得）。
    pub snapshot: SnapshotRecord,
    /// 本页条目，按 EntryId 升序。
    pub entries: Vec<SnapshotEntry>,
    /// 下一页的游标；`None` 表示已是最后一页。
    pub next_cursor: Option<u64>,
}

/// 快照读写失败。
#[derive(Debug)]
pub enum SnapshotError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// Root 不存在或不属于调用方的 owner scope。
    OutOfScope,
    /// 快照不存在、不属于该 Root/owner，或已过期；客户端必须重启快照。
    Expired,
    /// 分页大小不在 `1..=MAX_SNAPSHOT_PAGE`。
    InvalidLimit,
    /// 游标超过清单长度。
    InvalidCursor,
    /// 过期时间不晚于创建时间。
    InvalidLifetime,
    /// 数据库行不满足本模块的类型前提。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::OutOfScope => f.write_str("Root 不存在或不在调用方的 owner scope 内"),
            Self::Expired => f.write_str("快照不存在或已过期，需要重新创建"),
            Self::InvalidLimit => write!(f, "分页大小必须在 1..={MAX_SNAPSHOT_PAGE} 之内"),
            Self::InvalidCursor => f.write_str("游标超过快照清单长度"),
            Self::InvalidLifetime => f.write_str("快照过期时间必须晚于创建时间"),
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for SnapshotError {
    fn from(source: sqlx::Error) -> Self {
        Self::Db(source)
    }
}

/// 生成一份快照：同一写事务里读 `roots.seq`、复制活跃条目、写元数据。
///
/// 同时回收已过期的快照（任意 Root），并把本 Root 的未过期快照数压到
/// [`MAX_LIVE_SNAPSHOTS_PER_ROOT`] 以内（给新快照留位置）。
pub async fn create_snapshot(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    root: RootId,
    id: SnapshotId,
    now: UtcTimestamp,
    expires_at: UtcTimestamp,
) -> Result<SnapshotRecord, SnapshotError> {
    if expires_at.as_unix_seconds() <= now.as_unix_seconds() {
        return Err(SnapshotError::InvalidLifetime);
    }
    let head: Option<(i64, i64)> =
        sqlx::query_as("SELECT seq, epoch FROM roots WHERE id = ?1 AND owner_id = ?2")
            .bind(root.as_bytes().as_slice())
            .bind(owner.as_bytes().as_slice())
            .fetch_optional(&mut **tx)
            .await?;
    let Some((seq, epoch)) = head else {
        return Err(SnapshotError::OutOfScope);
    };
    let high_water_seq =
        RootSeq::from_db_integer(seq).map_err(|_| SnapshotError::MalformedRow { column: "seq" })?;
    let epoch = ServerEpoch::from_db_integer(epoch)
        .map_err(|_| SnapshotError::MalformedRow { column: "epoch" })?;

    // 回收：先清条目再清元数据（外键 CASCADE 兜底）。
    sqlx::query(
        "DELETE FROM snapshot_entries WHERE snapshot_id IN \
         (SELECT id FROM snapshots WHERE expires_at <= ?1)",
    )
    .bind(now.to_db_integer())
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM snapshots WHERE expires_at <= ?1")
        .bind(now.to_db_integer())
        .execute(&mut **tx)
        .await?;
    // 本 Root 最多保留 N-1 份，给新快照腾位置；最旧的先走。
    sqlx::query(
        "DELETE FROM snapshots WHERE id IN (\
           SELECT id FROM snapshots WHERE root_id = ?1 \
           ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?2)",
    )
    .bind(root.as_bytes().as_slice())
    .bind(MAX_LIVE_SNAPSHOTS_PER_ROOT - 1)
    .execute(&mut **tx)
    .await?;
    // 条目行靠外键 CASCADE 回收（连接选项里外键常开）。

    let entry_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM entries WHERE root_id = ?1 AND deleted_at IS NULL",
    )
    .bind(root.as_bytes().as_slice())
    .fetch_one(&mut **tx)
    .await?;

    sqlx::query(
        "INSERT INTO snapshots \
         (id, root_id, owner_id, high_water_seq, epoch, entry_count, created_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(root.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .bind(high_water_seq.to_db_integer())
    .bind(epoch.to_db_integer())
    .bind(entry_count)
    .bind(now.to_db_integer())
    .bind(expires_at.to_db_integer())
    .execute(&mut **tx)
    .await?;

    let copied = sqlx::query(
        "INSERT INTO snapshot_entries \
         (snapshot_id, ordinal, entry_id, parent_id, name, kind, revision, \
          content_version_id, content_size, content_hash) \
         SELECT ?1, ROW_NUMBER() OVER (ORDER BY e.id) - 1, e.id, e.parent_id, e.name, e.kind, \
                e.revision, e.content_version_id, v.size, v.object_hash \
         FROM entries e LEFT JOIN versions v ON v.id = e.content_version_id \
         WHERE e.root_id = ?2 AND e.deleted_at IS NULL",
    )
    .bind(id.as_bytes().as_slice())
    .bind(root.as_bytes().as_slice())
    .execute(&mut **tx)
    .await?;
    if i64::try_from(copied.rows_affected()).ok() != Some(entry_count) {
        return Err(SnapshotError::MalformedRow {
            column: "entry_count",
        });
    }

    Ok(SnapshotRecord {
        id,
        root,
        high_water_seq,
        epoch,
        entry_count: u64::try_from(entry_count).unwrap_or(0),
        created_at: now,
        expires_at,
    })
}

/// 取快照的一页条目。
///
/// `cursor` 是起始序号（首页为 0）；返回的 `next_cursor` 是下一页起始序号。快照不存在、不属于该
/// Root/owner 或已过期是 [`SnapshotError::Expired`]，不会得到空页。
pub async fn snapshot_page(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
    snapshot: SnapshotId,
    cursor: u64,
    limit: u32,
    now: UtcTimestamp,
) -> Result<SnapshotPage, SnapshotError> {
    if limit == 0 || limit > MAX_SNAPSHOT_PAGE {
        return Err(SnapshotError::InvalidLimit);
    }
    let cursor = i64::try_from(cursor).map_err(|_| SnapshotError::InvalidCursor)?;
    // 元数据与条目同在一个读事务里：并发回收不会让本页变成“元数据在、条目空”的假空页。
    let mut read = sqlx::Connection::begin(conn).await?;

    let meta = sqlx::query_as::<_, SnapshotRow>(
        "SELECT id, root_id, high_water_seq, epoch, entry_count, created_at, expires_at \
         FROM snapshots WHERE id = ?1 AND root_id = ?2 AND owner_id = ?3 AND expires_at > ?4",
    )
    .bind(snapshot.as_bytes().as_slice())
    .bind(root.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .bind(now.to_db_integer())
    .fetch_optional(&mut *read)
    .await?;
    let Some(meta) = meta else {
        return Err(SnapshotError::Expired);
    };
    let record = meta.into_record()?;
    if cursor > i64::try_from(record.entry_count).unwrap_or(i64::MAX) {
        return Err(SnapshotError::InvalidCursor);
    }

    let rows = sqlx::query_as::<_, EntryRow>(
        "SELECT entry_id, parent_id, name, kind, revision, content_version_id, content_size, \
                content_hash \
         FROM snapshot_entries WHERE snapshot_id = ?1 AND ordinal >= ?2 \
         ORDER BY ordinal LIMIT ?3",
    )
    .bind(snapshot.as_bytes().as_slice())
    .bind(cursor)
    .bind(i64::from(limit))
    .fetch_all(&mut *read)
    .await?;
    read.commit().await?;

    let entries = rows
        .into_iter()
        .map(EntryRow::into_entry)
        .collect::<Result<Vec<_>, _>>()?;
    let consumed = u64::try_from(cursor).unwrap_or(0) + entries.len() as u64;
    let next_cursor = (consumed < record.entry_count).then_some(consumed);
    Ok(SnapshotPage {
        snapshot: record,
        entries,
        next_cursor,
    })
}

#[derive(sqlx::FromRow)]
struct SnapshotRow {
    id: Vec<u8>,
    root_id: Vec<u8>,
    high_water_seq: i64,
    epoch: i64,
    entry_count: i64,
    created_at: i64,
    expires_at: i64,
}

impl SnapshotRow {
    fn into_record(self) -> Result<SnapshotRecord, SnapshotError> {
        Ok(SnapshotRecord {
            id: SnapshotId::from_bytes(identity_bytes(self.id, "id")?),
            root: RootId::from_bytes(identity_bytes(self.root_id, "root_id")?),
            high_water_seq: RootSeq::from_db_integer(self.high_water_seq).map_err(|_| {
                SnapshotError::MalformedRow {
                    column: "high_water_seq",
                }
            })?,
            epoch: ServerEpoch::from_db_integer(self.epoch)
                .map_err(|_| SnapshotError::MalformedRow { column: "epoch" })?,
            entry_count: u64::try_from(self.entry_count).map_err(|_| {
                SnapshotError::MalformedRow {
                    column: "entry_count",
                }
            })?,
            created_at: UtcTimestamp::from_db_integer(self.created_at).map_err(|_| {
                SnapshotError::MalformedRow {
                    column: "created_at",
                }
            })?,
            expires_at: UtcTimestamp::from_db_integer(self.expires_at).map_err(|_| {
                SnapshotError::MalformedRow {
                    column: "expires_at",
                }
            })?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct EntryRow {
    entry_id: Vec<u8>,
    parent_id: Option<Vec<u8>>,
    name: String,
    kind: String,
    revision: i64,
    content_version_id: Option<Vec<u8>>,
    content_size: Option<i64>,
    content_hash: Option<Vec<u8>>,
}

impl EntryRow {
    fn into_entry(self) -> Result<SnapshotEntry, SnapshotError> {
        Ok(SnapshotEntry {
            entry: EntryId::from_bytes(identity_bytes(self.entry_id, "entry_id")?),
            parent: self
                .parent_id
                .map(|bytes| identity_bytes(bytes, "parent_id").map(EntryId::from_bytes))
                .transpose()?,
            name: self.name,
            kind: EntryKind::from_text(&self.kind)
                .ok_or(SnapshotError::MalformedRow { column: "kind" })?,
            revision: Revision::from_db_integer(self.revision)
                .map_err(|_| SnapshotError::MalformedRow { column: "revision" })?,
            version: self
                .content_version_id
                .map(|bytes| identity_bytes(bytes, "content_version_id").map(VersionId::from_bytes))
                .transpose()?,
            size: self
                .content_size
                .map(|size| {
                    u64::try_from(size).map_err(|_| SnapshotError::MalformedRow {
                        column: "content_size",
                    })
                })
                .transpose()?,
            hash: self
                .content_hash
                .map(|bytes| {
                    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
                        SnapshotError::MalformedRow {
                            column: "content_hash",
                        }
                    })
                })
                .transpose()?,
        })
    }
}

/// 身份列长度检查：读到短值就明确报坏行，不截断、不补零。
fn identity_bytes(bytes: Vec<u8>, column: &'static str) -> Result<[u8; ID_LEN], SnapshotError> {
    <[u8; ID_LEN]>::try_from(bytes.as_slice()).map_err(|_| SnapshotError::MalformedRow { column })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::entries;
    use crate::test_support::{
        TestDir, at, entry, open_database, register_root, seed_file, seed_object, seed_owner,
        tx as tx_id,
    };
    use crate::{changes, versions};

    const SNAPSHOT: SnapshotId = SnapshotId::from_bytes([0x5a; ID_LEN]);

    fn sid(marker: u8) -> SnapshotId {
        SnapshotId::from_bytes([marker; ID_LEN])
    }

    async fn create(
        database: &crate::db::Database,
        owner: UserId,
        root: RootId,
        id: SnapshotId,
        now: i64,
        ttl: i64,
    ) -> SnapshotRecord {
        let mut tx = database.begin_write().await.expect("写事务");
        let record = create_snapshot(&mut tx, owner, root, id, at(now), at(now + ttl))
            .await
            .expect("创建快照");
        tx.commit().await.expect("提交快照");
        record
    }

    async fn page(
        database: &crate::db::Database,
        owner: UserId,
        root: RootId,
        id: SnapshotId,
        cursor: u64,
        limit: u32,
        now: i64,
    ) -> Result<SnapshotPage, SnapshotError> {
        let mut conn = database.pool().acquire().await.expect("取连接");
        snapshot_page(&mut conn, owner, root, id, cursor, limit, at(now)).await
    }

    #[tokio::test]
    async fn pages_read_the_manifest_not_the_live_tables() {
        let dir = TestDir::new("snap-consistent");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, root_entry) = register_root(&database, owner, 0x21).await;
        for marker in 0x30..0x35 {
            seed_file(
                &database,
                owner,
                root,
                root_entry,
                marker,
                &format!("f{marker}"),
            )
            .await;
        }
        let record = create(&database, owner, root, SNAPSHOT, 100, 600).await;
        assert_eq!(record.entry_count, 6, "5 个文件 + 合成根条目");
        assert_eq!(record.high_water_seq, RootSeq::ZERO);

        let first = page(&database, owner, root, SNAPSHOT, 0, 4, 101)
            .await
            .expect("首页");
        assert_eq!(first.entries.len(), 4);
        assert_eq!(first.next_cursor, Some(4));

        // 翻页期间：新增、删除、改名都发生在 live 表。
        seed_file(&database, owner, root, root_entry, 0x40, "added-later").await;
        let mut tx = database.begin_write().await.expect("写事务");
        entries::soft_delete_entry(&mut tx, owner, entry(0x30), at(102))
            .await
            .expect("软删除");
        tx.commit().await.expect("提交");

        let second = page(&database, owner, root, SNAPSHOT, 4, 4, 103)
            .await
            .expect("次页");
        assert_eq!(second.entries.len(), 2);
        assert_eq!(second.next_cursor, None);
        let mut seen: Vec<EntryId> = first
            .entries
            .iter()
            .chain(second.entries.iter())
            .map(|entry| entry.entry)
            .collect();
        seen.sort_by_key(|id| *id.as_bytes());
        let expected: Vec<EntryId> = std::iter::once(root_entry)
            .chain((0x30..0x35).map(entry))
            .collect::<Vec<_>>();
        let mut expected_sorted = expected;
        expected_sorted.sort_by_key(|id| *id.as_bytes());
        assert_eq!(
            seen, expected_sorted,
            "清单恰为创建时刻的 6 条，后续变更不出现"
        );
        assert!(
            seen.iter().all(|id| *id != entry(0x40)),
            "翻页期间新增的条目不能出现"
        );
        assert!(seen.contains(&entry(0x30)), "翻页期间被删除的条目不能消失");
        assert!(
            first.entries.last().map(|e| *e.entry.as_bytes())
                < second.entries.first().map(|e| *e.entry.as_bytes()),
            "页间按 EntryId 升序衔接"
        );
    }

    #[tokio::test]
    async fn expired_unknown_and_foreign_snapshots_are_errors_not_empty_pages() {
        let dir = TestDir::new("snap-expired");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let stranger = seed_owner(&database, 0x12).await;
        let (root, _) = register_root(&database, owner, 0x21).await;
        create(&database, owner, root, SNAPSHOT, 100, 60).await;

        assert!(
            page(&database, owner, root, SNAPSHOT, 0, 10, 159)
                .await
                .is_ok()
        );
        for (label, who, id, now) in [
            ("到期", owner, SNAPSHOT, 160),
            ("未知", owner, sid(0x01), 120),
            ("别人的账号", stranger, SNAPSHOT, 120),
        ] {
            let error = page(&database, who, root, id, 0, 10, now)
                .await
                .expect_err(label);
            assert!(matches!(error, SnapshotError::Expired), "{label}：{error}");
        }
        let (other_root, _) = register_root(&database, owner, 0x22).await;
        assert!(matches!(
            page(&database, owner, other_root, SNAPSHOT, 0, 10, 120).await,
            Err(SnapshotError::Expired)
        ));
        assert!(matches!(
            page(&database, owner, root, SNAPSHOT, 0, 0, 120).await,
            Err(SnapshotError::InvalidLimit)
        ));
        assert!(matches!(
            page(&database, owner, root, SNAPSHOT, 99, 10, 120).await,
            Err(SnapshotError::InvalidCursor)
        ));
    }

    #[tokio::test]
    async fn creating_recycles_expired_and_surplus_snapshots() {
        let dir = TestDir::new("snap-recycle");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, _) = register_root(&database, owner, 0x21).await;
        create(&database, owner, root, sid(0x01), 100, 10).await;
        // 第一份已过期（now=200 > 110），创建新快照时被回收。
        create(&database, owner, root, sid(0x02), 200, 1000).await;
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM snapshots")
            .fetch_one(database.pool())
            .await
            .expect("统计");
        assert_eq!(rows, 1);
        let orphans: i64 =
            sqlx::query_scalar("SELECT count(*) FROM snapshot_entries WHERE snapshot_id = ?1")
                .bind([0x01_u8; ID_LEN].as_slice())
                .fetch_one(database.pool())
                .await
                .expect("统计孤儿");
        assert_eq!(orphans, 0, "过期快照的条目行随元数据一起回收");

        for index in 0..u8::try_from(MAX_LIVE_SNAPSHOTS_PER_ROOT + 3).expect("上限很小") {
            create(
                &database,
                owner,
                root,
                sid(0x10 + index),
                300 + i64::from(index),
                1000,
            )
            .await;
        }
        let live: i64 = sqlx::query_scalar("SELECT count(*) FROM snapshots WHERE root_id = ?1")
            .bind(root.as_bytes().as_slice())
            .fetch_one(database.pool())
            .await
            .expect("统计");
        assert_eq!(
            live, MAX_LIVE_SNAPSHOTS_PER_ROOT,
            "每个 Root 的快照数有上界"
        );
        assert!(matches!(
            page(&database, owner, root, sid(0x10), 0, 10, 400).await,
            Err(SnapshotError::Expired)
        ));
    }

    #[tokio::test]
    async fn manifest_carries_the_content_version_facts() {
        let dir = TestDir::new("snap-content");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, root_entry) = register_root(&database, owner, 0x21).await;
        let file = seed_file(&database, owner, root, root_entry, 0x30, "a.txt").await;
        let bare = seed_file(&database, owner, root, root_entry, 0x31, "b.txt").await;
        let hash = seed_object(&database, owner, root, 0x61, 7).await;
        let version = VersionId::from_bytes([0x71; ID_LEN]);
        let mut tx = database.begin_write().await.expect("写事务");
        let batch = changes::append_change(
            &mut tx,
            owner,
            root,
            tx_id(0x41),
            r#"[{"kind":"put"}]"#,
            at(50),
        )
        .await
        .expect("追加批次");
        versions::record_version(
            &mut tx,
            owner,
            &versions::NewVersion {
                id: version,
                root,
                entry: file,
                object_hash: hash,
                size: 7,
                created_seq: batch.seq,
                origin: versions::VersionOrigin::Client,
                at: at(50),
            },
        )
        .await
        .expect("记录版本");
        tx.commit().await.expect("提交");

        let created = create(&database, owner, root, SNAPSHOT, 100, 600).await;
        assert_eq!(created.entry_count, 3);
        assert_eq!(
            created.high_water_seq, batch.seq,
            "high_water_seq 是创建时刻的 RootSeq"
        );
        let listed = page(&database, owner, root, SNAPSHOT, 0, 10, 101)
            .await
            .expect("取页");
        let root_row = listed
            .entries
            .iter()
            .find(|row| row.entry == root_entry)
            .expect("含合成根条目");
        assert_eq!((root_row.parent, root_row.name.as_str()), (None, ""));
        let file_row = listed
            .entries
            .iter()
            .find(|row| row.entry == file)
            .expect("文件");
        assert_eq!(file_row.parent, Some(root_entry));
        assert_eq!(file_row.name, "a.txt");
        assert_eq!(file_row.kind, EntryKind::File);
        assert_eq!(
            (file_row.version, file_row.size, file_row.hash),
            (Some(version), Some(7), Some(hash)),
            "带当前版本的文件给出版本、长度与哈希"
        );
        let bare_row = listed
            .entries
            .iter()
            .find(|row| row.entry == bare)
            .expect("无内容文件");
        assert_eq!(
            (bare_row.version, bare_row.size, bare_row.hash),
            (None, None, None),
            "尚无内容版本的文件不带内容事实"
        );
    }
}
