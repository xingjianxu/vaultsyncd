//! 服务端命名空间条目（`entries` 表）的事务内读写与约束（`docs/design.md` §7.2、§7.4）。
//!
//! 边界：本模块只读写 `entries`，不写 `roots`/`changes`，不做文件系统 I/O，也不分配
//! `Revision`/`RootSeq`（提交协调卡在同一个事务里推进序号）。写函数接收调用方已开的
//! `BEGIN IMMEDIATE` 写事务（[`Database::begin_write`](crate::db::Database::begin_write)），
//! 因此“先检查后写入”发生在同一个持锁事务里，不存在两个写者之间的检查竞态；只读函数接收
//! `&mut SqliteConnection`，可以直接用连接池，不为一次 GET 去抢写锁。
//!
//! 不变量：
//!
//! - **owner scope**：每个函数的业务参数都有 [`UserId`]；条目只能经 `roots.owner_id` 访问，
//!   不属于该 owner 的 Root 或条目一律 [`EntryError::OutOfScope`] / `None`，不区分“不存在”
//!   与“别人的”，避免泄露存在性。
//! - **活跃同父名称唯一**：`(root_id, parent_id, name_key)` 与 `(root_id, parent_id, name)` 各有
//!   一个 `deleted_at IS NULL` 的部分唯一索引（迁移 0002）。`name_key` 由调用方按平台等价规则
//!   派生（design §7.2：不用简单 Unicode 小写代替 Windows 等价），本层只按它判重，不改写
//!   `name`、不做归一化。
//! - **父子必须同 Root**：复合外键 `(root_id, parent_id) → entries(root_id, id)` 在 SQL 层生效；
//!   本层在写入前还会给出更精确的 [`EntryError::CrossRootParent`]。
//! - **不允许目录环**：移动时从目标父项向上走祖先链（递归 CTE；`UNION` 去重，因此即使库里已有
//!   坏环也会终止），链上出现被移动条目即 [`EntryError::DirectoryCycle`]。
//! - **删除不级联**：本模块只把单条条目标记为 `deleted_at`（软删除），并拒绝删除仍有活跃子项的
//!   目录；整棵子树的回收站删除属 S039。软删除同时释放同父名称（部分唯一索引不含已删行）。
//!
//! `content_version_id` 目前只受“目录没有内容版本”与长度检查约束：它指向的 `versions` 表由
//! S028 建立，外键要到那时才能加（SQLite 追加外键需要重建表）。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{EntryId, ID_LEN, RootId, UserId, VersionId};
use vaultsyncd_domain::version::{Revision, UtcTimestamp};
use vaultsyncd_fs_common::name::{NameIssue, validate_name};

/// 条目类型：文件或目录（design §7.2 `entries.kind`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// 普通文件，可以有内容版本。
    File,
    /// 目录，没有内容版本，可以有子项。
    Directory,
}

impl EntryKind {
    /// 数据库与日志里的规范文本。
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

/// 新建一个条目的输入。
///
/// 不变量（违反时 [`insert_entry`] 报错且不写入任何行）：`parent` 必须是同一个 Root 里活跃的
/// 目录；`name` 必须是跨平台可表示的名称；`name_key` 非空；目录的 `content_version` 必须为空。
#[derive(Clone, Debug)]
pub struct NewEntry {
    /// 新条目的身份（由应用层经 `IdSource` 生成）。
    pub id: EntryId,
    /// 所属 Root。
    pub root: RootId,
    /// 父目录条目；每个 Root 的合成根条目由 [`crate::roots::insert_root`] 建立，不走这里。
    pub parent: EntryId,
    /// 原始名称，原样保存。
    pub name: String,
    /// 平台等价规则派生的比较键，用于同父唯一性判断。
    pub name_key: String,
    /// 文件或目录。
    pub kind: EntryKind,
    /// 服务端分配的修订号。
    pub revision: Revision,
    /// 文件的内容版本；目录必须为 `None`。
    pub content_version: Option<VersionId>,
}

/// 从数据库读出的一条 `entries` 记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryRecord {
    /// 条目身份；路径改变不改变它。
    pub id: EntryId,
    /// 所属 Root。
    pub root: RootId,
    /// 父条目；只有每个 Root 的合成根条目为 `None`。
    pub parent: Option<EntryId>,
    /// 原始名称（根条目为空字符串）。
    pub name: String,
    /// 名称比较键（根条目为空字符串）。
    pub name_key: String,
    /// 文件或目录。
    pub kind: EntryKind,
    /// 服务端分配的修订号。
    pub revision: Revision,
    /// 文件的内容版本。
    pub content_version: Option<VersionId>,
    /// 软删除时间；`None` 表示活跃。
    pub deleted_at: Option<UtcTimestamp>,
}

/// 命名空间条目读写的失败。
#[derive(Debug)]
pub enum EntryError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// Root 不存在或不属于调用方的 owner scope（两种情况不区分）。
    OutOfScope,
    /// 条目不存在、已删除，或不在调用方的 owner scope 内。
    EntryNotFound {
        /// 请求的条目身份。
        id: EntryId,
    },
    /// 父条目不存在或已删除。
    ParentNotFound {
        /// 请求的父条目身份。
        id: EntryId,
    },
    /// 父条目不是目录：文件不能有子项。
    ParentNotDirectory {
        /// 父条目身份。
        id: EntryId,
    },
    /// 父条目属于另一个 Root（design §7.2：父子必须同 Root）。
    CrossRootParent {
        /// 条目自己的 Root。
        entry_root: RootId,
        /// 父条目的 Root。
        parent_root: RootId,
    },
    /// 同一父目录里已有同名的活跃条目。
    DuplicateName {
        /// 父条目身份。
        parent: EntryId,
        /// 冲突的名称。
        name: String,
    },
    /// 移动会让目录成为自己的祖先（design §7.4：目录移动检查环）。
    DirectoryCycle {
        /// 被移动的条目。
        entry: EntryId,
        /// 目标父条目。
        parent: EntryId,
    },
    /// 目录里还有活跃子项，不能删除；子树删除由回收站卡实现。
    DirectoryNotEmpty {
        /// 目录条目身份。
        id: EntryId,
    },
    /// 名称不是跨平台可表示的名称（判定来自 `vaultsyncd-fs-common`）。
    InvalidName {
        /// 给出的名称。
        name: String,
        /// 具体原因。
        issue: NameIssue,
    },
    /// 比较键为空：同父唯一性会退化成“任意名称都不同”，必须拒绝。
    EmptyNameKey,
    /// 目录带了内容版本。
    DirectoryHasContentVersion,
    /// 引用的内容版本还没有被记录：新建条目时不能直接指向一个不存在的版本。
    UnknownContentVersion {
        /// 请求的版本身份。
        version: VersionId,
    },
    /// 条目身份已被占用。
    DuplicateId {
        /// 冲突的条目身份。
        id: EntryId,
    },
    /// 比较并写失败：调用方观察到的 revision 已不是库里的值。
    RevisionChanged {
        /// 请求里携带的期望 revision。
        expected: Revision,
        /// 库里的当前值。
        actual: Revision,
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
            Self::OutOfScope => f.write_str("Root 不存在或不在调用方的 owner scope 内"),
            Self::EntryNotFound { id } => write!(f, "条目 {id} 不存在或已删除"),
            Self::ParentNotFound { id } => write!(f, "父条目 {id} 不存在或已删除"),
            Self::ParentNotDirectory { id } => write!(f, "父条目 {id} 不是目录"),
            Self::CrossRootParent {
                entry_root,
                parent_root,
            } => write!(
                f,
                "父条目属于 Root {parent_root}，与条目的 Root {entry_root} 不同"
            ),
            Self::DuplicateName { parent, name } => {
                write!(f, "父目录 {parent} 下已有同名条目 {name:?}")
            }
            Self::DirectoryCycle { entry, parent } => {
                write!(f, "把 {entry} 移到 {parent} 下会形成目录环")
            }
            Self::DirectoryNotEmpty { id } => write!(f, "目录 {id} 还有活跃子项，不能删除"),
            Self::InvalidName { name, issue } => {
                write!(f, "名称 {name:?} 不可跨平台表示：{issue}")
            }
            Self::EmptyNameKey => f.write_str("名称比较键为空"),
            Self::DirectoryHasContentVersion => f.write_str("目录不能带内容版本"),
            Self::UnknownContentVersion { version } => {
                write!(f, "内容版本 {version} 还没有被记录到该条目")
            }
            Self::DuplicateId { id } => write!(f, "条目身份 {id} 已被占用"),
            Self::RevisionChanged { expected, actual } => {
                write!(f, "条目 revision 已从 {expected} 变为 {actual}")
            }
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
        }
    }
}

