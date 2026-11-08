//! 回收站清单与墓碑（`trash_entries`、`tombstones` 表；`docs/design.md` §10.3、§7.2）。
//!
//! 边界：本模块只读写这两张表与 `entries.deleted_at`，不做文件系统 I/O、不分配 RootSeq、不写
//! `changes`。删除事务的编排（冻结子树、有界批次、事件批次、意图收尾）在
//! `crates/server-core` 的 `commit::delete` 与它的存储适配器里；本模块提供它需要的三件事：
//!
//! 1. [`soft_delete_batch`]：按**有界批次**软删除条目，每条的 `revision` 都是比较并写的条件——
//!    库里的 revision 已前进就整批失败，不覆盖别人的提交；
//! 2. [`record_trash_entry`] / [`insert_tombstone`]：把原位置与内容引用写进回收站清单，并把“这个条目
//!    没了”写成墓碑。两者都在调用方的写事务里执行，因而与软删除、事件批次同一事务提交；
//! 3. [`trash_entries`] / [`trash_entry`] / [`tombstones`]：按 owner scope 读清单与墓碑，供恢复
//!    （S040）与同步协议（墓碑压缩）使用。
//!
//! 不变量：
//!
//! - **owner scope**：每个函数的业务参数都有 [`UserId`]，Root 只能经 `roots.owner_id` 访问；不属于该
//!   owner 的 Root 一律 [`TrashError::OutOfScope`]，不区分“不存在”与“别人的”。
//! - **删除不动内容**：本模块不写 `objects`/`versions`，也不改 `entries.content_version_id`。回收站里
//!   的内容版本在 `expires_at` 之前仍可读，历史版本照常可查（design §10.3、I12）。
//! - **同一身份只能进回收站一次**：`trash_entries.entry_id` 是主键，重复写返回
//!   [`TrashError::AlreadyInTrash`]，而不是把两条删除当成一次。
//! - **目录没有内容版本**：迁移里的 `CHECK` 与这里的前置检查一致，违反返回
//!   [`TrashError::DirectoryWithContent`]。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{EntryId, ID_LEN, RootId, UserId, VersionId};
use vaultsyncd_domain::version::{Revision, UtcTimestamp};

use crate::entries::EntryKind;

/// 一批软删除的条目上限（与核心的 `MAX_DELETE_BATCH_ENTRIES` 同一量级）。
///
/// 适配器必须按这个粒度切片：一条语句删整棵树会把大树变成不可中断的写锁持有者。
pub const MAX_SOFT_DELETE_BATCH: usize = 256;

/// `trash_entries`/`tombstones` 的列顺序，多处查询共用，避免手写列名漂移。
const TRASH_COLUMNS: &str = "t.entry_id, t.root_id, t.parent_id, t.path, t.kind, t.revision, \
                             t.content_version_id, t.deleted_at, t.expires_at";
const TOMBSTONE_COLUMNS: &str = "t.root_id, t.entry_id, t.path, t.kind, t.deleted_at, t.expires_at";

/// 写一条回收站清单的输入。
#[derive(Clone, Debug)]
pub struct NewTrashEntry {
    /// 被删除的条目身份。
    pub entry: EntryId,
    /// 所属 Root。
    pub root: RootId,
    /// 删除时的父目录。
    pub parent: EntryId,
    /// 删除时的 Root 内相对路径。
    pub path: String,
    /// 条目种类。
    pub kind: EntryKind,
    /// 删除时的条目 revision。
    pub revision: Revision,
    /// 删除时的当前内容版本；目录必须为 `None`。
    pub content_version: Option<VersionId>,
    /// 删除时间。
    pub deleted_at: UtcTimestamp,
    /// 内容最早可被回收的时间（`deleted_at` + 保留期，由调用方算）。
    pub expires_at: UtcTimestamp,
}

/// 从数据库读出的一条回收站清单行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashEntry {
    /// 被删除的条目身份。
    pub entry: EntryId,
    /// 所属 Root。
    pub root: RootId,
    /// 删除时的父目录。
    pub parent: EntryId,
    /// 原路径。
    pub path: String,
    /// 条目种类。
    pub kind: EntryKind,
    /// 删除时的 revision。
    pub revision: Revision,
    /// 删除时的当前内容版本。
    pub content_version: Option<VersionId>,
    /// 删除时间。
    pub deleted_at: UtcTimestamp,
    /// 内容最早可被回收的时间。
    pub expires_at: UtcTimestamp,
}

/// 一条墓碑：旧设备基线据此判断条目已经消失。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tombstone {
    /// 所属 Root。
    pub root: RootId,
    /// 被删除的条目身份。
    pub entry: EntryId,
    /// 原路径。
    pub path: String,
    /// 条目种类。
    pub kind: EntryKind,
    /// 删除时间。
    pub deleted_at: UtcTimestamp,
    /// 与回收站清单同一个保留期。
    pub expires_at: UtcTimestamp,
}

