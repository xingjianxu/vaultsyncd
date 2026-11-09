//! 上传会话与固定块片段（`uploads`/`upload_parts` 表）（`docs/design.md` §7.2、§13.1—13.2、§19.4）。
//!
//! 边界：本模块只记录“哪个会话为哪个 Vault 的哪份不可变内容快照收了哪些块”。它不接收字节、不算哈希、
//! 不碰暂存文件、不发布内容对象，也不写 HTTP——流式接收、载荷上限与对象固定分别属于 S045 与 S033。
//!
//! 不变量：
//!
//! - **会话绑定一份不可变快照**（design §13.1）：`expected_size` 与总 `hash` 在创建会话时确定，
//!   之后不可改写。块属于同一份快照，因此“同一索引重传但哈希不同”被拒绝（[`UploadError::PartConflict`]），
//!   不能把两个本地 generation 的字节混进一个会话。
//! - **索引唯一、重复内容才幂等**（design §7.2）：`(upload_id, part_index)` 是主键，重复上传同一索引
//!   永远只保留一行；内容（长度、哈希、路径）完全一致时报 [`PartOutcome::AlreadyPresent`]，否则报
//!   `PartConflict`，旧行不被覆盖。
//! - **块几何由会话决定**：[`ChunkGeometry`] 由 `expected_size` 与 `chunk_size` 算出块数与每块期望长度，
//!   越界索引与长度不符的块一律拒绝。空内容（`expected_size = 0`）合法且没有块（design §13.1）。
//! - **未齐不上报完整**（design I05）：[`mark_upload_complete`] 要求没有缺失块；[`mark_upload_finalized`]
//!   只接受 `complete` 状态。`complete` 不是“内容已验证成版本”，后者要等 finalize 校验总哈希。
//! - **状态由数据库给出**（design §13.2）：续传依据是库里的已收块与状态，不采信客户端位图；
//!   `open`/`complete` 是未结束，`finalized`/`aborted` 是终态，终态不再接受块也不再改状态。
//! - **owner scope**：所有读写都带 [`UserId`]，经 `vaults.owner_id` 判定；越权上传表现为
//!   [`UploadError::OutOfScope`] 或 `None`，不返回空结果。
//!
//! 会话身份（`session_id`）只做长度校验：`sessions` 表由会话卡（S043）建立，本卡不建它，也不假装
//! 校验会话有效性（design §13.2 的“校验会话仍有效”由 API 层在调用本模块前完成）。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{ID_LEN, SessionId, UploadId, UserId, VaultId};
use vaultsyncd_domain::version::UtcTimestamp;

use crate::versions::ObjectHash;

/// `uploads` 表的列顺序，读取点共用。
const UPLOAD_COLUMNS: &str = "u.id, u.vault_id, u.session_id, u.expected_size, u.hash, \
                              u.chunk_size, u.expires_at, u.state";

/// `upload_parts` 表的列顺序，读取点共用。
const PART_COLUMNS: &str = "part_index, length, hash, path";

/// 一个上传会话的块几何：快照总大小与块大小（design §13.1、§15.2）。
///
/// 块数 = `ceil(total_size / chunk_size)`；每块长度等于 `chunk_size`，最后一块是其剩余长度。
/// 空内容（`total_size = 0`）有 0 块，因此不需要零长度块来表达“空文件”。
///
/// 算术本身由 `vaultsyncd_transfer::chunk::ChunkGeometry` 提供（design §5.4 的 `crates/transfer`）：
/// 客户端上传与服务端接收必须按同一份定义切块，因此本类型只是它在存储层的薄封装——`new` 把
/// 几何错误映射成存储层的 [`UploadError::InvalidChunkSize`]，不再另写一遍除法。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkGeometry {
    inner: vaultsyncd_transfer::chunk::ChunkGeometry,
}

impl ChunkGeometry {
    /// 由总大小与块大小构造；块大小为 0 时返回 [`UploadError::InvalidChunkSize`]。
    pub fn new(total_size: u64, chunk_size: u64) -> Result<Self, UploadError> {
        vaultsyncd_transfer::chunk::ChunkGeometry::new(total_size, chunk_size)
            .map(|inner| Self { inner })
            .map_err(|_| UploadError::InvalidChunkSize)
    }

    /// 快照总大小（字节）。
    pub fn total_size(self) -> u64 {
        self.inner.total_size()
    }

    /// 块大小（字节）。
    pub fn chunk_size(self) -> u64 {
        self.inner.chunk_size()
    }

    /// 块数：完整块与最后一个不足块合计；空内容为 0。
    pub fn part_count(self) -> u64 {
        self.inner.part_count()
    }

    /// 第 `index` 块的期望长度；索引大于等于块数时返回 `None`。
    pub fn part_length(self, index: u64) -> Option<u64> {
        self.inner.part_length(index)
    }
}

/// 上传会话的状态（`uploads.state`）。
///
/// 转换：`open → complete | aborted`；`complete → finalized | aborted`；`finalized`/`aborted` 是终态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadState {
    /// 接收块中。
    Open,
    /// 全部块齐备且逐块核对通过，等待 finalize 组装并校验总哈希（design §11.2 步骤 2）。
    Complete,
    /// 已发布为内容对象（终态）。
    Finalized,
    /// 已放弃或会话过期（终态）；已收块作为待清理证据保留。
    Aborted,
}