impl std::error::Error for EntryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::InvalidName { issue, .. } => Some(issue),
            _ => None,
        }
    }
}

/// `entries` 表的列顺序，多处查询共用，避免手写列名漂移。
const ENTRY_COLUMNS: &str = "e.id, e.root_id, e.parent_id, e.name, e.name_key, e.kind, \
                             e.revision, e.content_version_id, e.deleted_at";

/// 新建条目：检查 owner scope、父目录、名称与同父唯一性后写入一行。
///
/// 返回写入后的记录（重新读出，不把输入当成数据库内容）。
pub async fn insert_entry(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    new: &NewEntry,
) -> Result<EntryRecord, EntryError> {
    require_root_owner(tx, owner, new.root).await?;
    let parent = parent_facts(tx, new.parent).await?;
    if parent.root != new.root {
        return Err(EntryError::CrossRootParent {
            entry_root: new.root,
            parent_root: parent.root,
        });
    }
    if parent.deleted_at.is_some() {
        return Err(EntryError::ParentNotFound { id: new.parent });
    }
    if parent.kind != EntryKind::Directory {
        return Err(EntryError::ParentNotDirectory { id: new.parent });
    }
    validate_child_fields(&new.name, &new.name_key, new.kind, new.content_version)?;
    if let Some(version) = new.content_version {
        require_recorded_version(tx, new.root, new.id, version).await?;
    }

    let inserted = sqlx::query(
        "INSERT INTO entries \
         (id, root_id, parent_id, name, name_key, kind, revision, content_version_id, deleted_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
    )
    .bind(new.id.as_bytes().as_slice())
    .bind(new.root.as_bytes().as_slice())
    .bind(new.parent.as_bytes().as_slice())
    .bind(&new.name)
    .bind(&new.name_key)
    .bind(new.kind.as_text())
    .bind(new.revision.to_db_integer())
    .bind(
        new.content_version
            .map(|version| version.as_bytes().to_vec()),
    )
    .execute(&mut **tx)
    .await;

    if let Err(error) = inserted {
        return Err(classify_write_error(error, new.parent, &new.name, new.id));
    }
    entry_in_scope(tx, owner, new.id)
        .await?
        .ok_or(EntryError::EntryNotFound { id: new.id })
}

/// 在同一 Root 内移动/改名一个活跃条目。
///
/// 检查顺序：owner scope → 条目活跃 → 新名称 → 目标父目录（存在、活跃、同 Root、是目录）→
/// 目录环。目标父目录与当前父目录相同且名称不变时是合法的无操作更新（仍写入给定的修订号）。
pub async fn move_entry(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    id: EntryId,
    target_parent: EntryId,
    name: &str,
    name_key: &str,
    revision: Revision,
) -> Result<EntryRecord, EntryError> {
    let current = active_entry_in_scope(tx, owner, id).await?;
    validate_child_fields(name, name_key, current.kind, current.content_version)?;

    let parent = parent_facts(tx, target_parent).await?;
    if parent.root != current.root {
        return Err(EntryError::CrossRootParent {
            entry_root: current.root,
            parent_root: parent.root,
        });
    }
    if parent.deleted_at.is_some() {
        return Err(EntryError::ParentNotFound { id: target_parent });
    }
    if parent.kind != EntryKind::Directory {
        return Err(EntryError::ParentNotDirectory { id: target_parent });
    }
    if is_ancestor_or_self(tx, id, target_parent).await? {
        return Err(EntryError::DirectoryCycle {
            entry: id,
            parent: target_parent,
        });
    }

    let updated = sqlx::query(
        "UPDATE entries SET parent_id = ?1, name = ?2, name_key = ?3, revision = ?4 \
         WHERE id = ?5 AND deleted_at IS NULL",
    )
    .bind(target_parent.as_bytes().as_slice())
    .bind(name)
    .bind(name_key)
    .bind(revision.to_db_integer())
    .bind(id.as_bytes().as_slice())
    .execute(&mut **tx)
    .await;

    if let Err(error) = updated {
        return Err(classify_write_error(error, target_parent, name, id));
    }
    entry_in_scope(tx, owner, id)
        .await?
        .ok_or(EntryError::EntryNotFound { id })
}

/// 软删除一个活跃条目：写入 `deleted_at`，同父名称随即释放。
///
/// 目录必须已经没有活跃子项；整棵子树的回收站删除属 S039。
pub async fn soft_delete_entry(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    id: EntryId,
    at: UtcTimestamp,
) -> Result<EntryRecord, EntryError> {
    let current = active_entry_in_scope(tx, owner, id).await?;
    if current.kind == EntryKind::Directory {
        let active_children: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM entries \
             WHERE root_id = ?1 AND parent_id = ?2 AND deleted_at IS NULL",
        )
        .bind(current.root.as_bytes().as_slice())
        .bind(id.as_bytes().as_slice())
        .fetch_one(&mut **tx)
        .await
        .map_err(EntryError::Db)?;
        if active_children > 0 {
            return Err(EntryError::DirectoryNotEmpty { id });
        }
    }

    sqlx::query("UPDATE entries SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL")
        .bind(at.to_db_integer())
        .bind(id.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(EntryError::Db)?;

    entry_in_scope(tx, owner, id)
        .await?
        .ok_or(EntryError::EntryNotFound { id })
}

