//! 内容对象（`objects`）与不可变版本历史（`versions`）：事务内写入与只读查询
//! （`docs/design.md` §7.2、§11.2、I01、I05、I12）。
//!
//! 边界：本模块只读写 `objects`/`versions`，并把条目的“当前内容版本”指针写成同一个版本
//! （[`crate::entries`] 的 `set_current_content_version`）。它不做文件系统 I/O、不算哈希、不分配
//! `RootSeq`：哈希由平台层校验后交给本层（design §11.2 步骤 3），批次序号由 [`crate::changes`]
//! 分配。写函数接收调用方已开的 `BEGIN IMMEDIATE` 事务，因此“对象存在 → 版本入历史 → 条目指针
//! 更新”与同一事务里的事件批次一起提交或一起回滚。
//!
//! 不变量：
//!
//! - **版本只引用已持久化内容**：`versions(root_id, object_hash) → objects(root_id, hash)` 外键；
//!   本层在写入前核对对象存在且长度一致，长度不一致就是“[`VersionError::ContentSizeMismatch`]”，
//!   不允许“同一个内容地址两种长度”。（design I05：不完整内容不可见为完整版本。）
//! - **版本只属于已记录的事务批次**：`versions(root_id, created_seq) → changes(root_id, seq)`
//!   外键；写入前核对批次存在，否则 [`VersionError::SeqNotRecorded`]。回滚的事务不会留下版本、
//!   也不会有指向不存在批次的历史行（design I09、§11.2 步骤 7）。
//! - **历史只读**：已提交版本的标识与内容引用由触发器拒绝更新（design I12），唯一可改的列是
//!   `superseded_at`；恢复历史要写一个新版本并让它成为当前版本，而不是改写旧行。
//! - **每个条目至多一个当前版本**：部分唯一索引 `(root_id, entry_id) WHERE superseded_at IS NULL`。
//!   写入新版本时先把旧当前版本标记为被取代，再插入新行，因此中间状态也在事务内。
//! - **只有文件有条目版本**：目录没有 `VersionId`（design §7.1）。给目录记录版本报
//!   [`VersionError::NotAFile`]；为已软删除的条目重建版本也拒绝，删除后的历史仍然可读。
//! - **owner scope**：每个接口都带 [`UserId`]，经 `roots.owner_id` 判定；不属于该 owner 的 Root
//!   一律 [`VersionError::OutOfScope`] / `None`，不泄露存在性。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{EntryId, ID_LEN, RootId, UserId, VersionId};
use vaultsyncd_domain::version::{RootSeq, UtcTimestamp};

use crate::entries::{self, EntryError, EntryKind};

/// 内容哈希的字节长度（SHA-256，design §7.1 `ContentHash`）。
pub const OBJECT_HASH_LEN: usize = 32;

/// 一次历史清单的条数上限（design §15.2：所有清单分页有上限）。
pub const MAX_VERSION_PAGE: u32 = 500;

/// 内容哈希：SHA-256 原始 32 字节，与 `objects.hash`、`versions.object_hash` 同形。
pub type ObjectHash = [u8; OBJECT_HASH_LEN];

/// 对象是否仍可被新版本引用（`objects.gc_state`）。
///
/// 只有两值：`live` 可引用；`collectable` 已被 GC 标记，不再接受新引用。更细的 GC 阶段属于对象
/// 回收卡，本卡不提前定义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectGcState {
    /// 引用计数意义上的可见对象，可被新版本引用。
    Live,
    /// 已被标记待回收；只允许 GC 处理，不再接受新引用。
    Collectable,
}

impl ObjectGcState {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Collectable => "collectable",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜默认值。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "live" => Some(Self::Live),
            "collectable" => Some(Self::Collectable),
            _ => None,
        }
    }
}

/// 记录一个已持久化并校验过的内容对象。
///
/// 不变量（违反时报错且不写入）：Root 属于调用方；`storage_path` 非空且全库唯一；`size` 在数据库
/// 有符号 64 位范围内。同一 `(root, hash)` 再次以相同长度与同一路径登记是幂等的（返回既有记录），
/// 长度或路径不同则报 [`VersionError::ObjectConflict`]——内容地址只能对应一份内容。
#[derive(Clone, Debug)]
pub struct NewObject {
    /// 所属 Root。
    pub root: RootId,
    /// 内容 SHA-256。
    pub hash: ObjectHash,
    /// 内容字节数。
    pub size: u64,
    /// 内部对象存储里的位置。
    pub storage_path: String,
    /// 校验通过的时间（UTC Unix 秒）。
    pub verified_at: UtcTimestamp,
}

/// 从数据库读出的一条 `objects` 记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectRecord {
    /// 所属 Root。
    pub root: RootId,
    /// 内容 SHA-256。
    pub hash: ObjectHash,
    /// 内容字节数。
    pub size: u64,
    /// 内部对象存储里的位置。
    pub storage_path: String,
    /// 校验通过的时间。
    pub verified_at: UtcTimestamp,
    /// 回收状态。
    pub gc_state: ObjectGcState,
}

/// 新版本内容的来源（`versions.origin`）。
///
/// 外部变更与客户端提交走同一条版本与事件流程（design I14），区别只在审计字段：首次同步导入属于
/// [`Self::Client`]，服务端采集到的外部写入属于 [`Self::External`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionOrigin {
    /// 客户端提交或首次同步导入。
    Client,
    /// 服务端观察到的外部变更。
    External,
}

impl VersionOrigin {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::External => "external",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜默认值。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "client" => Some(Self::Client),
            "external" => Some(Self::External),
            _ => None,
        }
    }
}

