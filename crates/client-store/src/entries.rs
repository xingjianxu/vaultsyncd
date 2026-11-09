//! 客户端条目状态：已确认基线、已收到的远端状态与本地观察
//! （`docs/design.md` §7.3、§8.1、§8.4）。
//!
//! 边界：本模块只读写三张表的行，不规划、不传输、不碰文件系统。判定“本地是否真的变过”需要
//! 平台指纹与实际内容（design §8.4），本模块只保存调用方给出的事实。
//!
//! 不变量：
//!
//! - **三张表互相独立**：`baseline_entries`、`remote_entries`、`local_entries` 之间没有外键。
//!   同一 `(binding_id, entry_id)` 必须能同时保存旧基线、更新的远端状态和本地 dirty——这是
//!   客户端状态的核心组合（design §8.1 的“已下载 + 本地修改 + 远端更新”），因此任何一张表的
//!   写入或删除都不能约束另外两张。
//! - **远端行不需要基线**：首次同步（I07）里远端条目的基线还不存在；`local_entries` 同样允许
//!   先有本地行（新建的本地文件在提交前没有服务器 Identity）。
//! - **`dirty = clean` 不是“文件未变”的证明**：它只是上次观察的结论（design §8.4）。本模块
//!   只存标记，不做任何基于它的破坏性判断。
//! - **`generation` 只增不减地由调用方给出**：上传确认要用 generation 判断响应是否仍对应当前
//!   本地内容；本模块不生成 generation，也不在写入时改写它。
//! - **位置用身份表达**：父条目 + 名称，不用本地路径文本拼接；根条目的合成条目没有父项与名称
//!   （design §7.1）。
//! - **名称存原文**：名称的跨平台可表示性由 fs-common 的规则与同步流程判定；本层不归一化、
//!   不改写服务器给出的名称，避免把远端已接受的名称悄悄变成另一个名字。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{BindingId, EntryId, ID_LEN, VersionId};
use vaultsyncd_domain::state::{Dirty, Residency};
use vaultsyncd_domain::version::{Generation, Revision};

/// 条目类型：文件或目录（与 `entries.kind` 的取值一致）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// 普通文件，可以有内容版本。
    File,
    /// 目录，没有内容版本。
    Directory,
}

impl EntryKind {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜默认值。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "file" => Some(Self::File),
            "directory" => Some(Self::Directory),
            _ => None,
        }
    }
}

/// 最近已确认的共同基线（`baseline_entries` 一行，design §7.3）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaselineEntry {
    /// 条目身份。
    pub entry: EntryId,
    /// 父条目身份；根条目的合成条目为 `None`。
    pub parent: Option<EntryId>,
    /// 名称；根条目为空串。
    pub name: String,
    /// 文件或目录。
    pub kind: EntryKind,
    /// 服务器分配的修订号。
    pub revision: Revision,
    /// 文件内容版本；目录为 `None`。
    pub content_version: Option<VersionId>,
    /// 服务器已确认的删除标记（不是本地推断）。
    pub deleted: bool,
}

/// 最新收到但可能尚未落盘的远端状态（`remote_entries` 一行，design §7.3）。
///
/// 字段与 [`BaselineEntry`] 相同，但两者是**不同的行**：远端更新先写这里，基线只在本地发布
/// 完成后才推进（design §8.4）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry {
    /// 条目身份。
    pub entry: EntryId,
    /// 父条目身份；根条目为 `None`。
    pub parent: Option<EntryId>,
    /// 名称；根条目为空串。
    pub name: String,
    /// 文件或目录。
    pub kind: EntryKind,
    /// 服务器分配的修订号。
    pub revision: Revision,
    /// 文件内容版本；目录为 `None`。
    pub content_version: Option<VersionId>,
    /// 远端删除标记。
    pub deleted: bool,
}