/// 回收站与墓碑读写的失败。
#[derive(Debug)]
pub enum TrashError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// Root 不存在或不属于调用方的 owner scope（两种情况不区分）。
    OutOfScope,
    /// 条目不在该 Root 内。
    EntryNotInRoot {
        /// 请求的条目。
        entry: EntryId,
    },
    /// 该条目已经在回收站清单里：同一身份不能被删两次。
    AlreadyInTrash {
        /// 命中的条目。
        entry: EntryId,
    },
    /// 同一 Root 里已经有这个条目的墓碑。
    DuplicateTombstone {
        /// 命中的条目。
        entry: EntryId,
    },
    /// 一批软删除条目为空，或超过 [`MAX_SOFT_DELETE_BATCH`]。
    InvalidBatch {
        /// 本批实际条数。
        entries: usize,
    },
    /// 比较并写失败：条目已不是活跃的、或 revision 已变（不覆盖别人的提交）。
    BatchMismatch {
        /// 命中的条目。
        entry: EntryId,
    },
    /// 路径不是合法的 Root 内相对路径。
    InvalidPath {
        /// 请求的路径。
        path: String,
        /// 原因文本。
        reason: &'static str,
    },
    /// 目录带着内容版本（与迁移里的 `CHECK` 同一规则）。
    DirectoryWithContent {
        /// 命中的条目。
        entry: EntryId,
    },
    /// 保留期早于删除时间。
    InvalidRetention {
        /// 命中的条目。
        entry: EntryId,
    },
    /// 读到的行无法还原（身份长度、revision 越界、种类未知）。
    MalformedRow {
        /// 原因文本。
        detail: String,
    },
}

impl TrashError {
    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    pub fn code(&self) -> &'static str {
        match self {
            TrashError::Db(_) => "trash_storage_failed",
            TrashError::OutOfScope => "out_of_scope",
            TrashError::EntryNotInRoot { .. } => "entry_not_in_root",
            TrashError::AlreadyInTrash { .. } => "already_in_trash",
            TrashError::DuplicateTombstone { .. } => "duplicate_tombstone",
            TrashError::InvalidBatch { .. } => "invalid_batch",
            TrashError::BatchMismatch { .. } => "batch_mismatch",
            TrashError::InvalidPath { .. } => "invalid_path",
            TrashError::DirectoryWithContent { .. } => "directory_with_content",
            TrashError::InvalidRetention { .. } => "invalid_retention",
            TrashError::MalformedRow { .. } => "malformed_trash_row",
        }
    }
}

impl std::fmt::Display for TrashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrashError::Db(error) => write!(f, "回收站存储失败：{error}"),
            TrashError::OutOfScope => f.write_str("Root 不在调用方的 owner scope 内"),
            TrashError::EntryNotInRoot { entry } => write!(f, "条目 {entry} 不在该 Root 内"),
            TrashError::AlreadyInTrash { entry } => write!(f, "条目 {entry} 已经在回收站里"),
            TrashError::DuplicateTombstone { entry } => write!(f, "条目 {entry} 已有墓碑"),
            TrashError::InvalidBatch { entries } => write!(
                f,
                "软删除批次条数 {entries} 不合法（应为 1..={MAX_SOFT_DELETE_BATCH}）"
            ),
            TrashError::BatchMismatch { entry } => write!(
                f,
                "条目 {entry} 已不是活跃条目或 revision 已变：比较并写失败"
            ),
            TrashError::InvalidPath { path, reason } => {
                write!(f, "路径 {path:?} 不合法：{reason}")
            }
            TrashError::DirectoryWithContent { entry } => {
                write!(f, "目录条目 {entry} 不能带内容版本")
            }
            TrashError::InvalidRetention { entry } => {
                write!(f, "条目 {entry} 的保留期早于删除时间")
            }
            TrashError::MalformedRow { detail } => write!(f, "回收站行无法还原：{detail}"),
        }
    }
}

impl std::error::Error for TrashError {}

impl From<sqlx::Error> for TrashError {
    fn from(error: sqlx::Error) -> Self {
        TrashError::Db(error)
    }
}

/// 一批软删除的输入：身份 + 该条目在冻结清单里的 revision。
///
/// revision 是比较并写的条件：调用方在事务外冻结清单，事务里必须仍看到同一个 revision。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoftDelete {
    /// 被删除的条目。
    pub entry: EntryId,
    /// 冻结清单里的 revision。
    pub revision: Revision,
}