impl UploadState {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Complete => "complete",
            Self::Finalized => "finalized",
            Self::Aborted => "aborted",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜状态。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "open" => Some(Self::Open),
            "complete" => Some(Self::Complete),
            "finalized" => Some(Self::Finalized),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }

    /// 是否终态（不再接受块，也不再改状态）。
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Finalized | Self::Aborted)
    }

    /// 允许的状态转换；其余组合一律拒绝（design §13.2 的上传流程顺序）。
    pub const fn allows_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Open, Self::Complete)
                | (Self::Open, Self::Aborted)
                | (Self::Complete, Self::Finalized)
                | (Self::Complete, Self::Aborted)
        )
    }
}

/// 创建一个上传会话的输入。
///
/// 不变量（违反时 [`begin_upload`] 报错且不写入）：Vault 属于调用方；`chunk_size` 大于 0；
/// `expected_size` 与 `chunk_size` 不超过 SQLite `INTEGER` 上限；`id` 未被占用。
#[derive(Clone, Debug)]
pub struct NewUpload {
    /// 上传会话身份。
    pub id: UploadId,
    /// 所属 Vault。
    pub vault: VaultId,
    /// 签发上传的登录会话（本层只存身份，不校验有效性）。
    pub session: SessionId,
    /// 快照总大小。
    pub expected_size: u64,
    /// 快照总 SHA-256。
    pub hash: ObjectHash,
    /// 块大小（会话的块几何）。
    pub chunk_size: u64,
    /// 会话过期时间；过期后未完成的上传可按会话清理（design §19.4）。
    pub expires_at: UtcTimestamp,
}

/// 从数据库读出的一条 `uploads` 记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadRecord {
    /// 上传会话身份。
    pub id: UploadId,
    /// 所属 Vault。
    pub vault: VaultId,
    /// 签发上传的登录会话。
    pub session: SessionId,
    /// 快照总大小。
    pub expected_size: u64,
    /// 快照总 SHA-256。
    pub hash: ObjectHash,
    /// 块大小。
    pub chunk_size: u64,
    /// 会话过期时间。
    pub expires_at: UtcTimestamp,
    /// 状态。
    pub state: UploadState,
}

impl UploadRecord {
    /// 本会话的块几何；`chunk_size` 已在读库时校验为正，因此这里只可能报格式错误。
    pub fn geometry(&self) -> Result<ChunkGeometry, UploadError> {
        ChunkGeometry::new(self.expected_size, self.chunk_size)
    }
}

/// 一个已收到的块（`upload_parts` 一行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadPart {
    /// 块索引，从 0 起。
    pub index: u64,
    /// 块长度（与块几何一致）。
    pub length: u64,
    /// 块 SHA-256。
    pub hash: ObjectHash,
    /// 暂存片段文件位置。
    pub path: String,
}

/// 记录一个块的输入。
///
/// 不变量（违反时 [`record_part`] 报错且不写入）：上传未结束；索引小于块数；`length` 等于块几何给出的
/// 期望长度；`path` 非空；同一索引若已有记录，必须三者完全一致才算幂等重传。
#[derive(Clone, Debug)]
pub struct NewUploadPart {
    /// 块索引，从 0 起。
    pub index: u64,
    /// 块长度。
    pub length: u64,
    /// 块 SHA-256。
    pub hash: ObjectHash,
    /// 暂存片段文件位置。
    pub path: String,
}

/// [`record_part`] 的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartOutcome {
    /// 首次收到该索引的块。
    Recorded(UploadPart),
    /// 同一索引、同一内容重传：幂等，不新增行，返回库里已有的记录。
    AlreadyPresent(UploadPart),
}

/// 一段连续缺失的块索引：`first_index` 起 `count` 块。
///
/// 缺块用区间而不是逐块表示，因此列表长度最多是“已收块数 + 1”，不随文件大小增长。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MissingRange {
    /// 区间起点（含）。
    pub first_index: u64,
    /// 区间内的块数，至少为 1。
    pub count: u64,
}

/// 续传所需的会话进度（design §13.2「查询已收块」）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadProgress {
    /// 会话记录（含状态与块几何）。
    pub upload: UploadRecord,
    /// 已收到的块，按索引升序。
    pub received: Vec<UploadPart>,
    /// 仍缺失的块区间，按索引升序；空表示块已齐。
    pub missing: Vec<MissingRange>,
}

impl UploadProgress {
    /// 是否所有块都已收到（即块几何覆盖完整内容）。
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }

    /// 仍缺失的块数。
    pub fn missing_part_count(&self) -> u64 {
        self.missing.iter().map(|range| range.count).sum()
    }
}