/// 记录一个新版本（`versions` 的一行），并让它成为条目的当前版本。
///
/// 不变量（违反时报错且不写入）：Root 属于调用方；条目在该 Root 内、是活跃的文件；对象的长度与
/// 版本长度一致；`created_seq` 是该 Root 已存在的批次；`at` 不早于该批次的创建时间由调用方保证。
#[derive(Clone, Debug)]
pub struct NewVersion {
    /// 版本身份（由应用层经 `IdSource` 生成）。
    pub id: VersionId,
    /// 所属 Root。
    pub root: RootId,
    /// 内容所属的条目。
    pub entry: EntryId,
    /// 内容对象的哈希。
    pub object_hash: ObjectHash,
    /// 内容字节数，必须与对象一致。
    pub size: u64,
    /// 记录本版本的批次序号。
    pub created_seq: RootSeq,
    /// 内容来源。
    pub origin: VersionOrigin,
    /// 本版本成为当前版本的时间；同时写入被取代的旧版本的 `superseded_at`。
    pub at: UtcTimestamp,
}

/// 从数据库读出的一条 `versions` 记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionRecord {
    /// 版本身份。
    pub id: VersionId,
    /// 所属 Root。
    pub root: RootId,
    /// 内容所属的条目。
    pub entry: EntryId,
    /// 内容对象的哈希。
    pub object_hash: ObjectHash,
    /// 内容字节数。
    pub size: u64,
    /// 记录本版本的批次序号。
    pub created_seq: RootSeq,
    /// 被取代的时间；`None` 表示这是当前版本。
    pub superseded_at: Option<UtcTimestamp>,
    /// 内容来源。
    pub origin: VersionOrigin,
}

/// 对象与版本读写的失败。
#[derive(Debug)]
pub enum VersionError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// Root 不存在或不属于调用方的 owner scope（两种情况不区分）。
    OutOfScope,
    /// 条目不存在、已删除，或不属于该 Root。
    EntryNotFound {
        /// 请求的条目身份。
        id: EntryId,
    },
    /// 条目是目录：目录没有内容版本（design §7.1）。
    NotAFile {
        /// 条目身份。
        id: EntryId,
    },
    /// 版本引用的内容对象还没有被记录（内容未持久化或未校验）。
    UnknownObject {
        /// 请求的内容哈希。
        hash: ObjectHash,
    },
    /// 版本长度与内容对象的长度不同：同一个内容地址不能对应两份字节。
    ContentSizeMismatch {
        /// 版本给出的长度。
        version_size: u64,
        /// 对象记录的长度。
        object_size: u64,
    },
    /// 批次序号在该 Root 里不存在：版本只能写在已记录的事务批次里。
    SeqNotRecorded {
        /// 请求的批次序号。
        seq: RootSeq,
    },
    /// 同一内容地址已用不同的长度或路径登记过。
    ObjectConflict {
        /// 冲突的内容哈希。
        hash: ObjectHash,
        /// 具体原因。
        reason: &'static str,
    },
    /// 存储路径为空：对象必须有位置，不能用空路径登记。
    EmptyStoragePath,
    /// 存储路径已被另一个对象占用。
    DuplicateStoragePath {
        /// 冲突的路径。
        path: String,
    },
    /// 条目同时存在两个“当前版本”：新版本写入前必须先取代旧当前版本。
    CurrentVersionConflict {
        /// 条目身份。
        entry: EntryId,
    },
    /// 版本身份已被占用。
    DuplicateId {
        /// 冲突的版本身份。
        id: VersionId,
    },
    /// 长度超出数据库有符号 64 位范围。
    SizeAboveDbLimit {
        /// 给出的长度。
        size: u64,
    },
    /// 历史清单的分页大小不在 `1..=MAX_VERSION_PAGE`。
    InvalidLimit,
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
    /// 条目侧的检查失败（当前版本指针写入）。
    Entry(EntryError),
}

impl std::fmt::Display for VersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::OutOfScope => f.write_str("Root 不存在或不在调用方的 owner scope 内"),
            Self::EntryNotFound { id } => write!(f, "条目 {id} 不存在、已删除或不在该 Root 内"),
            Self::NotAFile { id } => write!(f, "条目 {id} 是目录，目录没有内容版本"),
            Self::UnknownObject { hash } => {
                write!(f, "内容对象 {} 尚未记录", hex(hash.as_slice()))
            }
            Self::ContentSizeMismatch {
                version_size,
                object_size,
            } => write!(
                f,
                "版本长度 {version_size} 与内容对象长度 {object_size} 不同"
            ),
            Self::SeqNotRecorded { seq } => write!(f, "批次序号 {seq} 在该 Root 里不存在"),
            Self::ObjectConflict { hash, reason } => {
                write!(f, "内容对象 {} 冲突：{reason}", hex(hash.as_slice()))
            }
            Self::EmptyStoragePath => f.write_str("内容对象的存储路径不能为空"),
            Self::DuplicateStoragePath { path } => write!(f, "存储路径 {path:?} 已被占用"),
            Self::CurrentVersionConflict { entry } => {
                write!(f, "条目 {entry} 同时存在两个当前版本")
            }
            Self::DuplicateId { id } => write!(f, "版本身份 {id} 已被占用"),
            Self::SizeAboveDbLimit { size } => {
                write!(f, "内容长度 {size} 超过数据库有符号 64 位上限")
            }
            Self::InvalidLimit => write!(f, "分页大小必须在 1..={MAX_VERSION_PAGE} 之内"),
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
            Self::Entry(source) => write!(f, "条目侧写入失败：{source}"),
        }
    }
}