/// 本地观察与本地状态（`local_entries` 一行，design §7.3、§8.1）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalEntry {
    /// 条目身份。
    pub entry: EntryId,
    /// 本地内容的 generation，由调用方在每次观察到修改时推进。
    pub generation: Generation,
    /// 本地变更维度（`clean`/`dirty`/`snapshotting`/`awaiting_commit`）。
    pub dirty: Dirty,
    /// 驻留维度（`metadata_only`/`partial`/`materialized`）。
    pub residency: Residency,
    /// 平台编码的当前指纹；`None` 表示尚未观察，不能当成“与基线一致”。
    pub fingerprint: Option<Vec<u8>>,
    /// 平台 file identity；`None` 表示尚未记录。
    pub file_identity: Option<Vec<u8>>,
}

/// 条目状态读写的失败。
#[derive(Debug)]
pub enum EntryError {
    /// SQLite 层面失败（含外键与 `CHECK` 拒绝）。
    Db(sqlx::Error),
    /// 根条目的形状不对：有父项，或（无父项时）有名称、不是目录。
    VaultEntryShape {
        /// 违反的具体规则。
        reason: &'static str,
    },
    /// 指纹或文件身份给了空字节；空字节不是“未知”，未知用 `None`。
    EmptyBytes {
        /// 出问题的字段名。
        field: &'static str,
    },
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for EntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::VaultEntryShape { reason } => write!(f, "根条目形状不合法：{reason}"),
            Self::EmptyBytes { field } => write!(f, "字段 {field} 是空字节"),
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
        }
    }
}

impl std::error::Error for EntryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::VaultEntryShape { .. } | Self::EmptyBytes { .. } | Self::MalformedRow { .. } => {
                None
            }
        }
    }
}

/// `baseline_entries`/`remote_entries` 共用的列顺序。
const ENTRY_COLUMNS: &str =
    "entry_id, parent_entry_id, name, kind, revision, content_version_id, deleted";

/// `local_entries` 的列顺序。
const LOCAL_COLUMNS: &str = "entry_id, generation, dirty, residency, fingerprint, file_identity";

/// 写入（或覆盖）一条基线；同一 `(binding_id, entry_id)` 只保留最新一行。
pub async fn put_baseline(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    entry: &BaselineEntry,
) -> Result<(), EntryError> {
    validate_location(entry.parent, &entry.name, entry.kind)?;
    sqlx::query(
        "INSERT INTO baseline_entries \
         (binding_id, entry_id, parent_entry_id, name, kind, revision, content_version_id, deleted) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (binding_id, entry_id) DO UPDATE SET \
           parent_entry_id = excluded.parent_entry_id, \
           name = excluded.name, \
           kind = excluded.kind, \
           revision = excluded.revision, \
           content_version_id = excluded.content_version_id, \
           deleted = excluded.deleted",
    )
    .bind(binding.as_bytes().as_slice())
    .bind(entry.entry.as_bytes().as_slice())
    .bind(parent_bytes(entry.parent))
    .bind(&entry.name)
    .bind(entry.kind.as_text())
    .bind(entry.revision.to_db_integer())
    .bind(version_bytes(entry.content_version))
    .bind(i64::from(entry.deleted))
    .execute(&mut **tx)
    .await
    .map_err(EntryError::Db)?;
    Ok(())
}