/// 上传会话与片段的读写失败。
#[derive(Debug)]
pub enum UploadError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// Vault 不存在或不属于调用方的 owner scope（两种情况不区分）。
    OutOfScope,
    /// 上传会话不存在或不在调用方的 owner scope 内。
    UnknownUpload {
        /// 请求的上传会话。
        id: UploadId,
    },
    /// 上传会话身份已被占用。
    DuplicateId {
        /// 冲突的上传会话。
        id: UploadId,
    },
    /// 块大小为 0。
    InvalidChunkSize,
    /// 长度或块大小超过 SQLite `INTEGER` 上限（`u64` 里更大的值落库会被截断）。
    SizeAboveDbLimit {
        /// 越界的字节数。
        size: u64,
    },
    /// 会话状态不允许这次转换。
    IllegalTransition {
        /// 当前状态。
        from: UploadState,
        /// 请求的目标状态。
        to: UploadState,
    },
    /// 会话已不再接收新块（`complete`/`finalized`/`aborted`）。
    PartsClosed {
        /// 当前状态。
        state: UploadState,
    },
    /// 读到的状态与写入时不一致（本层写事务是 `BEGIN IMMEDIATE` 串行的，出现即说明绕过本模块写入）。
    StateConflict {
        /// 上传会话。
        id: UploadId,
    },
    /// 块已齐之前不能标记 complete。
    IncompleteUpload {
        /// 仍缺失的块数。
        missing_parts: u64,
    },
    /// 块索引超出块几何范围。
    PartOutOfRange {
        /// 给出的索引。
        index: u64,
        /// 会话的块数。
        part_count: u64,
    },
    /// 块长度与块几何给出的期望长度不一致。
    PartLengthMismatch {
        /// 块索引。
        index: u64,
        /// 期望长度。
        expected: u64,
        /// 实际长度。
        actual: u64,
    },
    /// 同一索引重传了不同内容（design §13.1：不能混合两个本地 generation 的字节）。
    PartConflict {
        /// 块索引。
        index: u64,
        /// 哪一项不一致。
        reason: &'static str,
    },
    /// 块路径为空。
    EmptyPartPath,
    /// 块路径已被另一行占用。
    DuplicatePartPath {
        /// 冲突的路径。
        path: String,
    },
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::OutOfScope => f.write_str("Vault 不存在或不在调用方的 owner scope 内"),
            Self::UnknownUpload { id } => write!(f, "上传会话 {id} 不存在"),
            Self::DuplicateId { id } => write!(f, "上传会话身份 {id} 已被占用"),
            Self::InvalidChunkSize => f.write_str("块大小必须大于 0"),
            Self::SizeAboveDbLimit { size } => {
                write!(f, "字节数 {size} 超过数据库有符号 64 位上限")
            }
            Self::IllegalTransition { from, to } => {
                write!(f, "上传会话不能从 {} 转到 {}", from.as_text(), to.as_text())
            }
            Self::PartsClosed { state } => {
                write!(f, "上传会话已是 {}，不再接收新块", state.as_text())
            }
            Self::StateConflict { id } => {
                write!(f, "上传会话 {id} 的状态在读取后又被改动")
            }
            Self::IncompleteUpload { missing_parts } => {
                write!(f, "上传仍缺 {missing_parts} 个块")
            }
            Self::PartOutOfRange { index, part_count } => {
                write!(f, "块索引 {index} 超出块数 {part_count}")
            }
            Self::PartLengthMismatch {
                index,
                expected,
                actual,
            } => write!(f, "块 {index} 长度应为 {expected}，实际 {actual}"),
            Self::PartConflict { index, reason } => {
                write!(f, "块 {index} 已用不同内容登记过：{reason}")
            }
            Self::EmptyPartPath => f.write_str("块路径不能为空"),
            Self::DuplicatePartPath { path } => write!(f, "块路径 {path:?} 已被占用"),
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
        }
    }
}

impl std::error::Error for UploadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            _ => None,
        }
    }
}