impl std::error::Error for VersionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::Entry(source) => Some(source),
            _ => None,
        }
    }
}

/// `objects` 表的列顺序，多处查询共用。
const OBJECT_COLUMNS: &str = "o.root_id, o.hash, o.size, o.storage_path, o.verified_at, o.gc_state";

/// `versions` 表的列顺序，多处查询共用。
const VERSION_COLUMNS: &str = "v.id, v.root_id, v.entry_id, v.object_hash, v.size, \
                               v.created_seq, v.superseded_at, v.origin";

/// 记录一个已校验的内容对象；同一 `(root, hash)` 的相同登记是幂等的。
pub async fn record_object(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    new: &NewObject,
) -> Result<ObjectRecord, VersionError> {
    require_root_owner(tx, owner, new.root).await?;
    require_storage_path(&new.storage_path)?;
    let size = db_size(new.size)?;

    if let Some(existing) = object_facts(tx, new.root, &new.hash).await? {
        if existing.gc_state != "live" {
            return Err(VersionError::ObjectConflict {
                hash: new.hash,
                reason: "对象已授权 GC 删除",
            });
        }
        if existing.size != new.size {
            return Err(VersionError::ObjectConflict {
                hash: new.hash,
                reason: "同一内容地址已用不同长度登记",
            });
        }
        if existing.storage_path != new.storage_path {
            return Err(VersionError::ObjectConflict {
                hash: new.hash,
                reason: "同一内容地址已用不同存储路径登记",
            });
        }
        // 幂等登记返回**库里的**记录，而不是把输入拼成结果。
        return object_in_scope(tx, owner, new.root, &new.hash)
            .await?
            .ok_or(VersionError::OutOfScope);
    }

    let inserted = sqlx::query(
        "INSERT INTO objects (root_id, hash, size, storage_path, verified_at, gc_state) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(new.root.as_bytes().as_slice())
    .bind(new.hash.as_slice())
    .bind(size)
    .bind(&new.storage_path)
    .bind(new.verified_at.to_db_integer())
    .bind(ObjectGcState::Live.as_text())
    .execute(&mut **tx)
    .await;
    if let Err(error) = inserted {
        return Err(classify_object_error(error, new));
    }
    object_in_scope(tx, owner, new.root, &new.hash)
        .await?
        .ok_or(VersionError::OutOfScope)
}

/// 按内容哈希读一个对象（含已标记回收的）；owner scope 不匹配时返回 `None`。
pub async fn object_in_scope(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
    hash: &ObjectHash,
) -> Result<Option<ObjectRecord>, VersionError> {
    let row = sqlx::query_as::<_, ObjectRow>(&format!(
        "SELECT {OBJECT_COLUMNS} FROM objects o JOIN roots r ON r.id = o.root_id \
         WHERE o.root_id = ?1 AND o.hash = ?2 AND r.owner_id = ?3"
    ))
    .bind(root.as_bytes().as_slice())
    .bind(hash.as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(VersionError::Db)?;
    row.map(ObjectRow::into_record).transpose()
}

/// 记录一个新版本并让它成为条目的当前版本。
///
/// 调用顺序固定为：核对对象 → 核对批次 → 取代旧当前版本 → 插入新版本 → 更新条目指针，全部在调用方
/// 的同一个事务里；任一步失败都不得提交。
pub async fn record_version(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    new: &NewVersion,
) -> Result<VersionRecord, VersionError> {
    require_root_owner(tx, owner, new.root).await?;
    require_active_file(tx, new.root, new.entry).await?;
    let object =
        object_facts(tx, new.root, &new.object_hash)
            .await?
            .ok_or(VersionError::UnknownObject {
                hash: new.object_hash,
            })?;
    if object.size != new.size {
        return Err(VersionError::ContentSizeMismatch {
            version_size: new.size,
            object_size: object.size,
        });
    }
    require_recorded_seq(tx, new.root, new.created_seq).await?;

    // 先把旧当前版本标记为被取代，再插入新行：部分唯一索引要求任何时刻只有一行是当前版本。
    sqlx::query(
        "UPDATE versions SET superseded_at = ?1 \
         WHERE root_id = ?2 AND entry_id = ?3 AND superseded_at IS NULL",
    )
    .bind(new.at.to_db_integer())
    .bind(new.root.as_bytes().as_slice())
    .bind(new.entry.as_bytes().as_slice())
    .execute(&mut **tx)
    .await
    .map_err(VersionError::Db)?;

    let inserted = sqlx::query(
        "INSERT INTO versions \
         (id, root_id, entry_id, object_hash, size, created_seq, superseded_at, origin) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)",
    )
    .bind(new.id.as_bytes().as_slice())
    .bind(new.root.as_bytes().as_slice())
    .bind(new.entry.as_bytes().as_slice())
    .bind(new.object_hash.as_slice())
    .bind(db_size(new.size)?)
    .bind(new.created_seq.to_db_integer())
    .bind(new.origin.as_text())
    .execute(&mut **tx)
    .await;
    if let Err(error) = inserted {
        return Err(classify_version_error(error, new));
    }

    entries::set_current_content_version(tx, new.entry, new.id)
        .await
        .map_err(VersionError::Entry)?;

    version_in_scope(tx, owner, new.id)
        .await?
        .ok_or(VersionError::DuplicateId { id: new.id })
}

/// 读条目的当前版本（`superseded_at IS NULL`）；owner scope 不匹配时返回 `None`。
pub async fn current_version(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
    entry: EntryId,
) -> Result<Option<VersionRecord>, VersionError> {
    let row = sqlx::query_as::<_, VersionRow>(&format!(
        "SELECT {VERSION_COLUMNS} FROM versions v JOIN roots r ON r.id = v.root_id \
         WHERE v.root_id = ?1 AND v.entry_id = ?2 AND v.superseded_at IS NULL AND r.owner_id = ?3"
    ))
    .bind(root.as_bytes().as_slice())
    .bind(entry.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(VersionError::Db)?;
    row.map(VersionRow::into_record).transpose()
}

/// 读条目的历史版本（含当前版本），新的在前；最多 `limit` 条。
///
/// 已软删除条目的历史同样可读（design §15.2：权限检查覆盖已删除条目）。
pub async fn version_history(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
    entry: EntryId,
    limit: u32,
) -> Result<Vec<VersionRecord>, VersionError> {
    if limit == 0 || limit > MAX_VERSION_PAGE {
        return Err(VersionError::InvalidLimit);
    }
    require_root_owner(conn, owner, root).await?;
    let rows = sqlx::query_as::<_, VersionRow>(&format!(
        "SELECT {VERSION_COLUMNS} FROM versions v WHERE v.root_id = ?1 AND v.entry_id = ?2 \
         ORDER BY v.created_seq DESC, v.superseded_at, v.id LIMIT ?3"
    ))
    .bind(root.as_bytes().as_slice())
    .bind(entry.as_bytes().as_slice())
    .bind(i64::from(limit))
    .fetch_all(conn)
    .await
    .map_err(VersionError::Db)?;
    rows.into_iter().map(VersionRow::into_record).collect()
}

/// 按身份读一条版本，owner scope 不匹配时返回 `None`。
async fn version_in_scope(
    conn: &mut SqliteConnection,
    owner: UserId,
    id: VersionId,
) -> Result<Option<VersionRecord>, VersionError> {
    let row = sqlx::query_as::<_, VersionRow>(&format!(
        "SELECT {VERSION_COLUMNS} FROM versions v JOIN roots r ON r.id = v.root_id \
         WHERE v.id = ?1 AND r.owner_id = ?2"
    ))
    .bind(id.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(VersionError::Db)?;
    row.map(VersionRow::into_record).transpose()
}

/// 数据库行到 [`ObjectRecord`] 的中间形态。
#[derive(sqlx::FromRow)]
struct ObjectRow {
    root_id: Vec<u8>,
    hash: Vec<u8>,
    size: i64,
    storage_path: String,
    verified_at: i64,
    gc_state: String,
}

impl ObjectRow {
    fn into_record(self) -> Result<ObjectRecord, VersionError> {
        Ok(ObjectRecord {
            root: identity(self.root_id, "root_id", RootId::from_bytes)?,
            hash: content_hash(self.hash)?,
            size: size_of(self.size, "size")?,
            storage_path: self.storage_path,
            verified_at: UtcTimestamp::from_db_integer(self.verified_at).map_err(|_| {
                VersionError::MalformedRow {
                    column: "verified_at",
                }
            })?,
            gc_state: ObjectGcState::from_text(&self.gc_state)
                .ok_or(VersionError::MalformedRow { column: "gc_state" })?,
        })
    }
}

/// 数据库行到 [`VersionRecord`] 的中间形态。
#[derive(sqlx::FromRow)]
struct VersionRow {
    id: Vec<u8>,
    root_id: Vec<u8>,
    entry_id: Vec<u8>,
    object_hash: Vec<u8>,
    size: i64,
    created_seq: i64,
    superseded_at: Option<i64>,
    origin: String,
}

impl VersionRow {
    fn into_record(self) -> Result<VersionRecord, VersionError> {
        Ok(VersionRecord {
            id: identity(self.id, "id", VersionId::from_bytes)?,
            root: identity(self.root_id, "root_id", RootId::from_bytes)?,
            entry: identity(self.entry_id, "entry_id", EntryId::from_bytes)?,
            object_hash: content_hash(self.object_hash)?,
            size: size_of(self.size, "size")?,
            created_seq: RootSeq::from_db_integer(self.created_seq).map_err(|_| {
                VersionError::MalformedRow {
                    column: "created_seq",
                }
            })?,
            superseded_at: self
                .superseded_at
                .map(|seconds| {
                    UtcTimestamp::from_db_integer(seconds).map_err(|_| VersionError::MalformedRow {
                        column: "superseded_at",
                    })
                })
                .transpose()?,
            origin: VersionOrigin::from_text(&self.origin)
                .ok_or(VersionError::MalformedRow { column: "origin" })?,
        })
    }
}

/// 已登记对象的长度与存储路径：幂等登记与版本长度核对只需要这两列。
struct ObjectFacts {
    size: u64,
    storage_path: String,
    gc_state: String,
}

/// 读已登记对象的长度与路径；未登记返回 `None`。
async fn object_facts(
    conn: &mut SqliteConnection,
    root: RootId,
    hash: &ObjectHash,
) -> Result<Option<ObjectFacts>, VersionError> {
    let row: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT size, storage_path, gc_state FROM objects WHERE root_id = ?1 AND hash = ?2",
    )
    .bind(root.as_bytes().as_slice())
    .bind(hash.as_slice())
    .fetch_optional(conn)
    .await
    .map_err(VersionError::Db)?;
    row.map(|(size, storage_path, gc_state)| {
        Ok(ObjectFacts {
            size: size_of(size, "size")?,
            storage_path,
            gc_state,
        })
    })
    .transpose()
}

/// 确认 Root 属于调用方的 owner scope。
async fn require_root_owner(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
) -> Result<(), VersionError> {
    let found: Option<Vec<u8>> = sqlx::query_scalar("SELECT owner_id FROM roots WHERE id = ?1")
        .bind(root.as_bytes().as_slice())
        .fetch_optional(conn)
        .await
        .map_err(VersionError::Db)?;
    match found {
        Some(bytes) if bytes.as_slice() == owner.as_bytes() => Ok(()),
        _ => Err(VersionError::OutOfScope),
    }
}

/// 确认条目存在、活跃、属于该 Root 且是文件。
async fn require_active_file(
    conn: &mut SqliteConnection,
    root: RootId,
    entry: EntryId,
) -> Result<(), VersionError> {
    let row: Option<(Vec<u8>, String, Option<i64>)> =
        sqlx::query_as("SELECT root_id, kind, deleted_at FROM entries WHERE id = ?1")
            .bind(entry.as_bytes().as_slice())
            .fetch_optional(conn)
            .await
            .map_err(VersionError::Db)?;
    let Some((entry_root, kind, deleted_at)) = row else {
        return Err(VersionError::EntryNotFound { id: entry });
    };
    if entry_root.as_slice() != root.as_bytes() || deleted_at.is_some() {
        return Err(VersionError::EntryNotFound { id: entry });
    }
    match EntryKind::from_text(&kind) {
        Some(EntryKind::File) => Ok(()),
        Some(EntryKind::Directory) => Err(VersionError::NotAFile { id: entry }),
        None => Err(VersionError::MalformedRow { column: "kind" }),
    }
}

/// 确认批次序号在该 Root 的 `changes` 里存在。
async fn require_recorded_seq(
    conn: &mut SqliteConnection,
    root: RootId,
    seq: RootSeq,
) -> Result<(), VersionError> {
    let found: Option<i64> =
        sqlx::query_scalar("SELECT seq FROM changes WHERE root_id = ?1 AND seq = ?2")
            .bind(root.as_bytes().as_slice())
            .bind(seq.to_db_integer())
            .fetch_optional(conn)
            .await
            .map_err(VersionError::Db)?;
    match found {
        Some(_) => Ok(()),
        None => Err(VersionError::SeqNotRecorded { seq }),
    }
}

/// 存储路径的形式检查：对象必须有位置（迁移里的 CHECK 也会挡住空值）。
fn require_storage_path(path: &str) -> Result<(), VersionError> {
    if path.is_empty() {
        return Err(VersionError::EmptyStoragePath);
    }
    Ok(())
}

/// 长度落库前的范围检查：SQLite `INTEGER` 是有符号 64 位。
fn db_size(size: u64) -> Result<i64, VersionError> {
    i64::try_from(size).map_err(|_| VersionError::SizeAboveDbLimit { size })
}

/// 由数据库整数还原长度；负值按坏行处理。
fn size_of(value: i64, column: &'static str) -> Result<u64, VersionError> {
    u64::try_from(value).map_err(|_| VersionError::MalformedRow { column })
}

/// 内容哈希列长度检查：必须是恰好 32 字节。
fn content_hash(bytes: Vec<u8>) -> Result<ObjectHash, VersionError> {
    let array: ObjectHash = bytes
        .as_slice()
        .try_into()
        .map_err(|_| VersionError::MalformedRow { column: "hash" })?;
    Ok(array)
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; ID_LEN]) -> T,
) -> Result<T, VersionError> {
    let array: [u8; ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| VersionError::MalformedRow { column })?;
    Ok(wrap(array))
}

/// 十六进制文本，仅用于错误信息（不参与任何判定）。
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from_digit(u32::from(byte >> 4), 16).expect("半字节是十六进制数字"));
        text.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("半字节是十六进制数字"));
    }
    text
}