/// 按有界批次软删除条目：写 `entries.deleted_at`，同时释放同父名称。
///
/// 每批必须非空且不超过 [`MAX_SOFT_DELETE_BATCH`]；任一条目不是活跃条目、或 revision 已变，就整批
/// 返回 [`TrashError::BatchMismatch`]（调用方的事务应当回滚，不提交半批）。
///
/// 返回实际删除的条数（等于批次长度）。
pub async fn soft_delete_batch(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    root: RootId,
    batch: &[SoftDelete],
    at: UtcTimestamp,
) -> Result<usize, TrashError> {
    if batch.is_empty() || batch.len() > MAX_SOFT_DELETE_BATCH {
        return Err(TrashError::InvalidBatch {
            entries: batch.len(),
        });
    }
    require_root_owner(tx, owner, root).await?;
    for item in batch {
        let updated = sqlx::query(
            "UPDATE entries SET deleted_at = ?1 \
             WHERE id = ?2 AND root_id = ?3 AND deleted_at IS NULL AND revision = ?4",
        )
        .bind(at.to_db_integer())
        .bind(item.entry.as_bytes().as_slice())
        .bind(root.as_bytes().as_slice())
        .bind(item.revision.to_db_integer())
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(TrashError::BatchMismatch { entry: item.entry });
        }
    }
    Ok(batch.len())
}