/// 创建一个上传会话。
pub async fn begin_upload(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    new: &NewUpload,
) -> Result<UploadRecord, UploadError> {
    require_vault_owner(tx, owner, new.vault).await?;
    let expected_size = db_size(new.expected_size)?;
    let chunk_size = db_size(new.chunk_size)?;
    ChunkGeometry::new(new.expected_size, new.chunk_size)?;

    let inserted = sqlx::query(
        "INSERT INTO uploads \
         (id, vault_id, session_id, expected_size, hash, chunk_size, expires_at, state) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(new.id.as_bytes().as_slice())
    .bind(new.vault.as_bytes().as_slice())
    .bind(new.session.as_bytes().as_slice())
    .bind(expected_size)
    .bind(new.hash.as_slice())
    .bind(chunk_size)
    .bind(new.expires_at.to_db_integer())
    .bind(UploadState::Open.as_text())
    .execute(&mut **tx)
    .await;
    if let Err(error) = inserted {
        return Err(classify_write_error(error, new.id));
    }
    upload_in_scope(tx, owner, new.id)
        .await?
        .ok_or(UploadError::OutOfScope)
}

/// 按身份读一个上传会话；不存在或不在 owner scope 内返回 `None`。
pub async fn upload_in_scope(
    conn: &mut SqliteConnection,
    owner: UserId,
    id: UploadId,
) -> Result<Option<UploadRecord>, UploadError> {
    let row = sqlx::query_as::<_, UploadRow>(&format!(
        "SELECT {UPLOAD_COLUMNS} FROM uploads u JOIN vaults r ON r.id = u.vault_id \
         WHERE u.id = ?1 AND r.owner_id = ?2"
    ))
    .bind(id.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(UploadError::Db)?;
    row.map(UploadRow::into_record).transpose()
}

/// 列出某个 Vault 下尚未结束的上传会话（`open`/`complete`），按过期时间与身份排序。
///
/// 这是重启后的续传入口：客户端凭会话身份与这里的真实已收块继续上传，而不是凭自己的位图。
pub async fn unfinished_uploads(
    conn: &mut SqliteConnection,
    owner: UserId,
    vault: VaultId,
) -> Result<Vec<UploadRecord>, UploadError> {
    require_vault_owner(conn, owner, vault).await?;
    let rows = sqlx::query_as::<_, UploadRow>(&format!(
        "SELECT {UPLOAD_COLUMNS} FROM uploads u WHERE u.vault_id = ?1 \
         AND u.state IN ('open', 'complete') ORDER BY u.expires_at, u.id"
    ))
    .bind(vault.as_bytes().as_slice())
    .fetch_all(conn)
    .await
    .map_err(UploadError::Db)?;
    rows.into_iter().map(UploadRow::into_record).collect()
}

/// 记录一个已持久化的块。
///
/// 同一索引的相同内容重传是幂等的（[`PartOutcome::AlreadyPresent`]）；内容不同的重传报
/// [`UploadError::PartConflict`]，旧行保持不动（design §13.1）。
pub async fn record_part(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    upload: UploadId,
    new: &NewUploadPart,
) -> Result<PartOutcome, UploadError> {
    if new.path.is_empty() {
        return Err(UploadError::EmptyPartPath);
    }
    let record = require_upload(tx, owner, upload).await?;
    if record.state.is_finished() {
        return Err(UploadError::PartsClosed {
            state: record.state,
        });
    }
    let geometry = record.geometry()?;
    let expected = geometry
        .part_length(new.index)
        .ok_or(UploadError::PartOutOfRange {
            index: new.index,
            part_count: geometry.part_count(),
        })?;
    if new.length != expected {
        return Err(UploadError::PartLengthMismatch {
            index: new.index,
            expected,
            actual: new.length,
        });
    }

    if let Some(existing) = part_in_upload(tx, upload, new.index).await? {
        if existing.length != new.length {
            return Err(UploadError::PartConflict {
                index: new.index,
                reason: "长度不同",
            });
        }
        if existing.hash != new.hash {
            return Err(UploadError::PartConflict {
                index: new.index,
                reason: "哈希不同",
            });
        }
        if existing.path != new.path {
            return Err(UploadError::PartConflict {
                index: new.index,
                reason: "暂存路径不同",
            });
        }
        return Ok(PartOutcome::AlreadyPresent(existing));
    }
    let inserted = sqlx::query(
        "INSERT INTO upload_parts (upload_id, part_index, length, hash, path) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(upload.as_bytes().as_slice())
    .bind(db_size(new.index)?)
    .bind(db_size(new.length)?)
    .bind(new.hash.as_slice())
    .bind(&new.path)
    .execute(&mut **tx)
    .await;
    if let Err(error) = inserted {
        return Err(classify_part_error(error, new));
    }
    let recorded = part_in_upload(tx, upload, new.index)
        .await?
        .ok_or(UploadError::UnknownUpload { id: upload })?;
    Ok(PartOutcome::Recorded(recorded))
}

/// 读会话的续传进度：会话记录 + 已收块 + 缺失块区间（design §13.2）。
pub async fn upload_progress(
    conn: &mut SqliteConnection,
    owner: UserId,
    upload: UploadId,
) -> Result<UploadProgress, UploadError> {
    let record = require_upload(conn, owner, upload).await?;
    let received = read_parts(conn, upload).await?;
    let geometry = record.geometry()?;
    let mut missing = Vec::new();
    let mut next = 0;
    for part in &received {
        if part.index > next {
            missing.push(MissingRange {
                first_index: next,
                count: part.index - next,
            });
        }
        next = part.index + 1;
    }
    let part_count = geometry.part_count();
    if next < part_count {
        missing.push(MissingRange {
            first_index: next,
            count: part_count - next,
        });
    }
    Ok(UploadProgress {
        upload: record,
        received,
        missing,
    })
}

/// 读一个会话已收到的块，按索引升序。
pub async fn parts_of(
    conn: &mut SqliteConnection,
    owner: UserId,
    upload: UploadId,
) -> Result<Vec<UploadPart>, UploadError> {
    require_upload(conn, owner, upload).await?;
    read_parts(conn, upload).await
}

/// 读会话的块行，不做 owner scope 检查（调用方已核对）。
async fn read_parts(
    conn: &mut SqliteConnection,
    upload: UploadId,
) -> Result<Vec<UploadPart>, UploadError> {
    let rows = sqlx::query_as::<_, PartRow>(&format!(
        "SELECT {PART_COLUMNS} FROM upload_parts WHERE upload_id = ?1 ORDER BY part_index"
    ))
    .bind(upload.as_bytes().as_slice())
    .fetch_all(conn)
    .await
    .map_err(UploadError::Db)?;
    rows.into_iter().map(PartRow::into_part).collect()
}

/// 标记会话的块已齐：要求没有缺失块，之后只能 finalize 或 abort。
pub async fn mark_upload_complete(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    upload: UploadId,
) -> Result<UploadRecord, UploadError> {
    let record = require_upload(tx, owner, upload).await?;
    let progress = upload_progress(tx, owner, upload).await?;
    if !progress.is_complete() {
        return Err(UploadError::IncompleteUpload {
            missing_parts: progress.missing_part_count(),
        });
    }
    transition(tx, owner, &record, UploadState::Complete).await
}

/// 标记会话已发布为内容对象（终态）；只接受块已齐的会话（design I05：不完整内容不可见为完成）。
pub async fn mark_upload_finalized(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    upload: UploadId,
) -> Result<UploadRecord, UploadError> {
    let record = require_upload(tx, owner, upload).await?;
    transition(tx, owner, &record, UploadState::Finalized).await
}

/// 放弃会话（终态）：客户端取消或会话过期（design §19.4）。已收块行保留，作为清理证据。
pub async fn abort_upload(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    upload: UploadId,
) -> Result<UploadRecord, UploadError> {
    let record = require_upload(tx, owner, upload).await?;
    transition(tx, owner, &record, UploadState::Aborted).await
}

/// 按转换规则改状态；读与写之间的状态变化用比较并写发现，不盲目覆盖。
async fn transition(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    record: &UploadRecord,
    to: UploadState,
) -> Result<UploadRecord, UploadError> {
    if !record.state.allows_transition_to(to) {
        return Err(UploadError::IllegalTransition {
            from: record.state,
            to,
        });
    }
    let updated = sqlx::query("UPDATE uploads SET state = ?1 WHERE id = ?2 AND state = ?3")
        .bind(to.as_text())
        .bind(record.id.as_bytes().as_slice())
        .bind(record.state.as_text())
        .execute(&mut **tx)
        .await
        .map_err(UploadError::Db)?;
    if updated.rows_affected() != 1 {
        return Err(UploadError::StateConflict { id: record.id });
    }
    upload_in_scope(tx, owner, record.id)
        .await?
        .ok_or(UploadError::UnknownUpload { id: record.id })
}

/// 读一条必须存在的上传记录。
async fn require_upload(
    conn: &mut SqliteConnection,
    owner: UserId,
    id: UploadId,
) -> Result<UploadRecord, UploadError> {
    upload_in_scope(conn, owner, id)
        .await?
        .ok_or(UploadError::UnknownUpload { id })
}

/// 读一个会话里某个索引的块。
async fn part_in_upload(
    conn: &mut SqliteConnection,
    upload: UploadId,
    index: u64,
) -> Result<Option<UploadPart>, UploadError> {
    let row = sqlx::query_as::<_, PartRow>(&format!(
        "SELECT {PART_COLUMNS} FROM upload_parts WHERE upload_id = ?1 AND part_index = ?2"
    ))
    .bind(upload.as_bytes().as_slice())
    .bind(db_size(index)?)
    .fetch_optional(conn)
    .await
    .map_err(UploadError::Db)?;
    row.map(PartRow::into_part).transpose()
}

/// 确认 Vault 属于调用方的 owner scope。
async fn require_vault_owner(
    conn: &mut SqliteConnection,
    owner: UserId,
    vault: VaultId,
) -> Result<(), UploadError> {
    let found: Option<Vec<u8>> = sqlx::query_scalar("SELECT owner_id FROM vaults WHERE id = ?1")
        .bind(vault.as_bytes().as_slice())
        .fetch_optional(conn)
        .await
        .map_err(UploadError::Db)?;
    match found {
        Some(bytes) if bytes.as_slice() == owner.as_bytes() => Ok(()),
        _ => Err(UploadError::OutOfScope),
    }
}

/// 长度落库前的范围检查：SQLite `INTEGER` 是有符号 64 位。
fn db_size(size: u64) -> Result<i64, UploadError> {
    i64::try_from(size).map_err(|_| UploadError::SizeAboveDbLimit { size })
}

/// 由数据库整数还原字节数；负值按坏行处理。
fn size_of(value: i64, column: &'static str) -> Result<u64, UploadError> {
    u64::try_from(value).map_err(|_| UploadError::MalformedRow { column })
}

/// 内容哈希列长度检查：必须是恰好 32 字节。
fn content_hash(bytes: Vec<u8>) -> Result<ObjectHash, UploadError> {
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| UploadError::MalformedRow { column: "hash" })
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; ID_LEN]) -> T,
) -> Result<T, UploadError> {
    let array: [u8; ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| UploadError::MalformedRow { column })?;
    Ok(wrap(array))
}