/// 把对象插入语句的约束错误翻译成带上下文的类型化错误。
fn classify_object_error(error: sqlx::Error, new: &NewObject) -> VersionError {
    let Some(database_error) = error.as_database_error() else {
        return VersionError::Db(error);
    };
    let message = database_error.message();
    if !message.starts_with("UNIQUE constraint failed") {
        return VersionError::Db(error);
    }
    if message.contains("objects.storage_path") {
        return VersionError::DuplicateStoragePath {
            path: new.storage_path.clone(),
        };
    }
    VersionError::ObjectConflict {
        hash: new.hash,
        reason: "同一内容地址已有对象行",
    }
}

/// 把版本插入语句的约束错误翻译成带上下文的类型化错误。
fn classify_version_error(error: sqlx::Error, new: &NewVersion) -> VersionError {
    let Some(database_error) = error.as_database_error() else {
        return VersionError::Db(error);
    };
    let message = database_error.message();
    if !message.starts_with("UNIQUE constraint failed") {
        return VersionError::Db(error);
    }
    if message.contains("versions.id") {
        return VersionError::DuplicateId { id: new.id };
    }
    // 剩下的唯一性来源是“每个条目至多一个当前版本”的部分唯一索引。
    VersionError::CurrentVersionConflict { entry: new.entry }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::changes::{self, ChangeBatch};
    use crate::db::Database;
    use crate::entries;
    use crate::test_support::{
        TestDir, open_database, register_root, seed_file, seed_owner, tx as tx_id, user,
    };

    fn at(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_seconds(seconds).expect("固定时间戳必须合法")
    }

    fn hash(marker: u8) -> ObjectHash {
        [marker; OBJECT_HASH_LEN]
    }

    fn version(marker: u8) -> VersionId {
        VersionId::from_bytes([marker; ID_LEN])
    }

    const EVENTS: &str = r#"[{"kind":"content_put","entry":"01"}]"#;

    /// 在一个事务里追加批次并记录对象+版本，返回 (批次, 版本记录)。
    async fn commit_version(
        database: &Database,
        owner: UserId,
        root: RootId,
        new: NewVersion,
    ) -> Result<(ChangeBatch, VersionRecord), VersionError> {
        let mut tx = database.begin_write().await.expect("开始写事务");
        let batch = changes::append_change(&mut tx, owner, root, tx_id(0x31), EVENTS, new.at)
            .await
            .expect("追加批次");
        let recorded = record_version(
            &mut tx,
            owner,
            &NewVersion {
                created_seq: batch.seq,
                ..new
            },
        )
        .await?;
        tx.commit().await.expect("提交");
        Ok((batch, recorded))
    }

    async fn record_object_ok(
        database: &Database,
        owner: UserId,
        root: RootId,
        marker: u8,
        size: u64,
    ) -> ObjectRecord {
        let mut tx = database.begin_write().await.expect("开始写事务");
        let record = record_object(
            &mut tx,
            owner,
            &NewObject {
                root,
                hash: hash(marker),
                size,
                storage_path: format!("/srv/objects/{marker:02x}"),
                verified_at: at(9),
            },
        )
        .await
        .expect("记录内容对象");
        tx.commit().await.expect("提交");
        record
    }

    #[tokio::test]
    async fn recording_the_same_object_twice_is_idempotent_but_a_different_content_is_not() {
        let dir = TestDir::new("versions-object");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, _root_entry) = register_root(&database, owner, 0x21).await;

        let first = record_object_ok(&database, owner, root, 0x41, 12).await;
        assert_eq!(first.size, 12);
        assert_eq!(first.gc_state, ObjectGcState::Live);
        assert_eq!(first.verified_at, at(9));

        // 相同 (root, hash) + 相同长度 + 相同路径：幂等，返回库里的记录。
        let again = record_object_ok(&database, owner, root, 0x41, 12).await;
        assert_eq!(again, first);
        let mut conn = database.pool().acquire().await.expect("取连接");
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM objects")
            .fetch_one(&mut *conn)
            .await
            .expect("数对象");
        assert_eq!(rows, 1, "同一内容只应有一行");

        // 同一内容地址、不同长度：拒绝。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = record_object(
            &mut tx,
            owner,
            &NewObject {
                root,
                hash: hash(0x41),
                size: 13,
                storage_path: "/srv/objects/41".to_owned(),
                verified_at: at(9),
            },
        )
        .await
        .expect_err("同一内容地址不能有两种长度");
        assert!(
            matches!(error, VersionError::ObjectConflict { reason, .. } if reason.contains("长度")),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 同一内容地址、不同路径：拒绝。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = record_object(
            &mut tx,
            owner,
            &NewObject {
                root,
                hash: hash(0x41),
                size: 12,
                storage_path: "/srv/objects/other".to_owned(),
                verified_at: at(9),
            },
        )
        .await
        .expect_err("同一内容地址不能有两种路径");
        assert!(
            matches!(error, VersionError::ObjectConflict { reason, .. } if reason.contains("路径")),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 另一个对象占用同一路径：拒绝（按 Root 的 GC 不能删掉别人的字节）。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = record_object(
            &mut tx,
            owner,
            &NewObject {
                root,
                hash: hash(0x42),
                size: 5,
                storage_path: "/srv/objects/41".to_owned(),
                verified_at: at(9),
            },
        )
        .await
        .expect_err("同一路径不能被两个对象占用");
        assert!(
            matches!(error, VersionError::DuplicateStoragePath { .. }),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 空路径与超出数据库范围的长度都在写库前拒绝。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = record_object(
            &mut tx,
            owner,
            &NewObject {
                root,
                hash: hash(0x43),
                size: 1,
                storage_path: String::new(),
                verified_at: at(9),
            },
        )
        .await
        .expect_err("空路径必须拒绝");
        assert!(
            matches!(error, VersionError::EmptyStoragePath),
            "实际错误 {error:?}"
        );
        let error = record_object(
            &mut tx,
            owner,
            &NewObject {
                root,
                hash: hash(0x43),
                size: u64::MAX,
                storage_path: "/srv/objects/43".to_owned(),
                verified_at: at(9),
            },
        )
        .await
        .expect_err("超出 i64 的长度必须拒绝");
        assert!(
            matches!(error, VersionError::SizeAboveDbLimit { size } if size == u64::MAX),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 别人的账号看不到、也写不了。
        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            object_in_scope(&mut conn, user(0x12), root, &hash(0x41))
                .await
                .expect("读对象"),
            None
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn a_new_version_becomes_current_and_supersedes_the_old_one() {
        let dir = TestDir::new("versions-current");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, root_entry) = register_root(&database, owner, 0x21).await;
        let file = seed_file(&database, owner, root, root_entry, 0x51, "note.txt").await;

        record_object_ok(&database, owner, root, 0x41, 12).await;
        record_object_ok(&database, owner, root, 0x42, 20).await;

        let (_first_batch, first) = commit_version(
            &database,
            owner,
            root,
            NewVersion {
                id: version(0x61),
                root,
                entry: file,
                object_hash: hash(0x41),
                size: 12,
                created_seq: RootSeq::ZERO,
                origin: VersionOrigin::Client,
                at: at(10),
            },
        )
        .await
        .expect("记录第一个版本");
        assert_eq!(first.created_seq, RootSeq::new(1).expect("seq"));
        assert_eq!(first.superseded_at, None);
        assert_eq!(first.origin, VersionOrigin::Client);

        // 条目的当前内容版本指针由版本写入负责。
        let mut conn = database.pool().acquire().await.expect("取连接");
        let entry_record = entries::entry_in_scope(&mut conn, owner, file)
            .await
            .expect("读条目")
            .expect("条目存在");
        assert_eq!(entry_record.content_version, Some(version(0x61)));
        assert_eq!(
            current_version(&mut conn, owner, root, file)
                .await
                .expect("读当前版本"),
            Some(first.clone())
        );
        drop(conn);

        let (_second_batch, second) = commit_version(
            &database,
            owner,
            root,
            NewVersion {
                id: version(0x62),
                root,
                entry: file,
                object_hash: hash(0x42),
                size: 20,
                created_seq: RootSeq::ZERO,
                origin: VersionOrigin::External,
                at: at(20),
            },
        )
        .await
        .expect("记录第二个版本");
        assert_eq!(second.created_seq, RootSeq::new(2).expect("seq"));
        assert_eq!(second.superseded_at, None);

        let mut conn = database.pool().acquire().await.expect("取连接");
        let current = current_version(&mut conn, owner, root, file)
            .await
            .expect("读当前版本")
            .expect("当前版本存在");
        assert_eq!(current, second);
        assert_eq!(
            entries::entry_in_scope(&mut conn, owner, file)
                .await
                .expect("读条目")
                .expect("条目存在")
                .content_version,
            Some(version(0x62))
        );

        // 历史保留旧版本，且旧行只剩“被取代的时间”是新的。
        let history = version_history(&mut conn, owner, root, file, 10)
            .await
            .expect("读历史");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0], second, "新的在前");
        assert_eq!(history[1].id, first.id);
        assert_eq!(history[1].object_hash, first.object_hash);
        assert_eq!(history[1].size, first.size);
        assert_eq!(history[1].created_seq, first.created_seq);
        assert_eq!(history[1].superseded_at, Some(at(20)));
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn versions_are_refused_without_persisted_content_a_recorded_batch_or_a_live_file() {
        let dir = TestDir::new("versions-reject");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, root_entry) = register_root(&database, owner, 0x21).await;
        let (other_root, other_entry) = register_root(&database, owner, 0x22).await;
        let file = seed_file(&database, owner, root, root_entry, 0x51, "note.txt").await;
        let other_file =
            seed_file(&database, owner, other_root, other_entry, 0x52, "other.txt").await;
        record_object_ok(&database, owner, root, 0x41, 12).await;
        let mut tx = database.begin_write().await.expect("开始写事务");
        let batch = changes::append_change(&mut tx, owner, root, tx_id(0x31), EVENTS, at(10))
            .await
            .expect("追加批次");
        tx.commit().await.expect("提交批次");

        let base = NewVersion {
            id: version(0x61),
            root,
            entry: file,
            object_hash: hash(0x41),
            size: 12,
            created_seq: batch.seq,
            origin: VersionOrigin::Client,
            at: at(11),
        };

        // 内容对象未持久化。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = record_version(
            &mut tx,
            owner,
            &NewVersion {
                object_hash: hash(0x99),
                ..base.clone()
            },
        )
        .await
        .expect_err("没有对象不能有版本");
        assert!(
            matches!(error, VersionError::UnknownObject { hash: actual } if actual == hash(0x99)),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 长度与对象不一致。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = record_version(
            &mut tx,
            owner,
            &NewVersion {
                size: 13,
                ..base.clone()
            },
        )
        .await
        .expect_err("长度必须与对象一致");
        assert!(
            matches!(
                error,
                VersionError::ContentSizeMismatch {
                    version_size: 13,
                    object_size: 12
                }
            ),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 批次序号不存在。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = record_version(
            &mut tx,
            owner,
            &NewVersion {
                created_seq: RootSeq::new(9).expect("seq"),
                ..base.clone()
            },
        )
        .await
        .expect_err("版本必须写在已记录的批次里");
        assert!(
            matches!(error, VersionError::SeqNotRecorded { seq } if seq == RootSeq::new(9).expect("seq")),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 目录没有内容版本。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = record_version(
            &mut tx,
            owner,
            &NewVersion {
                entry: root_entry,
                ..base.clone()
            },
        )
        .await
        .expect_err("目录不能有内容版本");
        assert!(
            matches!(error, VersionError::NotAFile { id } if id == root_entry),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 另一个 Root 的条目不能借用本 Root 的版本，别人的账号也写不了。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = record_version(
            &mut tx,
            owner,
            &NewVersion {
                entry: other_file,
                ..base.clone()
            },
        )
        .await
        .expect_err("跨 Root 的条目不能有本 Root 的版本");
        assert!(
            matches!(error, VersionError::EntryNotFound { .. }),
            "实际错误 {error:?}"
        );
        let error = record_version(
            &mut tx,
            user(0x12),
            &NewVersion {
                id: version(0x62),
                ..base.clone()
            },
        )
        .await
        .expect_err("别人的账号不能写版本");
        assert!(
            matches!(error, VersionError::OutOfScope),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 已软删除的条目不能再重建版本，历史仍然可读。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let recorded = record_version(&mut tx, owner, &base)
            .await
            .expect("记录版本");
        entries::soft_delete_entry(&mut tx, owner, file, at(30))
            .await
            .expect("软删除条目");
        tx.commit().await.expect("提交");
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = record_version(
            &mut tx,
            owner,
            &NewVersion {
                id: version(0x63),
                ..base.clone()
            },
        )
        .await
        .expect_err("已删除条目不能重建版本");
        assert!(
            matches!(error, VersionError::EntryNotFound { .. }),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            version_history(&mut conn, owner, root, file, 10)
                .await
                .expect("读已删除条目的历史"),
            vec![recorded]
        );
        assert!(
            version_history(&mut conn, user(0x12), root, file, 10)
                .await
                .is_err(),
            "别人的账号不得读历史"
        );
        for limit in [0, MAX_VERSION_PAGE + 1] {
            let error = version_history(&mut conn, owner, root, file, limit)
                .await
                .expect_err("越界的分页大小必须拒绝");
            assert!(matches!(error, VersionError::InvalidLimit), "limit={limit}");
        }
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn history_and_object_content_references_cannot_be_rewritten() {
        let dir = TestDir::new("versions-immutable");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, root_entry) = register_root(&database, owner, 0x21).await;
        let file = seed_file(&database, owner, root, root_entry, 0x51, "note.txt").await;
        // 第二个文件留给“绕过 Rust 直接写 SQL”的用例：它还没有当前版本，唯一索引不会先报错。
        let second = seed_file(&database, owner, root, root_entry, 0x52, "second.txt").await;
        record_object_ok(&database, owner, root, 0x41, 12).await;
        let (_batch, recorded) = commit_version(
            &database,
            owner,
            root,
            NewVersion {
                id: version(0x61),
                root,
                entry: file,
                object_hash: hash(0x41),
                size: 12,
                created_seq: RootSeq::ZERO,
                origin: VersionOrigin::Client,
                at: at(10),
            },
        )
        .await
        .expect("记录版本");

        // 已提交版本的内容引用不可改写（design I12）。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = sqlx::query("UPDATE versions SET size = 99 WHERE id = ?1")
            .bind(recorded.id.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .expect_err("历史行不可改写");
        assert!(error.to_string().contains("不可修改"), "实际错误 {error:?}");
        // 对象的内容地址证据同理。
        let error = sqlx::query("UPDATE objects SET size = 99 WHERE hash = ?1")
            .bind(hash(0x41).as_slice())
            .execute(&mut *tx)
            .await
            .expect_err("对象的内容引用不可改写");
        assert!(error.to_string().contains("不可修改"), "实际错误 {error:?}");
        // 唯一允许的更新是 superseded_at。
        sqlx::query("UPDATE versions SET superseded_at = ?1 WHERE id = ?2")
            .bind(at(40).to_db_integer())
            .bind(recorded.id.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .expect("取代时间可以更新");
        tx.rollback().await.expect("回滚");

        // 绕过 Rust 直接写一个没有对象也没有批次的版本：外键本身必须挡住。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = sqlx::query(
            "INSERT INTO versions \
             (id, root_id, entry_id, object_hash, size, created_seq, superseded_at, origin) \
             VALUES (?1, ?2, ?3, ?4, 12, 7, NULL, 'client')",
        )
        .bind(version(0x62).as_bytes().as_slice())
        .bind(root.as_bytes().as_slice())
        .bind(second.as_bytes().as_slice())
        .bind(hash(0x99).as_slice())
        .execute(&mut *tx)
        .await
        .expect_err("没有对象与批次的版本必须被外键拒绝");
        assert!(
            error.to_string().contains("FOREIGN KEY"),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");
        database.close().await;
    }
}

/// 一个版本的完整事实（恢复与历史展示用）：内容、批次与批次提交时间。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionFacts {
    /// 所属条目。
    pub entry: EntryId,
    /// 内容哈希。
    pub object_hash: ObjectHash,
    /// 内容字节数。
    pub size: u64,
    /// 记录该版本的批次。
    pub created_seq: RootSeq,
    /// 该批次的提交时间（版本创建时间）。
    pub created_at: UtcTimestamp,
    /// 被取代的时间；`None` 表示它仍是当前版本。
    pub superseded_at: Option<UtcTimestamp>,
}

/// 按版本身份读一条版本事实（含所属批次时间）；不存在或越权返回 `None`。
///
/// # Errors
///
/// 存储失败或行无法还原。
pub async fn version_facts(
    conn: &mut sqlx::SqliteConnection,
    owner: UserId,
    root: RootId,
    version: VersionId,
) -> Result<Option<VersionFacts>, VersionError> {
    let row = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, i64, i64, i64, Option<i64>)>(
        "SELECT v.entry_id, v.object_hash, v.size, v.created_seq, c.created_at, v.superseded_at \
         FROM versions v JOIN roots r ON r.id = v.root_id \
         JOIN changes c ON c.root_id = v.root_id AND c.seq = v.created_seq \
         WHERE v.root_id = ?1 AND v.id = ?2 AND r.owner_id = ?3",
    )
    .bind(root.as_bytes().as_slice())
    .bind(version.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await
    .map_err(VersionError::Db)?;
    let Some((entry, hash, size, created_seq, created_at, superseded_at)) = row else {
        return Ok(None);
    };
    let entry = EntryId::from_bytes(
        entry
            .as_slice()
            .try_into()
            .map_err(|_| VersionError::MalformedRow { column: "entry_id" })?,
    );
    let hash: ObjectHash = hash
        .as_slice()
        .try_into()
        .map_err(|_| VersionError::MalformedRow {
            column: "object_hash",
        })?;
    let size = u64::try_from(size).map_err(|_| VersionError::MalformedRow { column: "size" })?;
    let created_at =
        UtcTimestamp::from_db_integer(created_at).map_err(|_| VersionError::MalformedRow {
            column: "created_at",
        })?;
    let superseded_at = superseded_at
        .map(UtcTimestamp::from_db_integer)
        .transpose()
        .map_err(|_| VersionError::MalformedRow {
            column: "superseded_at",
        })?;
    Ok(Some(VersionFacts {
        entry,
        object_hash: hash,
        size,
        created_seq: RootSeq::from_db_integer(created_seq).map_err(|_| {
            VersionError::MalformedRow {
                column: "created_seq",
            }
        })?,
        created_at,
        superseded_at,
    }))
}