/// 写一条回收站清单行并读回。
///
/// 前置检查：Root 属于 owner；条目在该 Root 内；路径是 Root 内相对路径；目录不带内容版本；
/// `expires_at` 不早于 `deleted_at`；条目还没有回收站清单行。
pub async fn record_trash_entry(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    new: &NewTrashEntry,
) -> Result<TrashEntry, TrashError> {
    require_root_owner(tx, owner, new.root).await?;
    require_entry_in_root(tx, new.root, new.entry).await?;
    validate_path(&new.path)?;
    if new.kind == EntryKind::Directory && new.content_version.is_some() {
        return Err(TrashError::DirectoryWithContent { entry: new.entry });
    }
    if new.expires_at < new.deleted_at {
        return Err(TrashError::InvalidRetention { entry: new.entry });
    }

    let inserted = sqlx::query(
        "INSERT INTO trash_entries \
         (entry_id, root_id, parent_id, path, kind, revision, content_version_id, deleted_at, \
          expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(new.entry.as_bytes().as_slice())
    .bind(new.root.as_bytes().as_slice())
    .bind(new.parent.as_bytes().as_slice())
    .bind(&new.path)
    .bind(new.kind.as_text())
    .bind(new.revision.to_db_integer())
    .bind(
        new.content_version
            .map(|version| version.as_bytes().to_vec()),
    )
    .bind(new.deleted_at.to_db_integer())
    .bind(new.expires_at.to_db_integer())
    .execute(&mut **tx)
    .await;
    if let Err(error) = inserted {
        if is_unique_violation(&error, "trash_entries") {
            return Err(TrashError::AlreadyInTrash { entry: new.entry });
        }
        return Err(TrashError::Db(error));
    }
    trash_entry(tx, owner, new.root, new.entry)
        .await?
        .ok_or(TrashError::EntryNotInRoot { entry: new.entry })
}

/// 写一条墓碑并读回（同一个条目只能有一条墓碑）。
pub async fn insert_tombstone(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    new: &NewTrashEntry,
) -> Result<Tombstone, TrashError> {
    require_root_owner(tx, owner, new.root).await?;
    require_entry_in_root(tx, new.root, new.entry).await?;
    validate_path(&new.path)?;
    if new.expires_at < new.deleted_at {
        return Err(TrashError::InvalidRetention { entry: new.entry });
    }
    let inserted = sqlx::query(
        "INSERT INTO tombstones (root_id, entry_id, path, kind, deleted_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(new.root.as_bytes().as_slice())
    .bind(new.entry.as_bytes().as_slice())
    .bind(&new.path)
    .bind(new.kind.as_text())
    .bind(new.deleted_at.to_db_integer())
    .bind(new.expires_at.to_db_integer())
    .execute(&mut **tx)
    .await;
    if let Err(error) = inserted {
        if is_unique_violation(&error, "tombstones") {
            return Err(TrashError::DuplicateTombstone { entry: new.entry });
        }
        return Err(TrashError::Db(error));
    }
    tombstone(tx, owner, new.root, new.entry)
        .await?
        .ok_or(TrashError::EntryNotInRoot { entry: new.entry })
}

/// 读一个 Root 的回收站清单，按路径（再按身份）排序。
///
/// Root 不在调用方的 owner scope 内时返回 [`TrashError::OutOfScope`]，不返回空列表冒充“没有可恢复
/// 内容”。
pub async fn trash_entries(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
) -> Result<Vec<TrashEntry>, TrashError> {
    require_root_owner(conn, owner, root).await?;
    let rows = sqlx::query_as::<_, TrashRow>(&format!(
        "SELECT {TRASH_COLUMNS} FROM trash_entries t JOIN roots r ON r.id = t.root_id \
         WHERE t.root_id = ?1 AND r.owner_id = ?2 ORDER BY t.path, t.entry_id"
    ))
    .bind(root.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TrashRow::into_record).collect()
}

/// 读一条回收站清单行；不存在或越权返回 `None`。
pub async fn trash_entry(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
    entry: EntryId,
) -> Result<Option<TrashEntry>, TrashError> {
    let row = sqlx::query_as::<_, TrashRow>(&format!(
        "SELECT {TRASH_COLUMNS} FROM trash_entries t JOIN roots r ON r.id = t.root_id \
         WHERE t.root_id = ?1 AND t.entry_id = ?2 AND r.owner_id = ?3"
    ))
    .bind(root.as_bytes().as_slice())
    .bind(entry.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(conn)
    .await?;
    row.map(TrashRow::into_record).transpose()
}

/// 读一个 Root 的墓碑，按删除时间（再按身份）排序。
pub async fn tombstones(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
) -> Result<Vec<Tombstone>, TrashError> {
    require_root_owner(conn, owner, root).await?;
    let rows = sqlx::query_as::<_, TombstoneRow>(&format!(
        "SELECT {TOMBSTONE_COLUMNS} FROM tombstones t JOIN roots r ON r.id = t.root_id \
         WHERE t.root_id = ?1 AND r.owner_id = ?2 ORDER BY t.deleted_at, t.entry_id"
    ))
    .bind(root.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TombstoneRow::into_record).collect()
}

/// 读一条墓碑；不存在或越权返回 `None`。
pub async fn tombstone(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
    entry: EntryId,
) -> Result<Option<Tombstone>, TrashError> {
    let row = sqlx::query_as::<_, TombstoneRow>(&format!(
        "SELECT {TOMBSTONE_COLUMNS} FROM tombstones t JOIN roots r ON r.id = t.root_id \
         WHERE t.root_id = ?1 AND t.entry_id = ?2 AND r.owner_id = ?3"
    ))
    .bind(root.as_bytes().as_slice())
    .bind(entry.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(conn)
    .await?;
    row.map(TombstoneRow::into_record).transpose()
}

async fn require_root_owner(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
) -> Result<(), TrashError> {
    let found: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM roots WHERE id = ?1 AND owner_id = ?2")
            .bind(root.as_bytes().as_slice())
            .bind(owner.as_bytes().as_slice())
            .fetch_optional(conn)
            .await?;
    found.map_or(Err(TrashError::OutOfScope), |_| Ok(()))
}

async fn require_entry_in_root(
    conn: &mut SqliteConnection,
    root: RootId,
    entry: EntryId,
) -> Result<(), TrashError> {
    let found: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM entries WHERE root_id = ?1 AND id = ?2")
            .bind(root.as_bytes().as_slice())
            .bind(entry.as_bytes().as_slice())
            .fetch_optional(conn)
            .await?;
    found.map_or(Err(TrashError::EntryNotInRoot { entry }), |_| Ok(()))
}

/// Root 内相对路径：非空、不以 `/` 起始、无空分量与 `.`/`..` 分量（与意图日志同一规则）。
fn validate_path(path: &str) -> Result<(), TrashError> {
    let invalid = |reason: &'static str| TrashError::InvalidPath {
        path: path.to_owned(),
        reason,
    };
    if path.is_empty() {
        return Err(invalid("不能为空"));
    }
    if path.contains('\0') {
        return Err(invalid("不能含 NUL"));
    }
    if path.starts_with('/') {
        return Err(invalid("必须是 Root 内相对路径"));
    }
    for component in path.split('/') {
        if component.is_empty() {
            return Err(invalid("不能含空路径分量"));
        }
        if component == "." || component == ".." {
            return Err(invalid("不能含 . 或 .. 分量"));
        }
    }
    Ok(())
}

/// 唯一约束失败是否来自指定表（把“重复”与别的约束失败分开）。
fn is_unique_violation(error: &sqlx::Error, table: &str) -> bool {
    match error {
        sqlx::Error::Database(database) => {
            let message = database.message();
            message.contains("UNIQUE constraint failed") && message.contains(table)
        }
        _ => false,
    }
}

fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    build: impl FnOnce([u8; ID_LEN]) -> T,
) -> Result<T, TrashError> {
    let length = bytes.len();
    <[u8; ID_LEN]>::try_from(bytes)
        .map(build)
        .map_err(|_| TrashError::MalformedRow {
            detail: format!("列 {column} 的身份长度是 {length}，应为 {ID_LEN}"),
        })
}

fn kind_of(text: &str, column: &'static str) -> Result<EntryKind, TrashError> {
    EntryKind::from_text(text).ok_or_else(|| TrashError::MalformedRow {
        detail: format!("列 {column} 的值 {text:?} 不是条目种类"),
    })
}

fn revision_of(value: i64) -> Result<Revision, TrashError> {
    Revision::from_db_integer(value).map_err(|error| TrashError::MalformedRow {
        detail: format!("revision 无法还原：{error}"),
    })
}

fn timestamp_of(value: i64, column: &'static str) -> Result<UtcTimestamp, TrashError> {
    UtcTimestamp::from_db_integer(value).map_err(|error| TrashError::MalformedRow {
        detail: format!("列 {column} 的时间无法还原：{error}"),
    })
}

#[derive(sqlx::FromRow)]
struct TrashRow {
    entry_id: Vec<u8>,
    root_id: Vec<u8>,
    parent_id: Vec<u8>,
    path: String,
    kind: String,
    revision: i64,
    content_version_id: Option<Vec<u8>>,
    deleted_at: i64,
    expires_at: i64,
}

impl TrashRow {
    fn into_record(self) -> Result<TrashEntry, TrashError> {
        Ok(TrashEntry {
            entry: identity(self.entry_id, "entry_id", EntryId::from_bytes)?,
            root: identity(self.root_id, "root_id", RootId::from_bytes)?,
            parent: identity(self.parent_id, "parent_id", EntryId::from_bytes)?,
            path: self.path,
            kind: kind_of(&self.kind, "kind")?,
            revision: revision_of(self.revision)?,
            content_version: match self.content_version_id {
                Some(bytes) => Some(identity(
                    bytes,
                    "content_version_id",
                    VersionId::from_bytes,
                )?),
                None => None,
            },
            deleted_at: timestamp_of(self.deleted_at, "deleted_at")?,
            expires_at: timestamp_of(self.expires_at, "expires_at")?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct TombstoneRow {
    root_id: Vec<u8>,
    entry_id: Vec<u8>,
    path: String,
    kind: String,
    deleted_at: i64,
    expires_at: i64,
}

impl TombstoneRow {
    fn into_record(self) -> Result<Tombstone, TrashError> {
        Ok(Tombstone {
            root: identity(self.root_id, "root_id", RootId::from_bytes)?,
            entry: identity(self.entry_id, "entry_id", EntryId::from_bytes)?,
            path: self.path,
            kind: kind_of(&self.kind, "kind")?,
            deleted_at: timestamp_of(self.deleted_at, "deleted_at")?,
            expires_at: timestamp_of(self.expires_at, "expires_at")?,
        })
    }
}

/// 删除一条回收站清单行（恢复后调用：条目不再在回收站里）。
///
/// 返回是否真的删掉了一行；清单行不存在不算错误（恢复可重入）。
///
/// # Errors
///
/// 存储失败。
pub async fn remove_trash_entry(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    root: RootId,
    entry: EntryId,
) -> Result<bool, TrashError> {
    require_root_owner(tx, owner, root).await?;
    let removed = sqlx::query("DELETE FROM trash_entries WHERE root_id = ?1 AND entry_id = ?2")
        .bind(root.as_bytes().as_slice())
        .bind(entry.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(TrashError::Db)?;
    Ok(removed.rows_affected() > 0)
}

/// 删除一个条目的墓碑（恢复后调用：条目重新活跃，之后再次删除要能写新墓碑）。
///
/// # Errors
///
/// 存储失败。
pub async fn remove_tombstone(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    root: RootId,
    entry: EntryId,
) -> Result<bool, TrashError> {
    require_root_owner(tx, owner, root).await?;
    let removed = sqlx::query("DELETE FROM tombstones WHERE root_id = ?1 AND entry_id = ?2")
        .bind(root.as_bytes().as_slice())
        .bind(entry.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(TrashError::Db)?;
    Ok(removed.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use vaultsyncd_domain::ids::VersionId;

    use crate::changes;
    use crate::entries::{self, NewEntry};
    use crate::test_support::{
        TestDir, at, content_hash, entry, open_database, register_root, seed_file, seed_object,
        seed_owner, tx as tx_id_marker,
    };
    use crate::versions::{self, NewVersion, VersionOrigin};

    const DAY: i64 = 24 * 60 * 60;
    const RETENTION: i64 = 30 * DAY;

    struct Fixture {
        _dir: TestDir,
        database: crate::db::Database,
        owner: UserId,
        root: RootId,
        root_entry: EntryId,
    }

    async fn fixture() -> Fixture {
        let dir = TestDir::new("s039-trash");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x01).await;
        let (root, root_entry) = register_root(&database, owner, 0x21).await;
        Fixture {
            _dir: dir,
            database,
            owner,
            root,
            root_entry,
        }
    }

    /// 在 Root 根条目下放一个文件，并给它记录一个带内容对象的版本。
    async fn seed_versioned_file(
        fixture: &Fixture,
        marker: u8,
        name: &str,
        object_marker: u8,
    ) -> EntryId {
        let id = seed_file(
            &fixture.database,
            fixture.owner,
            fixture.root,
            fixture.root_entry,
            marker,
            name,
        )
        .await;
        let hash = seed_object(
            &fixture.database,
            fixture.owner,
            fixture.root,
            object_marker,
            5,
        )
        .await;
        let mut tx = fixture.database.begin_write().await.expect("开始写事务");
        let batch = changes::append_change(
            &mut tx,
            fixture.owner,
            fixture.root,
            tx_id_marker(0x71),
            r#"[{"kind":"entry_written"}]"#,
            at(1_700_000_100),
        )
        .await
        .expect("追加事件批次");
        versions::record_version(
            &mut tx,
            fixture.owner,
            &NewVersion {
                id: VersionId::from_bytes([marker; ID_LEN]),
                root: fixture.root,
                entry: id,
                object_hash: hash,
                size: 5,
                created_seq: batch.seq,
                origin: VersionOrigin::Client,
                at: at(1_700_000_100),
            },
        )
        .await
        .expect("记录版本");
        tx.commit().await.expect("提交版本");
        id
    }

    fn trash_of(
        root: RootId,
        entry: EntryId,
        parent: EntryId,
        path: &str,
        kind: EntryKind,
    ) -> NewTrashEntry {
        NewTrashEntry {
            entry,
            root,
            parent,
            path: path.to_owned(),
            kind,
            revision: Revision::new(1).expect("修订号"),
            content_version: None,
            deleted_at: at(1_700_000_200),
            expires_at: at(1_700_000_200 + RETENTION),
        }
    }

    #[tokio::test]
    async fn soft_delete_releases_the_name_and_keeps_the_row() {
        let fixture = fixture().await;
        let id = seed_file(
            &fixture.database,
            fixture.owner,
            fixture.root,
            fixture.root_entry,
            0x31,
            "notes.txt",
        )
        .await;

        let mut tx = fixture.database.begin_write().await.expect("开始写事务");
        let deleted = soft_delete_batch(
            &mut tx,
            fixture.owner,
            fixture.root,
            &[SoftDelete {
                entry: id,
                revision: Revision::new(1).expect("修订号"),
            }],
            at(1_700_000_200),
        )
        .await
        .expect("软删除");
        assert_eq!(deleted, 1);
        tx.commit().await.expect("提交软删除");

        let mut conn = fixture.database.pool().acquire().await.expect("取连接");
        let record = entries::entry_in_scope(&mut conn, fixture.owner, id)
            .await
            .expect("读条目")
            .expect("条目还在（软删除不硬删行）");
        assert_eq!(record.deleted_at, Some(at(1_700_000_200)));
        let children =
            entries::children(&mut conn, fixture.owner, fixture.root, fixture.root_entry)
                .await
                .expect("读子项");
        assert!(children.is_empty(), "活跃子项里不该再有它");

        // 同父同名的活跃名称已被释放：新条目可以用同一个名字。
        let mut tx = fixture.database.begin_write().await.expect("开始写事务");
        entries::insert_entry(
            &mut tx,
            fixture.owner,
            &NewEntry {
                id: entry(0x32),
                root: fixture.root,
                parent: fixture.root_entry,
                name: "notes.txt".to_owned(),
                name_key: "notes.txt".to_owned(),
                kind: EntryKind::File,
                revision: Revision::new(1).expect("修订号"),
                content_version: None,
            },
            at(1_700_000_200),
        )
        .await
        .expect("同名新条目可以落在同一位置");
        tx.commit().await.expect("提交新条目");
    }

    #[tokio::test]
    async fn soft_delete_batch_compares_the_revision() {
        let fixture = fixture().await;
        let id = seed_file(
            &fixture.database,
            fixture.owner,
            fixture.root,
            fixture.root_entry,
            0x31,
            "notes.txt",
        )
        .await;

        let mut tx = fixture.database.begin_write().await.expect("开始写事务");
        let failure = soft_delete_batch(
            &mut tx,
            fixture.owner,
            fixture.root,
            &[SoftDelete {
                entry: id,
                revision: Revision::new(7).expect("修订号"),
            }],
            at(1_700_000_200),
        )
        .await
        .expect_err("revision 不符必须失败");
        assert!(matches!(failure, TrashError::BatchMismatch { .. }));
        tx.rollback().await.expect("回滚");

        let mut conn = fixture.database.pool().acquire().await.expect("取连接");
        assert!(
            entries::entry_in_scope(&mut conn, fixture.owner, id)
                .await
                .expect("读条目")
                .expect("条目还在")
                .deleted_at
                .is_none()
        );
    }

    #[tokio::test]
    async fn soft_delete_batch_is_bounded() {
        let fixture = fixture().await;
        let mut tx = fixture.database.begin_write().await.expect("开始写事务");
        assert!(matches!(
            soft_delete_batch(&mut tx, fixture.owner, fixture.root, &[], at(1_700_000_200)).await,
            Err(TrashError::InvalidBatch { entries: 0 })
        ));
        let oversized = vec![
            SoftDelete {
                entry: entry(0x31),
                revision: Revision::new(0).expect("修订号"),
            };
            MAX_SOFT_DELETE_BATCH + 1
        ];
        assert!(matches!(
            soft_delete_batch(
                &mut tx,
                fixture.owner,
                fixture.root,
                &oversized,
                at(1_700_000_200)
            )
            .await,
            Err(TrashError::InvalidBatch { .. })
        ));
        tx.rollback().await.expect("回滚");
    }

    #[tokio::test]
    async fn trash_entry_keeps_the_original_location_and_version() {
        let fixture = fixture().await;
        let id = seed_versioned_file(&fixture, 0x31, "notes.txt", 0x41).await;

        let mut tx = fixture.database.begin_write().await.expect("开始写事务");
        let mut new = trash_of(
            fixture.root,
            id,
            fixture.root_entry,
            "notes.txt",
            EntryKind::File,
        );
        new.content_version = Some(VersionId::from_bytes([0x31; ID_LEN]));
        let recorded = record_trash_entry(&mut tx, fixture.owner, &new)
            .await
            .expect("写回收站清单");
        tx.commit().await.expect("提交");

        assert_eq!(recorded.path, "notes.txt");
        assert_eq!(recorded.parent, fixture.root_entry);
        assert_eq!(recorded.revision, Revision::new(1).expect("修订号"));
        assert_eq!(
            recorded.content_version,
            Some(VersionId::from_bytes([0x31; ID_LEN]))
        );
        assert_eq!(recorded.deleted_at, at(1_700_000_200));
        assert_eq!(recorded.expires_at, at(1_700_000_200 + RETENTION));

        // 同一身份不能进回收站两次。
        let mut tx = fixture.database.begin_write().await.expect("开始写事务");
        assert!(matches!(
            record_trash_entry(&mut tx, fixture.owner, &new).await,
            Err(TrashError::AlreadyInTrash { .. })
        ));
        tx.rollback().await.expect("回滚");
    }

    #[tokio::test]
    async fn trash_entry_rejects_bad_input() {
        let fixture = fixture().await;
        let id = seed_file(
            &fixture.database,
            fixture.owner,
            fixture.root,
            fixture.root_entry,
            0x31,
            "notes.txt",
        )
        .await;

        let mut tx = fixture.database.begin_write().await.expect("开始写事务");
        let mut absolute = trash_of(
            fixture.root,
            id,
            fixture.root_entry,
            "/etc/passwd",
            EntryKind::File,
        );
        assert!(matches!(
            record_trash_entry(&mut tx, fixture.owner, &absolute).await,
            Err(TrashError::InvalidPath { .. })
        ));
        absolute.path = "notes.txt".to_owned();
        let mut directory_with_content = trash_of(
            fixture.root,
            id,
            fixture.root_entry,
            "notes.txt",
            EntryKind::Directory,
        );
        directory_with_content.content_version = Some(VersionId::from_bytes([0x31; ID_LEN]));
        assert!(matches!(
            record_trash_entry(&mut tx, fixture.owner, &directory_with_content).await,
            Err(TrashError::DirectoryWithContent { .. })
        ));
        let mut backwards = trash_of(
            fixture.root,
            id,
            fixture.root_entry,
            "notes.txt",
            EntryKind::File,
        );
        backwards.expires_at = at(1);
        assert!(matches!(
            record_trash_entry(&mut tx, fixture.owner, &backwards).await,
            Err(TrashError::InvalidRetention { .. })
        ));
        tx.rollback().await.expect("回滚");

        // 越权的 Root 不泄露“存在与否”。
        let mut tx = fixture.database.begin_write().await.expect("开始写事务");
        let intruder = UserId::from_bytes([0x02; ID_LEN]);
        assert!(matches!(
            record_trash_entry(
                &mut tx,
                intruder,
                &trash_of(
                    fixture.root,
                    id,
                    fixture.root_entry,
                    "notes.txt",
                    EntryKind::File
                )
            )
            .await,
            Err(TrashError::OutOfScope)
        ));
        tx.rollback().await.expect("回滚");
    }

    #[tokio::test]
    async fn tombstones_are_written_once_per_entry() {
        let fixture = fixture().await;
        let id = seed_file(
            &fixture.database,
            fixture.owner,
            fixture.root,
            fixture.root_entry,
            0x31,
            "notes.txt",
        )
        .await;

        let mut tx = fixture.database.begin_write().await.expect("开始写事务");
        let written = insert_tombstone(
            &mut tx,
            fixture.owner,
            &trash_of(
                fixture.root,
                id,
                fixture.root_entry,
                "notes.txt",
                EntryKind::File,
            ),
        )
        .await
        .expect("写墓碑");
        tx.commit().await.expect("提交");
        assert_eq!(written.entry, id);
        assert_eq!(written.path, "notes.txt");
        assert_eq!(written.deleted_at, at(1_700_000_200));
        assert_eq!(written.expires_at, at(1_700_000_200 + RETENTION));

        let mut tx = fixture.database.begin_write().await.expect("开始写事务");
        assert!(matches!(
            insert_tombstone(
                &mut tx,
                fixture.owner,
                &trash_of(
                    fixture.root,
                    id,
                    fixture.root_entry,
                    "notes.txt",
                    EntryKind::File
                )
            )
            .await,
            Err(TrashError::DuplicateTombstone { .. })
        ));
        tx.rollback().await.expect("回滚");
    }

    #[tokio::test]
    async fn delete_keeps_history_and_content_readable() {
        let fixture = fixture().await;
        let old = seed_versioned_file(&fixture, 0x31, "notes.txt", 0x41).await;

        // 删除：软删除 + 回收站清单 + 墓碑，同一事务。
        let mut tx = fixture.database.begin_write().await.expect("开始写事务");
        soft_delete_batch(
            &mut tx,
            fixture.owner,
            fixture.root,
            &[SoftDelete {
                entry: old,
                revision: Revision::new(1).expect("修订号"),
            }],
            at(1_700_000_200),
        )
        .await
        .expect("软删除");
        let mut new = trash_of(
            fixture.root,
            old,
            fixture.root_entry,
            "notes.txt",
            EntryKind::File,
        );
        new.content_version = Some(VersionId::from_bytes([0x31; ID_LEN]));
        record_trash_entry(&mut tx, fixture.owner, &new)
            .await
            .expect("回收站清单");
        insert_tombstone(&mut tx, fixture.owner, &new)
            .await
            .expect("墓碑");
        tx.commit().await.expect("提交删除事务");

        // 同路径的新内容（新条目、新版本）：删除不该碰到它，旧历史也要照旧可读。
        let mut conn = fixture.database.pool().acquire().await.expect("取连接");
        let history = versions::version_history(&mut conn, fixture.owner, fixture.root, old, 10)
            .await
            .expect("旧条目的历史仍可读");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].object_hash, content_hash(0x41));
        assert!(history[0].superseded_at.is_none(), "当前版本不因删除而失效");
        let object =
            versions::object_in_scope(&mut conn, fixture.owner, fixture.root, &content_hash(0x41))
                .await
                .expect("读内容对象")
                .expect("内容对象仍登记");
        assert_eq!(object.size, 5, "删除不释放内容对象");
        drop(conn);

        let fresh = seed_versioned_file(&fixture, 0x32, "notes.txt", 0x42).await;
        assert_ne!(fresh, old);
        let mut conn = fixture.database.pool().acquire().await.expect("取连接");
        let history = versions::version_history(&mut conn, fixture.owner, fixture.root, old, 10)
            .await
            .expect("旧历史仍可读");
        assert_eq!(history[0].object_hash, content_hash(0x41), "旧版本哈希不变");
        let history = versions::version_history(&mut conn, fixture.owner, fixture.root, fresh, 10)
            .await
            .expect("新条目历史可读");
        assert_eq!(history[0].object_hash, content_hash(0x42));
        let trashed = trash_entries(&mut conn, fixture.owner, fixture.root)
            .await
            .expect("读回收站");
        assert_eq!(trashed.len(), 1);
        assert_eq!(trashed[0].entry, old);
        assert_eq!(trashed[0].path, "notes.txt");
        let stones = tombstones(&mut conn, fixture.owner, fixture.root)
            .await
            .expect("读墓碑");
        assert_eq!(stones.len(), 1);
        assert_eq!(stones[0].entry, old);
        assert!(
            trash_entry(&mut conn, fixture.owner, fixture.root, fresh)
                .await
                .expect("读清单")
                .is_none(),
            "新条目不在回收站里"
        );
        assert!(
            tombstone(&mut conn, fixture.owner, fixture.root, old)
                .await
                .expect("读墓碑")
                .is_some()
        );
    }
}