/// 数据库行到 [`UploadRecord`] 的中间形态。
#[derive(sqlx::FromRow)]
struct UploadRow {
    id: Vec<u8>,
    vault_id: Vec<u8>,
    session_id: Vec<u8>,
    expected_size: i64,
    hash: Vec<u8>,
    chunk_size: i64,
    expires_at: i64,
    state: String,
}

impl UploadRow {
    fn into_record(self) -> Result<UploadRecord, UploadError> {
        let chunk_size = size_of(self.chunk_size, "chunk_size")?;
        if chunk_size == 0 {
            return Err(UploadError::MalformedRow {
                column: "chunk_size",
            });
        }
        Ok(UploadRecord {
            id: identity(self.id, "id", UploadId::from_bytes)?,
            vault: identity(self.vault_id, "vault_id", VaultId::from_bytes)?,
            session: identity(self.session_id, "session_id", SessionId::from_bytes)?,
            expected_size: size_of(self.expected_size, "expected_size")?,
            hash: content_hash(self.hash)?,
            chunk_size,
            expires_at: UtcTimestamp::from_db_integer(self.expires_at).map_err(|_| {
                UploadError::MalformedRow {
                    column: "expires_at",
                }
            })?,
            state: UploadState::from_text(&self.state)
                .ok_or(UploadError::MalformedRow { column: "state" })?,
        })
    }
}

/// 数据库行到 [`UploadPart`] 的中间形态。
#[derive(sqlx::FromRow)]
struct PartRow {
    part_index: i64,
    length: i64,
    hash: Vec<u8>,
    path: String,
}

impl PartRow {
    fn into_part(self) -> Result<UploadPart, UploadError> {
        Ok(UploadPart {
            index: size_of(self.part_index, "part_index")?,
            length: size_of(self.length, "length")?,
            hash: content_hash(self.hash)?,
            path: self.path,
        })
    }
}

/// 把上传会话写语句的约束错误翻译成带上下文的类型化错误。
fn classify_write_error(error: sqlx::Error, id: UploadId) -> UploadError {
    let Some(database_error) = error.as_database_error() else {
        return UploadError::Db(error);
    };
    if database_error
        .message()
        .starts_with("UNIQUE constraint failed")
    {
        return UploadError::DuplicateId { id };
    }
    UploadError::Db(error)
}