/// 按身份读一条条目（含已软删除的）；owner scope 不匹配时返回 `None`。
pub async fn entry_in_scope(
    conn: &mut SqliteConnection,
    owner: UserId,
    id: EntryId,
) -> Result<Option<EntryRecord>, EntryError> {
    let row = sqlx::query_as::<_, EntryRow>(&format!(
        "SELECT {ENTRY_COLUMNS} FROM entries e JOIN roots r ON r.id = e.root_id \
         WHERE e.id = ?1 AND r.owner_id = ?2"
    ))
    .bind(id.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(EntryError::Db)?;
    row.map(EntryRow::into_record).transpose()
}

/// 列出某个父目录下的活跃子项，按比较键（再按名称、身份）排序。
///
/// Root 不在调用方的 owner scope 内时返回空列表，不区分“无权访问”与“没有子项”。
pub async fn children(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
    parent: EntryId,
) -> Result<Vec<EntryRecord>, EntryError> {
    let rows = sqlx::query_as::<_, EntryRow>(&format!(
        "SELECT {ENTRY_COLUMNS} FROM entries e JOIN roots r ON r.id = e.root_id \
         WHERE e.root_id = ?1 AND e.parent_id = ?2 AND e.deleted_at IS NULL AND r.owner_id = ?3 \
         ORDER BY e.name_key, e.name, e.id"
    ))
    .bind(root.as_bytes().as_slice())
    .bind(parent.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_all(conn)
    .await
    .map_err(EntryError::Db)?;
    rows.into_iter().map(EntryRow::into_record).collect()
}

/// 读某个 Root 的合成根条目（`parent_id IS NULL` 的那一行）；owner 不匹配时为 `None`。
pub async fn root_entry(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
) -> Result<Option<EntryRecord>, EntryError> {
    let row = sqlx::query_as::<_, EntryRow>(&format!(
        "SELECT {ENTRY_COLUMNS} FROM entries e JOIN roots r ON r.id = e.root_id \
         WHERE e.root_id = ?1 AND e.parent_id IS NULL AND e.deleted_at IS NULL \
         AND r.owner_id = ?2"
    ))
    .bind(root.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(EntryError::Db)?;
    row.map(EntryRow::into_record).transpose()
}

/// 按 Root 内相对路径查活跃条目（逐分量按原始名称匹配）。
///
/// 路径是工作目录里的名字序列，因此按 `name` 精确比较而不是 `name_key`：比较键是跨平台等价规则
/// 派生的，用它查路径会把“平台认为等价但名字不同”的条目当成同一条。中间分量必须是活跃目录，
/// 任何一段不存在都返回 `None`（不区分“没有”与“被删”）。
///
/// # Errors
///
/// 存储失败或行无法还原。
#[allow(clippy::result_large_err)]
pub async fn entry_by_path(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
    path: &str,
) -> Result<Option<EntryRecord>, EntryError> {
    let mut current = match root_entry(conn, owner, root).await? {
        Some(entry) => entry,
        None => return Ok(None),
    };
    for part in path.split('/').filter(|part| !part.is_empty()) {
        let row = sqlx::query_as::<_, EntryRow>(&format!(
            "SELECT {ENTRY_COLUMNS} FROM entries e JOIN roots r ON r.id = e.root_id \
             WHERE e.root_id = ?1 AND e.parent_id = ?2 AND e.name = ?3 AND e.deleted_at IS NULL \
             AND r.owner_id = ?4"
        ))
        .bind(root.as_bytes().as_slice())
        .bind(current.id.as_bytes().as_slice())
        .bind(part)
        .bind(owner.as_bytes().as_slice())
        .fetch_optional(&mut *conn)
        .await
        .map_err(EntryError::Db)?;
        match row {
            Some(row) => current = row.into_record()?,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// 条目在 Root 内的相对路径（按父链拼原始名称）；根条目对应空串。
///
/// 已软删除的中间父项同样拼进路径（历史版本与回收站仍要显示原路径），条目不存在或越权返回 `None`。
///
/// # Errors
///
/// 存储失败或父链出现坏行（环、超深）。
#[allow(clippy::result_large_err)]
pub async fn path_of(
    conn: &mut SqliteConnection,
    owner: UserId,
    entry: EntryId,
) -> Result<Option<String>, EntryError> {
    const MAX_DEPTH: usize = 256;
    let mut parts: Vec<String> = Vec::new();
    let mut current = match entry_in_scope(&mut *conn, owner, entry).await? {
        Some(record) => record,
        None => return Ok(None),
    };
    for _ in 0..MAX_DEPTH {
        match current.parent {
            None => {
                parts.reverse();
                return Ok(Some(parts.join("/")));
            }
            Some(parent) => {
                parts.push(current.name.clone());
                current = match entry_in_scope(&mut *conn, owner, parent).await? {
                    Some(record) => record,
                    None => return Ok(None),
                };
            }
        }
    }
    Err(EntryError::MalformedRow {
        column: "parent_id",
    })
}

/// 比较并写地推进条目 revision（内容提交与外部采纳用）：期望值已变化则拒绝。
///
/// 不变量：`next` 必须严格大于 `expected`；条目必须活跃且属于调用方。
///
/// # Errors
///
/// 见 [`EntryError`]：越权/不存在、revision 已变化（[`EntryError::RevisionChanged`]）、非法计数。
#[allow(clippy::result_large_err)]
pub async fn advance_entry_revision(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    id: EntryId,
    expected: Revision,
    next: Revision,
) -> Result<EntryRecord, EntryError> {
    if next <= expected {
        return Err(EntryError::MalformedRow { column: "revision" });
    }
    let current = active_entry_in_scope(tx, owner, id).await?;
    if current.revision != expected {
        return Err(EntryError::RevisionChanged {
            expected,
            actual: current.revision,
        });
    }
    let updated = sqlx::query(
        "UPDATE entries SET revision = ?1 WHERE id = ?2 AND deleted_at IS NULL AND revision = ?3",
    )
    .bind(next.to_db_integer())
    .bind(id.as_bytes().as_slice())
    .bind(expected.to_db_integer())
    .execute(&mut **tx)
    .await
    .map_err(EntryError::Db)?;
    if updated.rows_affected() != 1 {
        let actual = active_entry_in_scope(tx, owner, id).await?.revision;
        return Err(EntryError::RevisionChanged { expected, actual });
    }
    entry_in_scope(tx, owner, id)
        .await?
        .ok_or(EntryError::EntryNotFound { id })
}

/// 建立 Root 的合成根条目；只由 [`crate::roots::insert_root`] 调用。
///
/// 根条目不是任何目录里的分量：名称为空字符串，`parent_id IS NULL`，类型固定为目录。
pub(crate) async fn insert_root_entry(
    tx: &mut Transaction<'_, Sqlite>,
    root: RootId,
    entry: EntryId,
) -> Result<(), EntryError> {
    let inserted = sqlx::query(
        "INSERT INTO entries \
         (id, root_id, parent_id, name, name_key, kind, revision, content_version_id) \
         VALUES (?1, ?2, NULL, '', '', 'directory', 0, NULL)",
    )
    .bind(entry.as_bytes().as_slice())
    .bind(root.as_bytes().as_slice())
    .execute(&mut **tx)
    .await;
    match inserted {
        Ok(_) => Ok(()),
        Err(error) if is_unique_violation(&error) => Err(EntryError::DuplicateId { id: entry }),
        Err(error) => Err(EntryError::Db(error)),
    }
}

/// 数据库行到 [`EntryRecord`] 的中间形态：身份列先读成字节，再按类型检查长度。
#[derive(sqlx::FromRow)]
struct EntryRow {
    id: Vec<u8>,
    root_id: Vec<u8>,
    parent_id: Option<Vec<u8>>,
    name: String,
    name_key: String,
    kind: String,
    revision: i64,
    content_version_id: Option<Vec<u8>>,
    deleted_at: Option<i64>,
}

impl EntryRow {
    fn into_record(self) -> Result<EntryRecord, EntryError> {
        Ok(EntryRecord {
            id: identity(self.id, "id", EntryId::from_bytes)?,
            root: identity(self.root_id, "root_id", RootId::from_bytes)?,
            parent: self
                .parent_id
                .map(|bytes| identity(bytes, "parent_id", EntryId::from_bytes))
                .transpose()?,
            name: self.name,
            name_key: self.name_key,
            kind: EntryKind::from_text(&self.kind)
                .ok_or(EntryError::MalformedRow { column: "kind" })?,
            revision: Revision::from_db_integer(self.revision)
                .map_err(|_| EntryError::MalformedRow { column: "revision" })?,
            content_version: self
                .content_version_id
                .map(|bytes| identity(bytes, "content_version_id", VersionId::from_bytes))
                .transpose()?,
            deleted_at: self
                .deleted_at
                .map(|seconds| {
                    UtcTimestamp::from_db_integer(seconds).map_err(|_| EntryError::MalformedRow {
                        column: "deleted_at",
                    })
                })
                .transpose()?,
        })
    }
}

/// 父条目的事实：所属 Root、类型与是否已软删除。
struct ParentFacts {
    root: RootId,
    kind: EntryKind,
    deleted_at: Option<i64>,
}

async fn parent_facts(
    conn: &mut SqliteConnection,
    parent: EntryId,
) -> Result<ParentFacts, EntryError> {
    let row: Option<(Vec<u8>, String, Option<i64>)> =
        sqlx::query_as("SELECT root_id, kind, deleted_at FROM entries WHERE id = ?1")
            .bind(parent.as_bytes().as_slice())
            .fetch_optional(conn)
            .await
            .map_err(EntryError::Db)?;
    let (root_id, kind, deleted_at) = row.ok_or(EntryError::ParentNotFound { id: parent })?;
    Ok(ParentFacts {
        root: identity(root_id, "root_id", RootId::from_bytes)?,
        kind: EntryKind::from_text(&kind).ok_or(EntryError::MalformedRow { column: "kind" })?,
        deleted_at,
    })
}

/// 读一条必须活跃的条目；已删除与不存在一样报 [`EntryError::EntryNotFound`]。
async fn active_entry_in_scope(
    conn: &mut SqliteConnection,
    owner: UserId,
    id: EntryId,
) -> Result<EntryRecord, EntryError> {
    match entry_in_scope(conn, owner, id).await? {
        Some(record) if record.deleted_at.is_none() => Ok(record),
        _ => Err(EntryError::EntryNotFound { id }),
    }
}

/// 确认 Root 属于调用方的 owner scope。
async fn require_root_owner(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
) -> Result<(), EntryError> {
    let found: Option<Vec<u8>> = sqlx::query_scalar("SELECT owner_id FROM roots WHERE id = ?1")
        .bind(root.as_bytes().as_slice())
        .fetch_optional(conn)
        .await
        .map_err(EntryError::Db)?;
    match found {
        Some(bytes) if bytes.as_slice() == owner.as_bytes() => Ok(()),
        _ => Err(EntryError::OutOfScope),
    }
}

/// `target` 是否就是 `entry`，或位于 `entry` 的子树里（即把 `entry` 移到 `target` 下会成环）。
///
/// 从 `target` 沿 `parent_id` 向上走；用 `UNION` 而不是 `UNION ALL`，因此库里已有坏环时也会终止。
async fn is_ancestor_or_self(
    conn: &mut SqliteConnection,
    entry: EntryId,
    target: EntryId,
) -> Result<bool, EntryError> {
    const SQL: &str = "WITH RECURSIVE ancestors(id, parent_id) AS ( \
                           SELECT id, parent_id FROM entries WHERE id = ?1 \
                           UNION \
                           SELECT e.id, e.parent_id FROM entries e \
                           JOIN ancestors a ON e.id = a.parent_id \
                       ) \
                       SELECT count(*) FROM ancestors WHERE id = ?2";
    let hits: i64 = sqlx::query_scalar(SQL)
        .bind(target.as_bytes().as_slice())
        .bind(entry.as_bytes().as_slice())
        .fetch_one(conn)
        .await
        .map_err(EntryError::Db)?;
    Ok(hits > 0)
}

/// 新建/移动共用的字段检查：名称可跨平台表示、比较键非空、目录不带内容版本。
fn validate_child_fields(
    name: &str,
    name_key: &str,
    kind: EntryKind,
    content_version: Option<VersionId>,
) -> Result<(), EntryError> {
    validate_name(name).map_err(|issue| EntryError::InvalidName {
        name: name.to_owned(),
        issue,
    })?;
    if name_key.is_empty() {
        return Err(EntryError::EmptyNameKey);
    }
    if kind == EntryKind::Directory && content_version.is_some() {
        return Err(EntryError::DirectoryHasContentVersion);
    }
    Ok(())
}

/// 把条目的当前内容版本指针改成给定版本；只由 [`crate::versions::record_version`] 调用。
///
/// 指针的可信来源只有一个：版本行在同一事务里先写入 `versions`（它引用已持久化的内容对象与已记录
/// 的事件批次），再把条目指过去。因此本函数不做存在性检查——存在性由调用方已完成的写入保证；
/// 其它路径想写这一列，必须先有一个版本。
pub(crate) async fn set_current_content_version(
    tx: &mut Transaction<'_, Sqlite>,
    entry: EntryId,
    version: VersionId,
) -> Result<(), EntryError> {
    let updated = sqlx::query("UPDATE entries SET content_version_id = ?1 WHERE id = ?2")
        .bind(version.as_bytes().as_slice())
        .bind(entry.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(EntryError::Db)?;
    if updated.rows_affected() != 1 {
        return Err(EntryError::EntryNotFound { id: entry });
    }
    Ok(())
}

/// 确认给定版本已经被记录到该 Root 的该条目（新建条目写入内容版本前的核对）。
///
async fn require_recorded_version(
    conn: &mut SqliteConnection,
    root: RootId,
    entry: EntryId,
    version: VersionId,
) -> Result<(), EntryError> {
    let found: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT id FROM versions WHERE id = ?1 AND root_id = ?2 AND entry_id = ?3",
    )
    .bind(version.as_bytes().as_slice())
    .bind(root.as_bytes().as_slice())
    .bind(entry.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(EntryError::Db)?;
    match found {
        Some(_) => Ok(()),
        None => Err(EntryError::UnknownContentVersion { version }),
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

/// 写语句是否命中了唯一性约束（主键或部分唯一索引）。
fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .map(|database_error| {
            database_error
                .message()
                .starts_with("UNIQUE constraint failed")
        })
        .unwrap_or(false)
}

/// 把写语句的约束错误翻译成带上下文的类型化错误。
///
/// SQLite 的报错文本会写明命中的列（`UNIQUE constraint failed: entries.root_id, …`），据此区分
/// “同父重名”与“条目身份被占用”；其它错误原样上抛，不猜默认值。
fn classify_write_error(
    error: sqlx::Error,
    parent: EntryId,
    name: &str,
    id: EntryId,
) -> EntryError {
    let Some(database_error) = error.as_database_error() else {
        return EntryError::Db(error);
    };
    let message = database_error.message();
    if !message.starts_with("UNIQUE constraint failed") {
        return EntryError::Db(error);
    }
    if message.contains("entries.name_key") || message.contains("entries.name") {
        return EntryError::DuplicateName {
            parent,
            name: name.to_owned(),
        };
    }
    EntryError::DuplicateId { id }
}

/// 把一个软删除的条目放回命名空间（回收站恢复）：清 `deleted_at`、改父目录/名称、写 revision。
///
/// 比较并写：条目必须仍是软删除且 revision 等于清单里的值；目标父目录必须活跃且是目录，同父活跃
/// 名称被占用时由唯一索引拒绝（[`EntryError::DuplicateName`]），绝不覆盖。
///
/// # Errors
///
/// 见 [`EntryError`]。
#[allow(clippy::result_large_err, clippy::too_many_arguments)]
pub async fn restore_entry(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    id: EntryId,
    target_parent: EntryId,
    name: &str,
    name_key: &str,
    expected_revision: Revision,
    revision: Revision,
) -> Result<EntryRecord, EntryError> {
    let current = entry_in_scope(&mut *tx, owner, id)
        .await?
        .ok_or(EntryError::EntryNotFound { id })?;
    if current.revision != expected_revision {
        return Err(EntryError::RevisionChanged {
            expected: expected_revision,
            actual: current.revision,
        });
    }
    if current.deleted_at.is_none() {
        return Err(EntryError::MalformedRow {
            column: "deleted_at",
        });
    }
    validate_child_fields(name, name_key, current.kind, current.content_version)?;
    let parent = parent_facts(tx, target_parent).await?;
    if parent.root != current.root {
        return Err(EntryError::CrossRootParent {
            entry_root: current.root,
            parent_root: parent.root,
        });
    }
    if parent.deleted_at.is_some() {
        return Err(EntryError::ParentNotFound { id: target_parent });
    }
    if parent.kind != EntryKind::Directory {
        return Err(EntryError::ParentNotDirectory { id: target_parent });
    }
    if is_ancestor_or_self(tx, id, target_parent).await? {
        return Err(EntryError::DirectoryCycle {
            entry: id,
            parent: target_parent,
        });
    }
    let updated = sqlx::query(
        "UPDATE entries SET deleted_at = NULL, parent_id = ?1, name = ?2, name_key = ?3, \
         revision = ?4 WHERE id = ?5 AND deleted_at IS NOT NULL AND revision = ?6",
    )
    .bind(target_parent.as_bytes().as_slice())
    .bind(name)
    .bind(name_key)
    .bind(revision.to_db_integer())
    .bind(id.as_bytes().as_slice())
    .bind(expected_revision.to_db_integer())
    .execute(&mut **tx)
    .await;
    if let Err(error) = updated {
        return Err(classify_write_error(error, target_parent, name, id));
    }
    let affected = updated.map(|result| result.rows_affected()).unwrap_or(0);
    if affected != 1 {
        return Err(EntryError::RevisionChanged {
            expected: expected_revision,
            actual: entry_in_scope(tx, owner, id)
                .await?
                .map_or(expected_revision, |record| record.revision),
        });
    }
    entry_in_scope(tx, owner, id)
        .await?
        .ok_or(EntryError::EntryNotFound { id })
}

/// 读同一父目录下名称等于 `name` 的活跃子项（用于恢复时的名称占用检查）。
///
/// # Errors
///
/// 存储失败。
pub async fn active_child_by_name(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
    parent: EntryId,
    name: &str,
) -> Result<Option<EntryRecord>, EntryError> {
    let row = sqlx::query_as::<_, EntryRow>(&format!(
        "SELECT {ENTRY_COLUMNS} FROM entries e JOIN roots r ON r.id = e.root_id \
         WHERE e.root_id = ?1 AND e.parent_id = ?2 AND e.name = ?3 AND e.deleted_at IS NULL \
         AND r.owner_id = ?4"
    ))
    .bind(root.as_bytes().as_slice())
    .bind(parent.as_bytes().as_slice())
    .bind(name)
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await
    .map_err(EntryError::Db)?;
    row.map(EntryRow::into_record).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    use sqlx::{ConnectOptions, Connection};

    use crate::db::Database;
    use crate::test_support::{TestDir, entry, open_database, register_root, seed_owner, user};

    fn rev(value: u64) -> Revision {
        Revision::new(value).expect("修订号")
    }

    fn at(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_seconds(seconds).expect("固定时间戳必须合法")
    }

    /// 在给定父目录下写一个条目；名称与比较键相同（本卡不派生平台等价键）。
    async fn insert(
        database: &Database,
        owner: UserId,
        root: RootId,
        parent: EntryId,
        id: EntryId,
        name: &str,
        kind: EntryKind,
    ) -> Result<EntryRecord, EntryError> {
        let mut tx = database.begin_write().await.expect("开始写事务");
        let record = insert_entry(
            &mut tx,
            owner,
            &NewEntry {
                id,
                root,
                parent,
                name: name.to_owned(),
                name_key: name.to_owned(),
                kind,
                revision: rev(1),
                content_version: None,
            },
        )
        .await;
        match &record {
            Ok(_) => tx.commit().await.expect("提交写事务"),
            Err(_) => tx.rollback().await.expect("回滚写事务"),
        }
        record
    }

    async fn insert_file(
        database: &Database,
        owner: UserId,
        root: RootId,
        parent: EntryId,
        id: EntryId,
        name: &str,
    ) -> Result<EntryRecord, EntryError> {
        insert(database, owner, root, parent, id, name, EntryKind::File).await
    }

    async fn insert_dir(
        database: &Database,
        owner: UserId,
        root: RootId,
        parent: EntryId,
        id: EntryId,
        name: &str,
    ) -> Result<EntryRecord, EntryError> {
        insert(
            database,
            owner,
            root,
            parent,
            id,
            name,
            EntryKind::Directory,
        )
        .await
    }

    /// 某个 Root 里所有条目的 `(name, parent_id 首字节, deleted_at)`，用于断言“失败不改数据”。
    async fn snapshot(database: &Database, root: RootId) -> Vec<(String, Option<u8>, bool)> {
        let mut conn = database.pool().acquire().await.expect("取连接");
        let rows = sqlx::query_as::<_, (String, Option<Vec<u8>>, Option<i64>)>(
            "SELECT name, parent_id, deleted_at FROM entries WHERE root_id = ?1 ORDER BY id",
        )
        .bind(root.as_bytes().as_slice())
        .fetch_all(&mut *conn)
        .await
        .expect("读条目");
        rows.into_iter()
            .map(|(name, parent, deleted_at)| {
                (name, parent.map(|bytes| bytes[0]), deleted_at.is_some())
            })
            .collect()
    }

    #[tokio::test]
    async fn parent_in_another_root_is_rejected_and_rows_are_unchanged() {
        let dir = TestDir::new("cross-root");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root_a, root_a_dir) = register_root(&database, owner, 0x21).await;
        let (root_b, root_b_dir) = register_root(&database, owner, 0x22).await;

        let before = snapshot(&database, root_a).await;
        let error = insert_file(
            &database,
            owner,
            root_a,
            root_b_dir,
            entry(0x31),
            "cross.txt",
        )
        .await
        .expect_err("跨 Root 父项必须失败");
        assert!(
            matches!(
                error,
                EntryError::CrossRootParent {
                    entry_root,
                    parent_root,
                } if entry_root == root_a && parent_root == root_b
            ),
            "实际错误 {error:?}"
        );
        assert_eq!(snapshot(&database, root_a).await, before, "失败不得改数据");
        assert_eq!(
            snapshot(&database, root_b).await,
            vec![(String::new(), None, false)],
            "另一个 Root 也不得被写入"
        );

        // 合法的同 Root 父项仍然可用：失败没有把连接或事务弄坏。
        let file = insert_file(&database, owner, root_a, root_a_dir, entry(0x32), "ok.txt")
            .await
            .expect("同 Root 父项应成功");
        assert_eq!(file.parent, Some(root_a_dir));
        database.close().await;
    }

    #[tokio::test]
    async fn duplicate_sibling_names_are_rejected_but_other_parents_and_roots_are_free() {
        let dir = TestDir::new("duplicate");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root_a, root_a_dir) = register_root(&database, owner, 0x21).await;
        let (root_b, root_b_dir) = register_root(&database, owner, 0x22).await;
        let subdir = insert_dir(&database, owner, root_a, root_a_dir, entry(0x31), "sub")
            .await
            .expect("建子目录")
            .id;

        let first = insert_file(
            &database,
            owner,
            root_a,
            root_a_dir,
            entry(0x32),
            "report.txt",
        )
        .await
        .expect("首次写入");
        let before = snapshot(&database, root_a).await;

        let error = insert_file(
            &database,
            owner,
            root_a,
            root_a_dir,
            entry(0x33),
            "report.txt",
        )
        .await
        .expect_err("同父同名必须失败");
        assert!(
            matches!(error, EntryError::DuplicateName { ref name, .. } if name == "report.txt"),
            "实际错误 {error:?}"
        );
        assert_eq!(snapshot(&database, root_a).await, before, "失败不得改数据");

        // 不同父目录、不同 Root 里可以用同一个名称。
        insert_file(&database, owner, root_a, subdir, entry(0x34), "report.txt")
            .await
            .expect("不同父目录允许同名");
        insert_file(
            &database,
            owner,
            root_b,
            root_b_dir,
            entry(0x35),
            "report.txt",
        )
        .await
        .expect("不同 Root 允许同名");

        // 比较键相同但名称不同也算重复（平台等价，例如 NTFS 大小写不敏感）。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = insert_entry(
            &mut tx,
            owner,
            &NewEntry {
                id: entry(0x36),
                root: root_a,
                parent: root_a_dir,
                name: "REPORT.TXT".to_owned(),
                name_key: "report.txt".to_owned(),
                kind: EntryKind::File,
                revision: rev(1),
                content_version: None,
            },
        )
        .await
        .expect_err("等价键必须撞名");
        assert!(
            matches!(error, EntryError::DuplicateName { .. }),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 名字完全相同但比较键不同：仍按重复拒绝（名称索引兜住调用方给了不一致的键）。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = insert_entry(
            &mut tx,
            owner,
            &NewEntry {
                id: entry(0x37),
                root: root_a,
                parent: root_a_dir,
                name: "report.txt".to_owned(),
                name_key: "another-key".to_owned(),
                kind: EntryKind::File,
                revision: rev(1),
                content_version: None,
            },
        )
        .await
        .expect_err("同名不同键也必须失败");
        assert!(
            matches!(error, EntryError::DuplicateName { .. }),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 大小写不同的名字不是同一个名称，本层不自动小写（design §7.2）：等价关系必须由注入的
        // 比较键表达。这里用不同的键，于是成为两个独立条目，名称原样保存。
        let uppercase = insert_file(
            &database,
            owner,
            root_a,
            root_a_dir,
            entry(0x39),
            "Report.TXT",
        )
        .await
        .expect("大小写不同、键不同是另一个条目");
        assert_eq!(uppercase.name, "Report.TXT");
        assert_eq!(uppercase.name_key, "Report.TXT");

        // 软删除释放名称。
        let mut tx = database.begin_write().await.expect("开始写事务");
        soft_delete_entry(&mut tx, owner, first.id, at(1_700_000_000))
            .await
            .expect("删除文件");
        tx.commit().await.expect("提交");
        insert_file(
            &database,
            owner,
            root_a,
            root_a_dir,
            entry(0x38),
            "report.txt",
        )
        .await
        .expect("软删除后名称可复用");
        database.close().await;
    }

    #[tokio::test]
    async fn moving_a_directory_under_its_descendant_is_rejected_and_positions_are_unchanged() {
        let dir = TestDir::new("cycle");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, root_dir) = register_root(&database, owner, 0x21).await;
        let outer = insert_dir(&database, owner, root, root_dir, entry(0x31), "outer")
            .await
            .expect("建 outer")
            .id;
        let inner = insert_dir(&database, owner, root, outer, entry(0x32), "inner")
            .await
            .expect("建 inner")
            .id;
        let deepest = insert_dir(&database, owner, root, inner, entry(0x33), "deepest")
            .await
            .expect("建 deepest")
            .id;
        let before = snapshot(&database, root).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = move_entry(&mut tx, owner, outer, deepest, "outer", "outer", rev(2))
            .await
            .expect_err("移到自己后代下必须失败");
        assert!(
            matches!(error, EntryError::DirectoryCycle { entry, parent } if entry == outer && parent == deepest),
            "实际错误 {error:?}"
        );
        // 自己当自己的父项同样是环。
        let error = move_entry(&mut tx, owner, inner, inner, "inner", "inner", rev(2))
            .await
            .expect_err("自己当父项必须失败");
        assert!(
            matches!(error, EntryError::DirectoryCycle { .. }),
            "实际错误 {error:?}"
        );
        tx.commit().await.expect("提交（失败不应留下改动）");

        assert_eq!(
            snapshot(&database, root).await,
            before,
            "位置与修订号不得改变"
        );
        // 合法的同 Root 移动仍然可以：把 deepest 移到根目录下。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let moved = move_entry(
            &mut tx,
            owner,
            deepest,
            root_dir,
            "deepest",
            "deepest",
            rev(2),
        )
        .await
        .expect("合法移动应成功");
        tx.commit().await.expect("提交");
        assert_eq!(moved.parent, Some(root_dir));
        assert_eq!(moved.revision, rev(2));
        database.close().await;
    }

    #[tokio::test]
    async fn moving_across_roots_and_under_a_file_is_rejected() {
        let dir = TestDir::new("move-reject");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root_a, root_a_dir) = register_root(&database, owner, 0x21).await;
        let (_root_b, root_b_dir) = register_root(&database, owner, 0x22).await;
        let file = insert_file(
            &database,
            owner,
            root_a,
            root_a_dir,
            entry(0x31),
            "file.txt",
        )
        .await
        .expect("建文件")
        .id;
        let before = snapshot(&database, root_a).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = move_entry(
            &mut tx,
            owner,
            file,
            root_b_dir,
            "file.txt",
            "file.txt",
            rev(2),
        )
        .await
        .expect_err("跨 Root 移动必须失败");
        assert!(
            matches!(error, EntryError::CrossRootParent { .. }),
            "实际错误 {error:?}"
        );
        let error = move_entry(&mut tx, owner, file, file, "file.txt", "file.txt", rev(2))
            .await
            .expect_err("文件不能当父项");
        assert!(
            matches!(error, EntryError::ParentNotDirectory { .. }),
            "实际错误 {error:?}"
        );
        tx.commit().await.expect("提交（失败不应留下改动）");
        assert_eq!(snapshot(&database, root_a).await, before);
        database.close().await;
    }

    #[tokio::test]
    async fn owner_scope_hides_and_blocks_another_owners_namespace() {
        let dir = TestDir::new("owner-scope");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let other = user(0x12);
        let mut tx = database.begin_write().await.expect("开始写事务");
        crate::test_support::insert_user(&mut tx, other).await;
        tx.commit().await.expect("提交");

        let (root, root_dir) = register_root(&database, owner, 0x21).await;
        let file = insert_file(&database, owner, root, root_dir, entry(0x31), "secret.txt")
            .await
            .expect("建文件")
            .id;
        let before = snapshot(&database, root).await;

        let mut conn = database.pool().acquire().await.expect("取只读连接");
        assert_eq!(
            entry_in_scope(&mut conn, other, file).await.expect("查询"),
            None,
            "别人的 owner 读不到条目"
        );
        assert!(
            children(&mut conn, other, root, root_dir)
                .await
                .expect("查询")
                .is_empty(),
            "别人的 owner 读不到子项列表"
        );
        assert_eq!(
            root_entry(&mut conn, other, root).await.expect("查询"),
            None,
            "别人的 owner 读不到根条目"
        );
        drop(conn);

        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = insert_entry(
            &mut tx,
            other,
            &NewEntry {
                id: entry(0x32),
                root,
                parent: root_dir,
                name: "intrusion.txt".to_owned(),
                name_key: "intrusion.txt".to_owned(),
                kind: EntryKind::File,
                revision: rev(1),
                content_version: None,
            },
        )
        .await
        .expect_err("别人的 owner 不能写入");
        assert!(
            matches!(error, EntryError::OutOfScope),
            "实际错误 {error:?}"
        );
        let error = move_entry(
            &mut tx,
            other,
            file,
            root_dir,
            "moved.txt",
            "moved.txt",
            rev(2),
        )
        .await
        .expect_err("别人的 owner 不能移动");
        assert!(
            matches!(error, EntryError::EntryNotFound { .. }),
            "实际错误 {error:?}"
        );
        let error = soft_delete_entry(&mut tx, other, file, at(1_700_000_000))
            .await
            .expect_err("别人的 owner 不能删除");
        assert!(
            matches!(error, EntryError::EntryNotFound { .. }),
            "实际错误 {error:?}"
        );
        tx.commit().await.expect("提交（失败不应留下改动）");
        assert_eq!(snapshot(&database, root).await, before);
        database.close().await;
    }

    #[tokio::test]
    async fn directory_with_active_children_cannot_be_deleted() {
        let dir = TestDir::new("delete-dir");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, root_dir) = register_root(&database, owner, 0x21).await;
        let outer = insert_dir(&database, owner, root, root_dir, entry(0x31), "outer")
            .await
            .expect("建 outer")
            .id;
        insert_file(&database, owner, root, outer, entry(0x32), "inside.txt")
            .await
            .expect("建文件");

        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = soft_delete_entry(&mut tx, owner, outer, at(1_700_000_000))
            .await
            .expect_err("非空目录不能删除");
        assert!(
            matches!(error, EntryError::DirectoryNotEmpty { id } if id == outer),
            "实际错误 {error:?}"
        );
        // 子项先删掉之后目录就能删（子树回收站属 S039，本卡只拒绝非空目录）。
        soft_delete_entry(&mut tx, owner, entry(0x32), at(1_700_000_001))
            .await
            .expect("删除子文件");
        let deleted = soft_delete_entry(&mut tx, owner, outer, at(1_700_000_002))
            .await
            .expect("删除空目录");
        assert_eq!(deleted.deleted_at, Some(at(1_700_000_002)));
        let error = soft_delete_entry(&mut tx, owner, outer, at(1_700_000_003))
            .await
            .expect_err("重复删除报不存在");
        assert!(
            matches!(error, EntryError::EntryNotFound { .. }),
            "实际错误 {error:?}"
        );
        tx.commit().await.expect("提交");
        database.close().await;
    }

    #[tokio::test]
    async fn invalid_names_empty_keys_and_directory_content_versions_are_rejected() {
        let dir = TestDir::new("invalid-input");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, root_dir) = register_root(&database, owner, 0x21).await;
        let before = snapshot(&database, root).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = insert_entry(
            &mut tx,
            owner,
            &NewEntry {
                id: entry(0x31),
                root,
                parent: root_dir,
                name: "CON".to_owned(),
                name_key: "con".to_owned(),
                kind: EntryKind::File,
                revision: rev(1),
                content_version: None,
            },
        )
        .await
        .expect_err("Windows 保留名必须拒绝");
        assert!(
            matches!(error, EntryError::InvalidName { ref issue, .. } if issue.to_string().contains("CON")),
            "实际错误 {error:?}"
        );
        let error = insert_entry(
            &mut tx,
            owner,
            &NewEntry {
                id: entry(0x32),
                root,
                parent: root_dir,
                name: "dir".to_owned(),
                name_key: "dir".to_owned(),
                kind: EntryKind::Directory,
                revision: rev(1),
                content_version: Some(VersionId::from_bytes([0x41; ID_LEN])),
            },
        )
        .await
        .expect_err("目录不能带内容版本");
        assert!(
            matches!(error, EntryError::DirectoryHasContentVersion),
            "实际错误 {error:?}"
        );
        let error = insert_entry(
            &mut tx,
            owner,
            &NewEntry {
                id: entry(0x33),
                root,
                parent: root_dir,
                name: "keys.txt".to_owned(),
                name_key: String::new(),
                kind: EntryKind::File,
                revision: rev(1),
                content_version: None,
            },
        )
        .await
        .expect_err("空比较键必须拒绝");
        assert!(
            matches!(error, EntryError::EmptyNameKey),
            "实际错误 {error:?}"
        );
        // 新建条目不能直接指向一个还没记录的版本（版本必须先引用已存在的条目）。
        let unrecorded = VersionId::from_bytes([0x42; ID_LEN]);
        let error = insert_entry(
            &mut tx,
            owner,
            &NewEntry {
                id: entry(0x34),
                root,
                parent: root_dir,
                name: "note.txt".to_owned(),
                name_key: "note.txt".to_owned(),
                kind: EntryKind::File,
                revision: rev(1),
                content_version: Some(unrecorded),
            },
        )
        .await
        .expect_err("未记录的内容版本必须拒绝");
        assert!(
            matches!(error, EntryError::UnknownContentVersion { version } if version == unrecorded),
            "实际错误 {error:?}"
        );
        tx.commit().await.expect("提交（失败不应留下改动）");
        assert_eq!(snapshot(&database, root).await, before, "只应有根条目");
        database.close().await;
    }

    #[tokio::test]
    async fn cross_root_parent_is_also_refused_by_the_database_constraint() {
        let dir = TestDir::new("db-constraint");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root_a, _root_a_dir) = register_root(&database, owner, 0x21).await;
        let (_root_b, root_b_dir) = register_root(&database, owner, 0x22).await;

        // 绕过本模块直接写 SQL：外键本身必须挡住跨 Root 的父项，而不是只靠 Rust 侧检查。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = sqlx::query(
            "INSERT INTO entries (id, root_id, parent_id, name, name_key, kind, revision) \
             VALUES (?1, ?2, ?3, 'raw.txt', 'raw.txt', 'file', 1)",
        )
        .bind(entry(0x31).as_bytes().as_slice())
        .bind(root_a.as_bytes().as_slice())
        .bind(root_b_dir.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .expect_err("外键必须拒绝跨 Root 父项");
        assert!(
            error.to_string().contains("FOREIGN KEY"),
            "实际错误 {error}"
        );
        tx.rollback().await.expect("回滚");
        assert_eq!(
            snapshot(&database, root_a).await,
            vec![(String::new(), None, false)],
            "外键失败不得写入"
        );
        database.close().await;
    }

    #[tokio::test]
    async fn upgrade_from_v1_creates_namespace_tables_and_keeps_existing_rows() {
        let dir = TestDir::new("upgrade-v1");
        let path = dir.database();
        let migrations = dir.write_migrations(&[("0001_initial.sql", BASELINE_SQL)]);
        let previous = sqlx::migrate::Migrator::new(migrations.as_path())
            .await
            .expect("解析旧迁移集");

        // 上一版二进制（只有 0001）建的库，带着一条数据。
        let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(10))
            .connect()
            .await
            .expect("建旧库");
        previous.run(&mut conn).await.expect("执行 0001");
        sqlx::query("INSERT INTO store_metadata (key, value) VALUES ('server_id', 'fixture')")
            .execute(&mut conn)
            .await
            .expect("写旧数据");
        sqlx::query(&format!(
            "PRAGMA application_id = {}",
            crate::db::SERVER_APPLICATION_ID
        ))
        .execute(&mut conn)
        .await
        .expect("写种类标识");
        sqlx::query("PRAGMA user_version = 1")
            .execute(&mut conn)
            .await
            .expect("写 schema 版本");
        conn.close().await.expect("关闭旧库连接");

        // 本二进制打开它：0002 建命名空间表、0003 建对象/版本/批次/操作表、0004 建上传/意图/租约表、
        // 0005 建回收站清单与墓碑，
        // 旧数据保留，迁移前备份落在同目录。
        let database = Database::open(&path).await.expect("升级旧库");
        assert_eq!(database.schema_version(), 8);
        let mut conn = database.pool().acquire().await.expect("取连接");
        let value: String =
            sqlx::query_scalar("SELECT value FROM store_metadata WHERE key = 'server_id'")
                .fetch_one(&mut *conn)
                .await
                .expect("读取旧数据");
        assert_eq!(value, "fixture");
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name IN ('users', 'roots', 'entries', 'objects', 'versions', 'changes', 'operations', \
                          'uploads', 'upload_parts', 'fs_intents', 'backup_leases', 'object_leases', \
                          'trash_entries', 'tombstones') \
             ORDER BY name",
        )
        .fetch_all(&mut *conn)
        .await
        .expect("列出新表");
        assert_eq!(
            tables,
            vec![
                "backup_leases",
                "changes",
                "entries",
                "fs_intents",
                "object_leases",
                "objects",
                "operations",
                "roots",
                "tombstones",
                "trash_entries",
                "upload_parts",
                "uploads",
                "users",
                "versions"
            ]
        );
        drop(conn);
        assert!(
            crate::db::migration_backup_path(&path, 1, 8).exists(),
            "非空库升级前应有备份"
        );

        // 新表立刻可用：owner + Root + 根条目一次事务写入。
        let mut tx = database.begin_write().await.expect("开始写事务");
        crate::test_support::insert_user(&mut tx, user(0x11)).await;
        tx.commit().await.expect("提交");
        register_root(&database, user(0x11), 0x21).await;
        database.close().await;
    }

    /// `migrations/0001_initial.sql` 的真实内容，作为“上一版已发布迁移”的 fixture。
    const BASELINE_SQL: &str = include_str!("../migrations/0001_initial.sql");
}