/// 读一条基线；不存在返回 `None`。
pub async fn baseline_entry(
    conn: &mut SqliteConnection,
    binding: BindingId,
    entry: EntryId,
) -> Result<Option<BaselineEntry>, EntryError> {
    let row = sqlx::query_as::<_, EntryRow>(&format!(
        "SELECT {ENTRY_COLUMNS} FROM baseline_entries WHERE binding_id = ?1 AND entry_id = ?2"
    ))
    .bind(binding.as_bytes().as_slice())
    .bind(entry.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(EntryError::Db)?;
    row.map(|row| row.into_baseline()).transpose()
}

/// 删除一条基线；返回是否真的删掉了行。
pub async fn remove_baseline(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    entry: EntryId,
) -> Result<bool, EntryError> {
    let deleted =
        sqlx::query("DELETE FROM baseline_entries WHERE binding_id = ?1 AND entry_id = ?2")
            .bind(binding.as_bytes().as_slice())
            .bind(entry.as_bytes().as_slice())
            .execute(&mut **tx)
            .await
            .map_err(EntryError::Db)?;
    Ok(deleted.rows_affected() > 0)
}

/// 写入（或覆盖）一条远端状态；不与基线、本地状态互相影响。
pub async fn put_remote(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    entry: &RemoteEntry,
) -> Result<(), EntryError> {
    validate_location(entry.parent, &entry.name, entry.kind)?;
    sqlx::query(
        "INSERT INTO remote_entries \
         (binding_id, entry_id, parent_entry_id, name, kind, revision, content_version_id, deleted) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (binding_id, entry_id) DO UPDATE SET \
           parent_entry_id = excluded.parent_entry_id, \
           name = excluded.name, \
           kind = excluded.kind, \
           revision = excluded.revision, \
           content_version_id = excluded.content_version_id, \
           deleted = excluded.deleted",
    )
    .bind(binding.as_bytes().as_slice())
    .bind(entry.entry.as_bytes().as_slice())
    .bind(parent_bytes(entry.parent))
    .bind(&entry.name)
    .bind(entry.kind.as_text())
    .bind(entry.revision.to_db_integer())
    .bind(version_bytes(entry.content_version))
    .bind(i64::from(entry.deleted))
    .execute(&mut **tx)
    .await
    .map_err(EntryError::Db)?;
    Ok(())
}

/// 读一条远端状态；不存在返回 `None`。
pub async fn remote_entry(
    conn: &mut SqliteConnection,
    binding: BindingId,
    entry: EntryId,
) -> Result<Option<RemoteEntry>, EntryError> {
    let row = sqlx::query_as::<_, EntryRow>(&format!(
        "SELECT {ENTRY_COLUMNS} FROM remote_entries WHERE binding_id = ?1 AND entry_id = ?2"
    ))
    .bind(binding.as_bytes().as_slice())
    .bind(entry.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(EntryError::Db)?;
    row.map(|row| row.into_remote()).transpose()
}

/// 删除一条远端状态；返回是否真的删掉了行。
pub async fn remove_remote(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    entry: EntryId,
) -> Result<bool, EntryError> {
    let deleted = sqlx::query("DELETE FROM remote_entries WHERE binding_id = ?1 AND entry_id = ?2")
        .bind(binding.as_bytes().as_slice())
        .bind(entry.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(EntryError::Db)?;
    Ok(deleted.rows_affected() > 0)
}

/// 写入（或覆盖）本地状态。`generation` 与 `dirty` 由调用方给出，本函数不改写。
pub async fn put_local(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    entry: &LocalEntry,
) -> Result<(), EntryError> {
    reject_empty("fingerprint", entry.fingerprint.as_deref())?;
    reject_empty("file_identity", entry.file_identity.as_deref())?;
    sqlx::query(
        "INSERT INTO local_entries \
         (binding_id, entry_id, generation, dirty, residency, fingerprint, file_identity) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT (binding_id, entry_id) DO UPDATE SET \
           generation = excluded.generation, \
           dirty = excluded.dirty, \
           residency = excluded.residency, \
           fingerprint = excluded.fingerprint, \
           file_identity = excluded.file_identity",
    )
    .bind(binding.as_bytes().as_slice())
    .bind(entry.entry.as_bytes().as_slice())
    .bind(entry.generation.to_db_integer())
    .bind(dirty_text(entry.dirty))
    .bind(residency_text(entry.residency))
    .bind(entry.fingerprint.as_deref())
    .bind(entry.file_identity.as_deref())
    .execute(&mut **tx)
    .await
    .map_err(EntryError::Db)?;
    Ok(())
}

/// 读一条本地状态；不存在返回 `None`。
pub async fn local_entry(
    conn: &mut SqliteConnection,
    binding: BindingId,
    entry: EntryId,
) -> Result<Option<LocalEntry>, EntryError> {
    let row = sqlx::query_as::<_, LocalRow>(&format!(
        "SELECT {LOCAL_COLUMNS} FROM local_entries WHERE binding_id = ?1 AND entry_id = ?2"
    ))
    .bind(binding.as_bytes().as_slice())
    .bind(entry.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(EntryError::Db)?;
    row.map(LocalRow::into_record).transpose()
}

/// 删除一条本地状态；返回是否真的删掉了行。
pub async fn remove_local(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    entry: EntryId,
) -> Result<bool, EntryError> {
    let deleted = sqlx::query("DELETE FROM local_entries WHERE binding_id = ?1 AND entry_id = ?2")
        .bind(binding.as_bytes().as_slice())
        .bind(entry.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(EntryError::Db)?;
    Ok(deleted.rows_affected() > 0)
}

/// 数据库行到 [`BaselineEntry`] 或 [`RemoteEntry`] 的中间形态。
#[derive(sqlx::FromRow)]
struct EntryRow {
    entry_id: Vec<u8>,
    parent_entry_id: Option<Vec<u8>>,
    name: String,
    kind: String,
    revision: i64,
    content_version_id: Option<Vec<u8>>,
    deleted: i64,
}

impl EntryRow {
    fn into_baseline(self) -> Result<BaselineEntry, EntryError> {
        let (entry, parent, name, kind, revision, content_version, deleted) = self.unpack()?;
        Ok(BaselineEntry {
            entry,
            parent,
            name,
            kind,
            revision,
            content_version,
            deleted,
        })
    }

    fn into_remote(self) -> Result<RemoteEntry, EntryError> {
        let (entry, parent, name, kind, revision, content_version, deleted) = self.unpack()?;
        Ok(RemoteEntry {
            entry,
            parent,
            name,
            kind,
            revision,
            content_version,
            deleted,
        })
    }

    #[allow(clippy::type_complexity)]
    fn unpack(
        self,
    ) -> Result<
        (
            EntryId,
            Option<EntryId>,
            String,
            EntryKind,
            Revision,
            Option<VersionId>,
            bool,
        ),
        EntryError,
    > {
        Ok((
            identity(self.entry_id, "entry_id", EntryId::from_bytes)?,
            self.parent_entry_id
                .map(|bytes| identity(bytes, "parent_entry_id", EntryId::from_bytes))
                .transpose()?,
            self.name,
            EntryKind::from_text(&self.kind).ok_or(EntryError::MalformedRow { column: "kind" })?,
            Revision::from_db_integer(self.revision)
                .map_err(|_| EntryError::MalformedRow { column: "revision" })?,
            self.content_version_id
                .map(|bytes| identity(bytes, "content_version_id", VersionId::from_bytes))
                .transpose()?,
            match self.deleted {
                0 => false,
                1 => true,
                _ => return Err(EntryError::MalformedRow { column: "deleted" }),
            },
        ))
    }
}

/// 数据库行到 [`LocalEntry`] 的中间形态。
#[derive(sqlx::FromRow)]
struct LocalRow {
    entry_id: Vec<u8>,
    generation: i64,
    dirty: String,
    residency: String,
    fingerprint: Option<Vec<u8>>,
    file_identity: Option<Vec<u8>>,
}

impl LocalRow {
    fn into_record(self) -> Result<LocalEntry, EntryError> {
        Ok(LocalEntry {
            entry: identity(self.entry_id, "entry_id", EntryId::from_bytes)?,
            generation: Generation::from_db_integer(self.generation).map_err(|_| {
                EntryError::MalformedRow {
                    column: "generation",
                }
            })?,
            dirty: dirty_from_text(&self.dirty)
                .ok_or(EntryError::MalformedRow { column: "dirty" })?,
            residency: residency_from_text(&self.residency).ok_or(EntryError::MalformedRow {
                column: "residency",
            })?,
            fingerprint: self.fingerprint,
            file_identity: self.file_identity,
        })
    }
}

/// 根条目形状检查（与迁移里的 `CHECK` 一致，先给出更精确的错误）。
fn validate_location(
    parent: Option<EntryId>,
    name: &str,
    kind: EntryKind,
) -> Result<(), EntryError> {
    if parent.is_some() {
        return Ok(());
    }
    if !name.is_empty() {
        return Err(EntryError::VaultEntryShape {
            reason: "根条目没有文件名",
        });
    }
    if kind != EntryKind::Directory {
        return Err(EntryError::VaultEntryShape {
            reason: "根条目只能是目录",
        });
    }
    Ok(())
}

/// 空字节不是“未知”：未知用 `None` 表示，空 `BLOB` 会被拒绝。
fn reject_empty(field: &'static str, bytes: Option<&[u8]>) -> Result<(), EntryError> {
    match bytes {
        Some([]) => Err(EntryError::EmptyBytes { field }),
        _ => Ok(()),
    }
}

fn parent_bytes(parent: Option<EntryId>) -> Option<Vec<u8>> {
    parent.map(|entry| entry.as_bytes().to_vec())
}

fn version_bytes(version: Option<VersionId>) -> Option<Vec<u8>> {
    version.map(|version| version.as_bytes().to_vec())
}

/// `Dirty` 的数据库文本（与 design §8.1 的拼写一致）。
const fn dirty_text(dirty: Dirty) -> &'static str {
    match dirty {
        Dirty::Clean => "clean",
        Dirty::Dirty => "dirty",
        Dirty::Snapshotting => "snapshotting",
        Dirty::AwaitingCommit => "awaiting_commit",
    }
}

fn dirty_from_text(text: &str) -> Option<Dirty> {
    match text {
        "clean" => Some(Dirty::Clean),
        "dirty" => Some(Dirty::Dirty),
        "snapshotting" => Some(Dirty::Snapshotting),
        "awaiting_commit" => Some(Dirty::AwaitingCommit),
        _ => None,
    }
}

/// `Residency` 的数据库文本（与 design §8.1 的拼写一致）。
const fn residency_text(residency: Residency) -> &'static str {
    match residency {
        Residency::MetadataOnly => "metadata_only",
        Residency::Partial => "partial",
        Residency::Materialized => "materialized",
    }
}

fn residency_from_text(text: &str) -> Option<Residency> {
    match text {
        "metadata_only" => Some(Residency::MetadataOnly),
        "partial" => Some(Residency::Partial),
        "materialized" => Some(Residency::Materialized),
        _ => None,
    }
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; ID_LEN]) -> T,
) -> Result<T, EntryError> {
    let array: [u8; ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| EntryError::MalformedRow { column })?;
    Ok(wrap(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{
        TestDir, binding_id, entry_id, open_database, seed_binding, version_id,
    };

    fn revision(value: u64) -> Revision {
        Revision::new(value).expect("测试用的修订号在 SQLite INTEGER 范围内")
    }

    /// 一个普通文件的基线行。
    fn baseline(revision_value: u64, version: VersionId) -> BaselineEntry {
        BaselineEntry {
            entry: entry_id(0x31),
            parent: Some(entry_id(0x30)),
            name: "doc.txt".to_owned(),
            kind: EntryKind::File,
            revision: revision(revision_value),
            content_version: Some(version),
            deleted: false,
        }
    }

    /// 同一个文件在远端的更新状态。
    fn remote(revision_value: u64, version: VersionId) -> RemoteEntry {
        RemoteEntry {
            entry: entry_id(0x31),
            parent: Some(entry_id(0x30)),
            name: "doc.txt".to_owned(),
            kind: EntryKind::File,
            revision: revision(revision_value),
            content_version: Some(version),
            deleted: false,
        }
    }

    fn local(generation: u64) -> LocalEntry {
        LocalEntry {
            entry: entry_id(0x31),
            generation: Generation::new(generation).expect("generation"),
            dirty: Dirty::Dirty,
            residency: Residency::Materialized,
            fingerprint: Some(vec![0xAA, 0xBB, 0xCC]),
            file_identity: Some(vec![0x11; 48]),
        }
    }

    #[tokio::test]
    async fn one_entry_keeps_old_baseline_new_remote_and_local_dirty_together() {
        let dir = TestDir::new("three-views");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x21).await;

        let old_baseline = baseline(3, version_id(0x41));
        let new_remote = remote(5, version_id(0x42));
        let dirty_local = local(7);

        let mut tx = database.begin_write().await.expect("开始写事务");
        put_baseline(&mut tx, binding, &old_baseline)
            .await
            .expect("写基线");
        put_remote(&mut tx, binding, &new_remote)
            .await
            .expect("写远端");
        put_local(&mut tx, binding, &dirty_local)
            .await
            .expect("写本地状态");
        tx.commit().await.expect("提交");

        // 三者同时存在，且各自独立：远端已是 5，基线仍是 3；本地仍是同一 generation 的 dirty。
        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            baseline_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读基线"),
            Some(old_baseline.clone())
        );
        assert_eq!(
            remote_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读远端"),
            Some(new_remote.clone())
        );
        assert_eq!(
            local_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读本地状态"),
            Some(dirty_local.clone())
        );

        // 远端再更新一次：基线不回退、不前进，本地 dirty 与 generation 都不被清除。
        let newer_remote = remote(6, version_id(0x43));
        let mut tx = database.begin_write().await.expect("开始写事务");
        put_remote(&mut tx, binding, &newer_remote)
            .await
            .expect("更新远端");
        tx.commit().await.expect("提交");
        assert_eq!(
            baseline_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读基线"),
            Some(old_baseline.clone()),
            "远端更新不得改写已确认基线"
        );
        assert_eq!(
            local_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读本地状态"),
            Some(dirty_local),
            "远端更新不得清除本地 dirty"
        );

        // 本地发布完成后推进基线：远端与本地行都保持原状。
        let mut published = old_baseline.clone();
        published.revision = revision(6);
        published.content_version = Some(version_id(0x43));
        let mut tx = database.begin_write().await.expect("开始写事务");
        put_baseline(&mut tx, binding, &published)
            .await
            .expect("推进基线");
        tx.commit().await.expect("提交");
        assert_eq!(
            baseline_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读基线"),
            Some(published.clone())
        );
        assert_eq!(
            remote_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读远端"),
            Some(newer_remote)
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn remote_and_local_rows_exist_without_a_baseline() {
        let dir = TestDir::new("first-sync");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x22).await;

        // 首次同步（I07）：远端有条目、本地有新建文件，但都还没有共同基线。
        let first_remote = remote(1, version_id(0x51));
        let mut new_local = local(1);
        new_local.dirty = Dirty::Dirty;
        new_local.residency = Residency::Materialized;

        let mut tx = database.begin_write().await.expect("开始写事务");
        put_remote(&mut tx, binding, &first_remote)
            .await
            .expect("写远端");
        put_local(&mut tx, binding, &new_local)
            .await
            .expect("写本地状态");
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            baseline_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读基线"),
            None
        );
        assert_eq!(
            remote_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读远端"),
            Some(first_remote)
        );
        assert_eq!(
            local_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读本地状态"),
            Some(new_local)
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn deleting_one_view_leaves_the_other_two_untouched() {
        let dir = TestDir::new("independent-delete");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x23).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        put_baseline(&mut tx, binding, &baseline(3, version_id(0x61)))
            .await
            .expect("写基线");
        put_remote(&mut tx, binding, &remote(5, version_id(0x62)))
            .await
            .expect("写远端");
        put_local(&mut tx, binding, &local(7))
            .await
            .expect("写本地状态");
        assert!(
            remove_baseline(&mut tx, binding, entry_id(0x31))
                .await
                .expect("删基线"),
            "删除应命中一行"
        );
        assert!(
            !remove_baseline(&mut tx, binding, entry_id(0x31))
                .await
                .expect("再删基线"),
            "重复删除不应命中"
        );
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            baseline_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读基线"),
            None
        );
        assert_eq!(
            remote_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读远端"),
            Some(remote(5, version_id(0x62)))
        );
        assert_eq!(
            local_entry(&mut conn, binding, entry_id(0x31))
                .await
                .expect("读本地状态"),
            Some(local(7))
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn vault_entry_directory_version_and_empty_bytes_are_rejected() {
        let dir = TestDir::new("entry-reject");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x24).await;

        let mut tx = database.begin_write().await.expect("开始写事务");

        // 根条目不能有名称，也不能是文件。
        let mut named_vault = baseline(1, version_id(0x71));
        named_vault.parent = None;
        named_vault.name = "vault".to_owned();
        named_vault.kind = EntryKind::Directory;
        named_vault.content_version = None;
        assert!(matches!(
            put_baseline(&mut tx, binding, &named_vault)
                .await
                .expect_err("有名称的根条目必须被拒绝"),
            EntryError::VaultEntryShape { .. }
        ));
        let mut file_vault = named_vault.clone();
        file_vault.name = String::new();
        file_vault.kind = EntryKind::File;
        file_vault.content_version = Some(version_id(0x71));
        assert!(matches!(
            put_baseline(&mut tx, binding, &file_vault)
                .await
                .expect_err("文件型根条目必须被拒绝"),
            EntryError::VaultEntryShape { .. }
        ));

        // 目录没有内容版本：Rust 层放行后由迁移里的 `CHECK` 兜底拒绝。
        let mut directory_with_version = baseline(1, version_id(0x72));
        directory_with_version.kind = EntryKind::Directory;
        assert!(
            put_baseline(&mut tx, binding, &directory_with_version)
                .await
                .is_err(),
            "目录带内容版本必须被 CHECK 拒绝"
        );

        // 空字节不等于“未知”。
        let mut empty_fingerprint = local(1);
        empty_fingerprint.fingerprint = Some(Vec::new());
        assert!(matches!(
            put_local(&mut tx, binding, &empty_fingerprint)
                .await
                .expect_err("空指纹必须被拒绝"),
            EntryError::EmptyBytes {
                field: "fingerprint"
            }
        ));
        tx.rollback().await.expect("回滚");

        database.close().await;
    }

    #[tokio::test]
    async fn writes_against_an_unknown_binding_are_rejected_by_the_foreign_key() {
        let dir = TestDir::new("unknown-binding");
        let database = open_database(&dir).await;
        // 先建一个绑定与一条基线，再确认另一个（不存在的）绑定写不进去。
        let binding = seed_binding(&database, 0x25).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        put_baseline(&mut tx, binding, &baseline(3, version_id(0x81)))
            .await
            .expect("写基线");
        assert!(
            put_local(&mut tx, binding_id(0xFE), &local(1))
                .await
                .is_err(),
            "不存在的绑定必须被外键拒绝"
        );
        tx.commit().await.expect("提交");
        database.close().await;
    }

    #[test]
    fn state_text_encodings_round_trip_and_reject_unknown() {
        for dirty in [
            Dirty::Clean,
            Dirty::Dirty,
            Dirty::Snapshotting,
            Dirty::AwaitingCommit,
        ] {
            assert_eq!(dirty_from_text(dirty_text(dirty)), Some(dirty));
        }
        for residency in [
            Residency::MetadataOnly,
            Residency::Partial,
            Residency::Materialized,
        ] {
            assert_eq!(
                residency_from_text(residency_text(residency)),
                Some(residency)
            );
        }
        assert_eq!(dirty_from_text("dirty "), None);
        assert_eq!(dirty_from_text("unknown"), None);
        assert_eq!(residency_from_text("hydrated"), None);
        assert_eq!(EntryKind::from_text("symlink"), None);
        assert_eq!(
            EntryKind::from_text(EntryKind::Directory.as_text()),
            Some(EntryKind::Directory)
        );
    }
}