/// 把块写语句的约束错误翻译成带上下文的类型化错误。
fn classify_part_error(error: sqlx::Error, new: &NewUploadPart) -> UploadError {
    let Some(database_error) = error.as_database_error() else {
        return UploadError::Db(error);
    };
    let message = database_error.message();
    if message.contains("upload_parts.path") {
        return UploadError::DuplicatePartPath {
            path: new.path.clone(),
        };
    }
    if message.starts_with("UNIQUE constraint failed") || message.contains("upload_parts.upload_id")
    {
        // 写事务是 `BEGIN IMMEDIATE` 串行的，正常路径不会走到这里；出现即说明库里的行被绕过本模块
        // 写过，按内容冲突报错而不是覆盖别人的记录。
        return UploadError::PartConflict {
            index: new.index,
            reason: "同一索引已被另一行占用",
        };
    }
    UploadError::Db(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::db::Database;
    use crate::test_support::{
        TestDir, at, content_hash, open_database, register_vault, seed_owner, session,
        upload as upload_id,
    };

    /// 一个 10 字节、块大小 4 的上传：块数与每块长度是 4、4、2。
    fn ten_bytes(vault: VaultId, marker: u8) -> NewUpload {
        NewUpload {
            id: upload_id(marker),
            vault,
            session: session(0x31),
            expected_size: 10,
            hash: content_hash(0x10),
            chunk_size: 4,
            expires_at: at(1_800),
        }
    }

    fn part(index: u64, length: u64, marker: u8) -> NewUploadPart {
        NewUploadPart {
            index,
            length,
            hash: content_hash(marker),
            path: format!("/srv/uploads/parts-{marker:02x}"),
        }
    }

    async fn begin(database: &Database, owner: UserId, new: &NewUpload) -> UploadRecord {
        let mut tx = database.begin_write().await.expect("开始写事务");
        let record = begin_upload(&mut tx, owner, new).await.expect("建上传会话");
        tx.commit().await.expect("提交");
        record
    }

    async fn record(
        database: &Database,
        owner: UserId,
        upload: UploadId,
        new: &NewUploadPart,
    ) -> PartOutcome {
        let mut tx = database.begin_write().await.expect("开始写事务");
        let outcome = record_part(&mut tx, owner, upload, new)
            .await
            .expect("记录块");
        tx.commit().await.expect("提交");
        outcome
    }

    #[tokio::test]
    async fn a_restarted_server_lists_unfinished_sessions_and_missing_blocks() {
        let dir = TestDir::new("uploads-restart");
        let path = dir.database();
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        begin(&database, owner, &ten_bytes(vault, 0x41)).await;

        record(&database, owner, upload_id(0x41), &part(0, 4, 0x01)).await;
        record(&database, owner, upload_id(0x41), &part(2, 2, 0x03)).await;
        database.close().await;

        // 重启：只靠数据库就能列出未结束的上传、已收块与缺块，不需要客户端位图。
        let database = Database::open(&path).await.expect("重启后打开库");
        let mut conn = database.pool().acquire().await.expect("取连接");
        let open = unfinished_uploads(&mut conn, owner, vault)
            .await
            .expect("列出未结束上传");
        drop(conn);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, upload_id(0x41));
        assert_eq!(open[0].state, UploadState::Open);
        assert_eq!(open[0].geometry().expect("已校验块大小").part_count(), 3);

        let mut conn = database.pool().acquire().await.expect("取连接");
        let progress = upload_progress(&mut conn, owner, upload_id(0x41))
            .await
            .expect("读进度");
        assert_eq!(
            progress
                .received
                .iter()
                .map(|part| (part.index, part.length))
                .collect::<Vec<_>>(),
            vec![(0, 4), (2, 2)]
        );
        // 缺块以区间表示：索引 1 一块。
        assert_eq!(
            progress.missing,
            vec![MissingRange {
                first_index: 1,
                count: 1
            }]
        );
        assert_eq!(progress.missing_part_count(), 1);
        assert!(!progress.is_complete());
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn a_repeated_part_index_never_creates_a_second_row() {
        let dir = TestDir::new("uploads-part-identity");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        begin(&database, owner, &ten_bytes(vault, 0x41)).await;

        let first = part(1, 4, 0x02);
        match record(&database, owner, upload_id(0x41), &first).await {
            PartOutcome::Recorded(recorded) => {
                assert_eq!(recorded.index, 1);
                assert_eq!(recorded.length, 4);
            }
            other => panic!("期望首次记录，实际 {other:?}"),
        }
        // 同一索引、同一内容重传：幂等，不新增行。
        assert_eq!(
            record(&database, owner, upload_id(0x41), &first).await,
            PartOutcome::AlreadyPresent(UploadPart {
                index: 1,
                length: 4,
                hash: content_hash(0x02),
                path: "/srv/uploads/parts-02".to_owned(),
            })
        );

        // 同一索引、不同内容：拒绝，旧行不动（design §13.1：不能混合两个本地 generation）。
        for (conflict, reason) in [
            (part(1, 4, 0x09), "哈希不同"),
            (
                NewUploadPart {
                    path: "/srv/uploads/parts-99".to_owned(),
                    ..part(1, 4, 0x02)
                },
                "暂存路径不同",
            ),
        ] {
            let mut tx = database.begin_write().await.expect("开始写事务");
            let error = record_part(&mut tx, owner, upload_id(0x41), &conflict)
                .await
                .expect_err("不同内容必须拒绝");
            match error {
                UploadError::PartConflict {
                    index,
                    reason: actual,
                } => {
                    assert_eq!(index, 1);
                    assert_eq!(actual, reason);
                }
                other => panic!("期望 PartConflict，实际 {other:?}"),
            }
            tx.rollback().await.expect("回滚");
        }

        // 索引唯一由主键保证：整张表只有一行。
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM upload_parts")
            .fetch_one(&mut *database.pool().acquire().await.unwrap())
            .await
            .expect("计数块行");
        assert_eq!(rows, 1);
        let stored = parts_of(
            &mut database.pool().acquire().await.unwrap(),
            owner,
            upload_id(0x41),
        )
        .await
        .expect("读块");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].hash, content_hash(0x02), "旧行不被覆盖");
        database.close().await;
    }

    #[tokio::test]
    async fn raw_sql_cannot_bypass_the_part_and_session_constraints() {
        let dir = TestDir::new("uploads-constraints");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        begin(&database, owner, &ten_bytes(vault, 0x41)).await;
        record(&database, owner, upload_id(0x41), &part(0, 4, 0x01)).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        // 主键 `(upload_id, part_index)` 是“索引唯一”的最后一道保证：绕过 Rust 再写同一索引也失败。
        let duplicate = sqlx::query(
            "INSERT INTO upload_parts (upload_id, part_index, length, hash, path) \
             VALUES (?1, 0, 4, ?2, '/srv/uploads/other')",
        )
        .bind(upload_id(0x41).as_bytes().as_slice())
        .bind(content_hash(0x09).as_slice())
        .execute(&mut *tx)
        .await
        .expect_err("同一索引的第二行必须被主键拒绝");
        assert!(
            duplicate.to_string().contains("UNIQUE"),
            "实际错误 {duplicate}"
        );
        // 零长度块与零块大小没有意义，迁移的 CHECK 兜底。
        let zero_length = sqlx::query(
            "INSERT INTO upload_parts (upload_id, part_index, length, hash, path) \
             VALUES (?1, 1, 0, ?2, '/srv/uploads/zero')",
        )
        .bind(upload_id(0x41).as_bytes().as_slice())
        .bind(content_hash(0x08).as_slice())
        .execute(&mut *tx)
        .await
        .expect_err("零长度块必须被 CHECK 拒绝");
        assert!(
            zero_length.to_string().contains("CHECK"),
            "实际错误 {zero_length}"
        );
        let zero_chunk = sqlx::query(
            "INSERT INTO uploads \
             (id, vault_id, session_id, expected_size, hash, chunk_size, expires_at, state) \
             VALUES (?1, ?2, ?3, 10, ?4, 0, 1800, 'open')",
        )
        .bind(upload_id(0x47).as_bytes().as_slice())
        .bind(vault.as_bytes().as_slice())
        .bind(session(0x31).as_bytes().as_slice())
        .bind(content_hash(0x10).as_slice())
        .execute(&mut *tx)
        .await
        .expect_err("零块大小必须被 CHECK 拒绝");
        assert!(
            zero_chunk.to_string().contains("CHECK"),
            "实际错误 {zero_chunk}"
        );
        tx.rollback().await.expect("回滚");
        database.close().await;
    }

    #[tokio::test]
    async fn chunk_geometry_bounds_every_block() {
        // 块几何：10 字节 / 4 字节块 = 4、4、2；空内容 0 块。
        let geometry = ChunkGeometry::new(10, 4).expect("块几何");
        assert_eq!(geometry.part_count(), 3);
        assert_eq!(
            (0..3)
                .map(|index| geometry.part_length(index))
                .collect::<Vec<_>>(),
            vec![Some(4), Some(4), Some(2)]
        );
        assert_eq!(geometry.part_length(3), None);
        assert_eq!(
            ChunkGeometry::new(0, 4).expect("空内容块几何").part_count(),
            0
        );
        assert!(matches!(
            ChunkGeometry::new(10, 0),
            Err(UploadError::InvalidChunkSize)
        ));

        let dir = TestDir::new("uploads-geometry");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        begin(&database, owner, &ten_bytes(vault, 0x41)).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        for (new, expected) in [(part(3, 2, 0x04), "越界"), (part(1, 3, 0x05), "长度")] {
            let error = record_part(&mut tx, owner, upload_id(0x41), &new)
                .await
                .expect_err("越界或长度不符的块必须拒绝");
            match (expected, error) {
                ("越界", UploadError::PartOutOfRange { index, part_count }) => {
                    assert_eq!((index, part_count), (3, 3));
                }
                (
                    "长度",
                    UploadError::PartLengthMismatch {
                        index,
                        expected,
                        actual,
                    },
                ) => {
                    assert_eq!((index, expected, actual), (1, 4, 3));
                }
                (_, other) => panic!("意外的错误 {other:?}"),
            }
        }
        // 空块路径不能登记（路径是清理与续传的依据）。
        let error = record_part(
            &mut tx,
            owner,
            upload_id(0x41),
            &NewUploadPart {
                path: String::new(),
                ..part(0, 4, 0x06)
            },
        )
        .await
        .expect_err("空路径必须拒绝");
        assert!(matches!(error, UploadError::EmptyPartPath));
        tx.rollback().await.expect("回滚");

        // 0 字节内容：合法、没有块，可以直接标记齐备。
        let empty = NewUpload {
            id: upload_id(0x42),
            expected_size: 0,
            hash: content_hash(0x11),
            ..ten_bytes(vault, 0x42)
        };
        begin(&database, owner, &empty).await;
        let mut tx = database.begin_write().await.expect("开始写事务");
        let complete = mark_upload_complete(&mut tx, owner, upload_id(0x42))
            .await
            .expect("空内容没有缺块");
        tx.commit().await.expect("提交");
        assert_eq!(complete.state, UploadState::Complete);
        database.close().await;
    }

    #[tokio::test]
    async fn a_session_only_becomes_complete_when_every_block_is_present() {
        let dir = TestDir::new("uploads-states");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        begin(&database, owner, &ten_bytes(vault, 0x41)).await;
        record(&database, owner, upload_id(0x41), &part(0, 4, 0x01)).await;

        // 缺块时不是 complete：部分内容不能上报为完成（design I05）。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = mark_upload_complete(&mut tx, owner, upload_id(0x41))
            .await
            .expect_err("缺块不能标记齐备");
        assert!(
            matches!(error, UploadError::IncompleteUpload { missing_parts: 2 }),
            "实际错误 {error:?}"
        );
        // 也不能跳过 complete 直接 finalize。
        let error = mark_upload_finalized(&mut tx, owner, upload_id(0x41))
            .await
            .expect_err("未齐不能 finalize");
        assert!(
            matches!(
                error,
                UploadError::IllegalTransition {
                    from: UploadState::Open,
                    to: UploadState::Finalized
                }
            ),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        record(&database, owner, upload_id(0x41), &part(1, 4, 0x02)).await;
        record(&database, owner, upload_id(0x41), &part(2, 2, 0x03)).await;
        let mut tx = database.begin_write().await.expect("开始写事务");
        mark_upload_complete(&mut tx, owner, upload_id(0x41))
            .await
            .expect("块已齐");
        let finalized = mark_upload_finalized(&mut tx, owner, upload_id(0x41))
            .await
            .expect("发布为对象");
        tx.commit().await.expect("提交");
        assert_eq!(finalized.state, UploadState::Finalized);

        // 终态稳定：不再收块、不再改状态。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = record_part(&mut tx, owner, upload_id(0x41), &part(0, 4, 0x01))
            .await
            .expect_err("终态不再收块");
        assert!(
            matches!(
                error,
                UploadError::PartsClosed {
                    state: UploadState::Finalized
                }
            ),
            "实际错误 {error:?}"
        );
        let error = abort_upload(&mut tx, owner, upload_id(0x41))
            .await
            .expect_err("终态不能再放弃");
        assert!(matches!(error, UploadError::IllegalTransition { .. }));
        tx.rollback().await.expect("回滚");

        // 放弃是另一条终态路径，已收块保留为清理证据。
        begin(&database, owner, &ten_bytes(vault, 0x43)).await;
        record(&database, owner, upload_id(0x43), &part(0, 4, 0x07)).await;
        let mut tx = database.begin_write().await.expect("开始写事务");
        let aborted = abort_upload(&mut tx, owner, upload_id(0x43))
            .await
            .expect("放弃会话");
        tx.commit().await.expect("提交");
        assert_eq!(aborted.state, UploadState::Aborted);
        let parts = parts_of(
            &mut database.pool().acquire().await.unwrap(),
            owner,
            upload_id(0x43),
        )
        .await
        .expect("读块");
        assert_eq!(parts.len(), 1, "放弃后已收块行保留");
        database.close().await;
    }

    #[tokio::test]
    async fn uploads_validate_input_and_owner_scope() {
        let dir = TestDir::new("uploads-validation");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let other = seed_owner(&database, 0x12).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        // 块大小为 0 没有块几何。
        let error = begin_upload(
            &mut tx,
            owner,
            &NewUpload {
                chunk_size: 0,
                ..ten_bytes(vault, 0x44)
            },
        )
        .await
        .expect_err("块大小必须大于 0");
        assert!(matches!(error, UploadError::InvalidChunkSize));
        // 超过 SQLite INTEGER 上限的长度落库会被截断。
        let error = begin_upload(
            &mut tx,
            owner,
            &NewUpload {
                expected_size: u64::MAX,
                ..ten_bytes(vault, 0x45)
            },
        )
        .await
        .expect_err("长度必须可落库");
        assert!(
            matches!(error, UploadError::SizeAboveDbLimit { size } if size == u64::MAX),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        begin(&database, owner, &ten_bytes(vault, 0x41)).await;
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = begin_upload(&mut tx, owner, &ten_bytes(vault, 0x41))
            .await
            .expect_err("重复身份必须拒绝");
        assert!(
            matches!(error, UploadError::DuplicateId { id } if id == upload_id(0x41)),
            "实际错误 {error:?}"
        );
        let error = begin_upload(&mut tx, other, &ten_bytes(vault, 0x46))
            .await
            .expect_err("别人的 Vault 不能建上传");
        assert!(matches!(error, UploadError::OutOfScope));
        let error = record_part(&mut tx, other, upload_id(0x41), &part(0, 4, 0x01))
            .await
            .expect_err("别人的上传不能写块");
        assert!(matches!(error, UploadError::UnknownUpload { .. }));
        tx.rollback().await.expect("回滚");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            upload_in_scope(&mut conn, other, upload_id(0x41))
                .await
                .expect("别人的账号读不到"),
            None
        );
        drop(conn);
        database.close().await;
    }
}
