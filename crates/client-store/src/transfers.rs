//! 入站内容传输的进度、块位图与取消状态（`docs/design.md` §7.3、§9.3）。
//!
//! 边界：本模块只保存“这个 VersionId 要下哪些块、已经收到哪些、校验结果、临时文件在哪、是否
//! 被取消”。它不发起 HTTP、不写文件、不算哈希：块由传输层写入 `temp_path`，校验由校验方完成后
//! 再调用 [`complete_transfer`]。
//!
//! 只记**入站**内容（下载）：出站上传的已收块以服务端 `upload_parts` 为准（design §9.3
//! “不能只相信客户端位图”），不在这张表里重复维护一份可能过期的位图。
//!
//! 不变量：
//!
//! - **位图长度由块几何决定**：块数 = `ceil(total_size / chunk_size)`，位图字节数 =
//!   `ceil(块数 / 8)`；写入前与读取时都核对，尾部多余 bit 必须为 0。位图的第 i 位对应第 i 块
//!   （低位在前），因此同一份位图只有一种字节表示。
//! - **内容按 VersionId 固定**：主键是 `(binding, version)`；同一个不可变版本只有一条进度，
//!   版本内容不变（design I12），位图可以跨重启续传。
//! - **只有校验通过的完整内容才是 `completed`**：`completed` 由 [`complete_transfer`] 写入并
//!   要求位图覆盖全部块；`failed` 必须带错误码；`cancelled` 是持久状态，取消后不能再记录块
//!   （要继续就重新开始一条传输）。
//! - **状态转换只从 `running` 出发**：`completed`/`cancelled`/`failed` 是终态，只有
//!   [`discard_transfer`] 能把它们删掉，避免终态被悄悄改回进行中。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{BindingId, ID_LEN, VersionId};
use vaultsyncd_domain::version::DB_INTEGER_MAX;

/// 传输状态（`transfers.state`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferState {
    /// 正在传输：可以记录块、完成、失败或取消。
    Running,
    /// 已取消；位图与临时路径保留到调用方清理。
    Cancelled,
    /// 内容完整且已通过校验。
    Completed,
    /// 停止自动推进；原因见 [`TransferRecord::last_error`]。
    Failed,
}

impl TransferState {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Cancelled => "cancelled",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜状态。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "running" => Some(Self::Running),
            "cancelled" => Some(Self::Cancelled),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// 开始一条下载的输入。
///
/// 不变量（违反时 [`start_transfer`] 报错且不写入）：`chunk_size > 0`；`total_size` 不超过
/// SQLite `INTEGER` 上限；`bitmap` 长度与块几何一致且尾部 bit 为 0；`temp_path` 非空、无 NUL；
/// `(binding, version)` 未被占用。
#[derive(Clone, Debug)]
pub struct NewTransfer {
    /// 所属绑定。
    pub binding: BindingId,
    /// 固定下载的内容版本。
    pub version: VersionId,
    /// 块大小（字节）。
    pub chunk_size: u64,
    /// 内容总长度（字节）。
    pub total_size: u64,
    /// 已收到的块位图；全新下载时是全 0 的正确长度位图。
    pub bitmap: Vec<u8>,
    /// 临时文件路径（缓存目录内）；本层只保存，不做文件系统 I/O。
    pub temp_path: String,
}

/// 一条传输进度（`transfers` 一行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferRecord {
    /// 所属绑定。
    pub binding: BindingId,
    /// 固定下载的内容版本。
    pub version: VersionId,
    /// 块大小（字节）。
    pub chunk_size: u64,
    /// 内容总长度（字节）。
    pub total_size: u64,
    /// 已收到的块位图（低位在前）。
    pub bitmap: Vec<u8>,
    /// 临时文件路径。
    pub temp_path: String,
    /// 当前状态。
    pub state: TransferState,
    /// 内容是否已通过校验。
    pub verified: bool,
    /// 失败原因；只有 [`TransferState::Failed`] 有。
    pub last_error: Option<ErrorCode>,
}

impl TransferRecord {
    /// 块数：`ceil(total_size / chunk_size)`。
    pub fn chunk_count(&self) -> u64 {
        chunk_count(self.total_size, self.chunk_size)
    }

    /// 第 `index` 块是否已收到；越界返回 `false`。
    pub fn has_chunk(&self, index: u64) -> bool {
        index < self.chunk_count() && bit_is_set(&self.bitmap, index)
    }
}

/// 传输读写的失败。
#[derive(Debug)]
pub enum TransferError {
    /// SQLite 层面失败（含外键与 `CHECK` 拒绝）。
    Db(sqlx::Error),
    /// 块大小为 0：无法定义位图。
    ZeroChunkSize,
    /// 长度或块大小超过 SQLite `INTEGER` 上限。
    SizeAboveDbLimit,
    /// 位图字节数与块几何不一致。
    BitmapSizeMismatch {
        /// 期望的字节数。
        expected: u64,
        /// 实际给的字节数。
        found: u64,
    },
    /// 位图尾部（块数之后）的 bit 不为 0。
    BitmapTailNotZero,
    /// 临时路径不合格。
    InvalidTempPath {
        /// 违反的具体规则。
        reason: &'static str,
    },
    /// 该 `(binding, version)` 已有传输记录。
    DuplicateTransfer {
        /// 冲突的内容版本。
        version: VersionId,
    },
    /// 传输状态不允许这个操作（已取消、已完成或已失败）。
    TransferClosed {
        /// 当前状态。
        state: TransferState,
    },
    /// 块序号超出块数。
    ChunkOutOfRange {
        /// 请求的块序号。
        index: u64,
        /// 该内容的块数。
        chunks: u64,
    },
    /// 内容没有全部收到，不能算完成。
    IncompleteContent {
        /// 尚未收到的块数。
        missing: u64,
    },
    /// 记录不存在。
    UnknownTransfer {
        /// 缺失的内容版本。
        version: VersionId,
    },
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::ZeroChunkSize => f.write_str("块大小必须大于 0"),
            Self::SizeAboveDbLimit => {
                write!(f, "长度或块大小超过数据库有符号 64 位上限 {DB_INTEGER_MAX}")
            }
            Self::BitmapSizeMismatch { expected, found } => {
                write!(f, "位图长度应为 {expected} 字节，实际 {found} 字节")
            }
            Self::BitmapTailNotZero => f.write_str("位图在块数之后的 bit 必须为 0"),
            Self::InvalidTempPath { reason } => write!(f, "临时路径不合法：{reason}"),
            Self::DuplicateTransfer { version } => {
                write!(f, "内容版本 {version} 已有传输记录")
            }
            Self::TransferClosed { state } => {
                write!(f, "传输状态为 {}，不再接受进度更新", state.as_text())
            }
            Self::ChunkOutOfRange { index, chunks } => {
                write!(f, "块序号 {index} 超出自 0 起的 {chunks} 块")
            }
            Self::IncompleteContent { missing } => {
                write!(f, "还有 {missing} 块未收到，内容不算完整")
            }
            Self::UnknownTransfer { version } => write!(f, "内容版本 {version} 没有传输记录"),
            Self::MalformedRow { column } => {
                write!(f, "数据库列 {column} 的值不符合类型前提")
            }
        }
    }
}

impl std::error::Error for TransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::ZeroChunkSize
            | Self::SizeAboveDbLimit
            | Self::BitmapSizeMismatch { .. }
            | Self::BitmapTailNotZero
            | Self::InvalidTempPath { .. }
            | Self::DuplicateTransfer { .. }
            | Self::TransferClosed { .. }
            | Self::ChunkOutOfRange { .. }
            | Self::IncompleteContent { .. }
            | Self::UnknownTransfer { .. }
            | Self::MalformedRow { .. } => None,
        }
    }
}

/// `transfers` 的列顺序。
const TRANSFER_COLUMNS: &str = "binding_id, version_id, chunk_size, total_size, bitmap, temp_path, \
                                state, verified, last_error_code";

/// 开始一条下载：写入块几何、初始位图与临时路径，状态为 `running`。
pub async fn start_transfer(
    tx: &mut Transaction<'_, Sqlite>,
    new: &NewTransfer,
) -> Result<TransferRecord, TransferError> {
    let chunks = checked_chunk_count(new.total_size, new.chunk_size)?;
    let expected = bitmap_len(chunks);
    let found = new.bitmap.len() as u64;
    if found != expected {
        return Err(TransferError::BitmapSizeMismatch { expected, found });
    }
    if !tail_bits_clear(&new.bitmap, chunks) {
        return Err(TransferError::BitmapTailNotZero);
    }
    validate_temp_path(&new.temp_path)?;

    let inserted = sqlx::query(
        "INSERT INTO transfers \
         (binding_id, version_id, chunk_size, total_size, bitmap, temp_path, state) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(new.binding.as_bytes().as_slice())
    .bind(new.version.as_bytes().as_slice())
    .bind(db_integer(new.chunk_size)?)
    .bind(db_integer(new.total_size)?)
    .bind(&new.bitmap)
    .bind(&new.temp_path)
    .bind(TransferState::Running.as_text())
    .execute(&mut **tx)
    .await;

    if let Err(error) = inserted {
        if error
            .as_database_error()
            .map(|database_error| database_error.is_unique_violation())
            .unwrap_or(false)
        {
            return Err(TransferError::DuplicateTransfer {
                version: new.version,
            });
        }
        return Err(TransferError::Db(error));
    }

    transfer(tx, new.binding, new.version)
        .await?
        .ok_or(TransferError::UnknownTransfer {
            version: new.version,
        })
}

/// 读一条传输进度；不存在返回 `None`。
pub async fn transfer(
    conn: &mut SqliteConnection,
    binding: BindingId,
    version: VersionId,
) -> Result<Option<TransferRecord>, TransferError> {
    let row = sqlx::query_as::<_, TransferRow>(&format!(
        "SELECT {TRANSFER_COLUMNS} FROM transfers WHERE binding_id = ?1 AND version_id = ?2"
    ))
    .bind(binding.as_bytes().as_slice())
    .bind(version.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(TransferError::Db)?;
    row.map(TransferRow::into_record).transpose()
}

/// 列出某个绑定的全部传输进度（按版本字节序，便于稳定断言与续传遍历）。
pub async fn transfers_for_binding(
    conn: &mut SqliteConnection,
    binding: BindingId,
) -> Result<Vec<TransferRecord>, TransferError> {
    let rows = sqlx::query_as::<_, TransferRow>(&format!(
        "SELECT {TRANSFER_COLUMNS} FROM transfers WHERE binding_id = ?1 ORDER BY version_id"
    ))
    .bind(binding.as_bytes().as_slice())
    .fetch_all(conn)
    .await
    .map_err(TransferError::Db)?;
    rows.into_iter()
        .map(TransferRow::into_record)
        .collect::<Result<Vec<_>, _>>()
}

/// 记录第 `index` 块已收到；返回是否是**新**记下的块（重复记录同一块返回 `false`）。
///
/// 只有 `running` 的传输接受进度；越界块、已取消/已完成/已失败的传输都明确报错，不静默丢弃。
pub async fn record_chunk(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    version: VersionId,
    index: u64,
) -> Result<bool, TransferError> {
    let record = transfer(tx, binding, version)
        .await?
        .ok_or(TransferError::UnknownTransfer { version })?;
    if record.state != TransferState::Running {
        return Err(TransferError::TransferClosed {
            state: record.state,
        });
    }
    let chunks = record.chunk_count();
    if index >= chunks {
        return Err(TransferError::ChunkOutOfRange { index, chunks });
    }
    if record.has_chunk(index) {
        return Ok(false);
    }

    let mut bitmap = record.bitmap;
    set_bit(&mut bitmap, index);
    sqlx::query("UPDATE transfers SET bitmap = ?1 WHERE binding_id = ?2 AND version_id = ?3")
        .bind(&bitmap)
        .bind(binding.as_bytes().as_slice())
        .bind(version.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(TransferError::Db)?;
    Ok(true)
}

/// 标记内容完整且已通过校验；位图必须覆盖全部块，否则报 [`TransferError::IncompleteContent`]。
///
/// “已通过校验”是调用方的结论：本层不重算哈希，只记录这个结果并把它作为 `completed` 的前提
/// （design §9.3：校验结果随传输一起持久化）。
pub async fn complete_transfer(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    version: VersionId,
) -> Result<bool, TransferError> {
    let record = running_transfer(tx, binding, version).await?;
    let missing = (0..record.chunk_count())
        .filter(|index| !record.has_chunk(*index))
        .count() as u64;
    if missing > 0 {
        return Err(TransferError::IncompleteContent { missing });
    }
    set_state(
        tx,
        binding,
        version,
        TransferState::Completed,
        Some(true),
        None,
    )
    .await
}

/// 记录一次失败并保留已收块：状态转 `failed`，原因必须给出。
pub async fn fail_transfer(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    version: VersionId,
    code: ErrorCode,
) -> Result<bool, TransferError> {
    running_transfer(tx, binding, version).await?;
    set_state(
        tx,
        binding,
        version,
        TransferState::Failed,
        None,
        Some(code),
    )
    .await
}

/// 取消传输：保留位图与临时路径（回滚/清理由调用方决定），状态转 `cancelled`。
pub async fn cancel_transfer(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    version: VersionId,
) -> Result<bool, TransferError> {
    running_transfer(tx, binding, version).await?;
    set_state(tx, binding, version, TransferState::Cancelled, None, None).await
}

/// 删除一条传输记录（安装完成或已清理临时文件）；返回是否命中。
pub async fn discard_transfer(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    version: VersionId,
) -> Result<bool, TransferError> {
    let deleted = sqlx::query("DELETE FROM transfers WHERE binding_id = ?1 AND version_id = ?2")
        .bind(binding.as_bytes().as_slice())
        .bind(version.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(TransferError::Db)?;
    Ok(deleted.rows_affected() > 0)
}

/// 读记录并要求它仍在进行中：终态不再接受进度更新或第二次结束。
async fn running_transfer(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    version: VersionId,
) -> Result<TransferRecord, TransferError> {
    let record = transfer(tx, binding, version)
        .await?
        .ok_or(TransferError::UnknownTransfer { version })?;
    if record.state != TransferState::Running {
        return Err(TransferError::TransferClosed {
            state: record.state,
        });
    }
    Ok(record)
}

/// 写状态与可选的校验结果、错误码。
async fn set_state(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    version: VersionId,
    state: TransferState,
    verified: Option<bool>,
    code: Option<ErrorCode>,
) -> Result<bool, TransferError> {
    let updated = sqlx::query(
        "UPDATE transfers SET state = ?1, verified = COALESCE(?2, verified), last_error_code = ?3 \
         WHERE binding_id = ?4 AND version_id = ?5",
    )
    .bind(state.as_text())
    .bind(verified.map(i64::from))
    .bind(code.map(ErrorCode::as_str))
    .bind(binding.as_bytes().as_slice())
    .bind(version.as_bytes().as_slice())
    .execute(&mut **tx)
    .await
    .map_err(TransferError::Db)?;
    Ok(updated.rows_affected() > 0)
}

fn validate_temp_path(path: &str) -> Result<(), TransferError> {
    if path.is_empty() {
        return Err(TransferError::InvalidTempPath {
            reason: "不能为空"
        });
    }
    if path.contains('\0') {
        return Err(TransferError::InvalidTempPath {
            reason: "不能包含 NUL",
        });
    }
    Ok(())
}

fn db_integer(value: u64) -> Result<i64, TransferError> {
    if value > DB_INTEGER_MAX {
        return Err(TransferError::SizeAboveDbLimit);
    }
    Ok(value as i64)
}

/// 由长度与块大小推出块数；`chunk_size` 为 0 或超过数据库上限时报错。
///
/// 两者都不超过 `i64::MAX`，因此 `total_size + chunk_size - 1` 不会溢出 `u64`（上限约 2^64-2）。
fn checked_chunk_count(total_size: u64, chunk_size: u64) -> Result<u64, TransferError> {
    if chunk_size == 0 {
        return Err(TransferError::ZeroChunkSize);
    }
    if chunk_size > DB_INTEGER_MAX || total_size > DB_INTEGER_MAX {
        return Err(TransferError::SizeAboveDbLimit);
    }
    Ok(total_size.div_ceil(chunk_size))
}

/// 读路径上的块数：块大小已由 `CHECK (chunk_size > 0)` 保证，这里只需避免除零。
fn chunk_count(total_size: u64, chunk_size: u64) -> u64 {
    if chunk_size == 0 {
        return 0;
    }
    total_size.div_ceil(chunk_size)
}

fn bitmap_len(chunks: u64) -> u64 {
    chunks.div_ceil(8)
}

fn bit_is_set(bitmap: &[u8], index: u64) -> bool {
    let byte = (index / 8) as usize;
    match bitmap.get(byte) {
        Some(value) => value & (1 << (index % 8)) != 0,
        None => false,
    }
}

fn set_bit(bitmap: &mut [u8], index: u64) {
    let byte = (index / 8) as usize;
    if let Some(value) = bitmap.get_mut(byte) {
        *value |= 1 << (index % 8);
    }
}

/// 块数之后的 bit 必须为 0，否则同一份“收到哪些块”的事实有两种编码。
fn tail_bits_clear(bitmap: &[u8], chunks: u64) -> bool {
    let full_bytes = (chunks / 8) as usize;
    let tail_bits = (chunks % 8) as u32;
    let mut index = full_bytes;
    while index < bitmap.len() {
        let used = if index == full_bytes && tail_bits > 0 {
            // 尾部字节只允许低 `tail_bits` 位被使用。
            let mask = (1u8 << tail_bits).wrapping_sub(1);
            bitmap[index] & !mask
        } else {
            bitmap[index]
        };
        if used != 0 {
            return false;
        }
        index += 1;
    }
    true
}

/// 数据库行到 [`TransferRecord`] 的中间形态。
#[derive(sqlx::FromRow)]
struct TransferRow {
    binding_id: Vec<u8>,
    version_id: Vec<u8>,
    chunk_size: i64,
    total_size: i64,
    bitmap: Vec<u8>,
    temp_path: String,
    state: String,
    verified: i64,
    last_error_code: Option<String>,
}

impl TransferRow {
    fn into_record(self) -> Result<TransferRecord, TransferError> {
        let binding = identity(self.binding_id, "binding_id", BindingId::from_bytes)?;
        let version = identity(self.version_id, "version_id", VersionId::from_bytes)?;
        let chunk_size =
            u64::try_from(self.chunk_size).map_err(|_| TransferError::MalformedRow {
                column: "chunk_size",
            })?;
        let total_size =
            u64::try_from(self.total_size).map_err(|_| TransferError::MalformedRow {
                column: "total_size",
            })?;
        if chunk_size == 0 {
            return Err(TransferError::MalformedRow {
                column: "chunk_size",
            });
        }
        // 位图长度与尾部 bit 都是可校验事实：外部写入的坏行在这里被拒绝，而不是当成“少下几块”。
        let chunks = chunk_count(total_size, chunk_size);
        if self.bitmap.len() as u64 != bitmap_len(chunks) {
            return Err(TransferError::MalformedRow { column: "bitmap" });
        }
        if !tail_bits_clear(&self.bitmap, chunks) {
            return Err(TransferError::MalformedRow { column: "bitmap" });
        }
        Ok(TransferRecord {
            binding,
            version,
            chunk_size,
            total_size,
            bitmap: self.bitmap,
            temp_path: self.temp_path,
            state: TransferState::from_text(&self.state)
                .ok_or(TransferError::MalformedRow { column: "state" })?,
            verified: self.verified != 0,
            last_error: self
                .last_error_code
                .map(|text| {
                    ErrorCode::from_code(&text).ok_or(TransferError::MalformedRow {
                        column: "last_error_code",
                    })
                })
                .transpose()?,
        })
    }
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; ID_LEN]) -> T,
) -> Result<T, TransferError> {
    let array: [u8; ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| TransferError::MalformedRow { column })?;
    Ok(wrap(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{TestDir, open_database, seed_binding, version_id};

    fn new_transfer(binding: BindingId, marker: u8) -> NewTransfer {
        NewTransfer {
            binding,
            version: version_id(marker),
            chunk_size: 4,
            total_size: 10,
            // 10 字节 / 4 字节块 = 3 块；3 块需要 1 字节位图。
            bitmap: vec![0],
            temp_path: format!(r"C:\vaultsyncd-test\cache\{marker:02x}.part"),
        }
    }

    #[tokio::test]
    async fn transfer_round_trips_with_bitmap_geometry() {
        let dir = TestDir::new("transfer");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x21).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let started = start_transfer(&mut tx, &new_transfer(binding, 0x41))
            .await
            .expect("开始传输");
        assert_eq!(started.state, TransferState::Running);
        assert_eq!(started.chunk_count(), 3);
        assert!(!started.verified);
        assert!(!started.has_chunk(0));

        // 乱序记录块：位图第 i 位对应第 i 块。
        assert!(
            record_chunk(&mut tx, binding, version_id(0x41), 2)
                .await
                .expect("记录第 3 块")
        );
        assert!(
            record_chunk(&mut tx, binding, version_id(0x41), 0)
                .await
                .expect("记录第 1 块")
        );
        assert!(
            !record_chunk(&mut tx, binding, version_id(0x41), 0)
                .await
                .expect("重复记录同一块"),
            "重复记录不是新的块"
        );
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let record = transfer(&mut conn, binding, version_id(0x41))
            .await
            .expect("读传输")
            .expect("传输必须存在");
        assert_eq!(record.bitmap, vec![0b0000_0101], "第 0、2 块已收到");
        assert!(record.has_chunk(0) && record.has_chunk(2));
        assert!(!record.has_chunk(1));
        assert_eq!(record.temp_path, r"C:\vaultsyncd-test\cache\41.part");
        assert_eq!(
            transfers_for_binding(&mut conn, binding)
                .await
                .expect("列传输"),
            vec![record.clone()]
        );
        drop(conn);
        database.close().await;

        // 重启后位图仍在：续传不必重下已收块。
        let reopened = open_database(&dir).await;
        let mut conn = reopened.pool().acquire().await.expect("取连接");
        let record = transfer(&mut conn, binding, version_id(0x41))
            .await
            .expect("读传输")
            .expect("传输必须存在");
        assert_eq!(record.bitmap, vec![0b0000_0101]);
        assert_eq!(record.state, TransferState::Running);
        drop(conn);
        reopened.close().await;
    }

    #[tokio::test]
    async fn bitmap_geometry_and_temp_path_are_validated_before_write() {
        let dir = TestDir::new("transfer-geometry");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x22).await;

        let mut tx = database.begin_write().await.expect("开始写事务");

        let mut zero_chunk = new_transfer(binding, 0x42);
        zero_chunk.chunk_size = 0;
        zero_chunk.bitmap = Vec::new();
        assert!(matches!(
            start_transfer(&mut tx, &zero_chunk)
                .await
                .expect_err("块大小为 0 必须被拒绝"),
            TransferError::ZeroChunkSize
        ));

        let mut wrong_len = new_transfer(binding, 0x43);
        wrong_len.bitmap = vec![0, 0];
        assert!(matches!(
            start_transfer(&mut tx, &wrong_len)
                .await
                .expect_err("位图长度必须与块数一致"),
            TransferError::BitmapSizeMismatch {
                expected: 1,
                found: 2
            }
        ));

        let mut tail = new_transfer(binding, 0x44);
        // 3 块只用低 3 位；第 4 位为 1 说明编码有两种写法。
        tail.bitmap = vec![0b0000_1000];
        assert!(matches!(
            start_transfer(&mut tx, &tail)
                .await
                .expect_err("尾部 bit 必须为 0"),
            TransferError::BitmapTailNotZero
        ));

        let mut empty_path = new_transfer(binding, 0x45);
        empty_path.temp_path = String::new();
        assert!(matches!(
            start_transfer(&mut tx, &empty_path)
                .await
                .expect_err("临时路径不能为空"),
            TransferError::InvalidTempPath { .. }
        ));

        let too_big = new_transfer(binding, 0x46);
        assert!(
            checked_chunk_count(u64::MAX, 1).is_err(),
            "超过数据库上限的长度必须被拒绝"
        );
        start_transfer(&mut tx, &too_big)
            .await
            .expect("合法传输仍然可以开始");
        assert!(matches!(
            start_transfer(&mut tx, &too_big)
                .await
                .expect_err("同一版本不能有两条传输"),
            TransferError::DuplicateTransfer { .. }
        ));
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            transfers_for_binding(&mut conn, binding)
                .await
                .expect("列传输")
                .len(),
            1,
            "被拒绝的开始不得留下行"
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn chunk_progress_respects_state_and_range() {
        let dir = TestDir::new("transfer-transitions");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x23).await;
        let version = version_id(0x47);

        let mut tx = database.begin_write().await.expect("开始写事务");
        start_transfer(&mut tx, &new_transfer(binding, 0x47))
            .await
            .expect("开始传输");

        assert!(matches!(
            record_chunk(&mut tx, binding, version, 3)
                .await
                .expect_err("越界块必须被拒绝"),
            TransferError::ChunkOutOfRange {
                index: 3,
                chunks: 3
            }
        ));
        assert!(matches!(
            complete_transfer(&mut tx, binding, version)
                .await
                .expect_err("缺块不能算完成"),
            TransferError::IncompleteContent { missing: 3 }
        ));

        for index in 0..3 {
            record_chunk(&mut tx, binding, version, index)
                .await
                .expect("记录块");
        }
        assert!(
            complete_transfer(&mut tx, binding, version)
                .await
                .expect("完成传输"),
            "全部块收到后可以完成"
        );
        let completed = transfer(&mut tx, binding, version)
            .await
            .expect("读传输")
            .expect("传输必须存在");
        assert_eq!(completed.state, TransferState::Completed);
        assert!(completed.verified, "完成即已校验通过");
        assert_eq!(completed.last_error, None);

        // 终态不再接受进度、完成、失败或取消；只有 discard 能清掉它。
        let blocked = [
            record_chunk(&mut tx, binding, version, 0).await,
            complete_transfer(&mut tx, binding, version).await,
            fail_transfer(&mut tx, binding, version, ErrorCode::RateLimited).await,
            cancel_transfer(&mut tx, binding, version).await,
        ];
        for outcome in blocked {
            let error = outcome.expect_err("完成的传输不再接受状态更新");
            assert!(
                matches!(error, TransferError::TransferClosed { .. }),
                "实际错误 {error:?}"
            );
        }
        assert!(
            discard_transfer(&mut tx, binding, version)
                .await
                .expect("丢弃传输"),
            "终态可以被清理"
        );
        assert!(
            !discard_transfer(&mut tx, binding, version)
                .await
                .expect("再次丢弃")
        );
        assert!(matches!(
            record_chunk(&mut tx, binding, version, 0)
                .await
                .expect_err("记录不存在"),
            TransferError::UnknownTransfer { .. }
        ));
        tx.commit().await.expect("提交");
        database.close().await;
    }

    #[tokio::test]
    async fn cancel_and_failure_are_persisted_and_block_further_progress() {
        let dir = TestDir::new("transfer-states");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x24).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        start_transfer(&mut tx, &new_transfer(binding, 0x48))
            .await
            .expect("开始第一条");
        start_transfer(&mut tx, &new_transfer(binding, 0x49))
            .await
            .expect("开始第二条");
        record_chunk(&mut tx, binding, version_id(0x48), 1)
            .await
            .expect("记录一块");
        assert!(
            cancel_transfer(&mut tx, binding, version_id(0x48))
                .await
                .expect("取消"),
            "进行中的传输可以取消"
        );
        assert!(
            fail_transfer(&mut tx, binding, version_id(0x49), ErrorCode::HashMismatch)
                .await
                .expect("记失败"),
            "进行中的传输可以失败"
        );
        tx.commit().await.expect("提交");
        database.close().await;

        let reopened = open_database(&dir).await;
        let mut conn = reopened.pool().acquire().await.expect("取连接");
        let cancelled = transfer(&mut conn, binding, version_id(0x48))
            .await
            .expect("读传输")
            .expect("传输必须存在");
        assert_eq!(cancelled.state, TransferState::Cancelled);
        assert_eq!(cancelled.bitmap, vec![0b0000_0010], "取消保留已收块");
        assert_eq!(cancelled.last_error, None);
        let failed = transfer(&mut conn, binding, version_id(0x49))
            .await
            .expect("读传输")
            .expect("传输必须存在");
        assert_eq!(failed.state, TransferState::Failed);
        assert_eq!(failed.last_error, Some(ErrorCode::HashMismatch));
        drop(conn);

        // 取消后不能再记块：要继续就重新开始一条传输。
        let mut tx = reopened.begin_write().await.expect("开始写事务");
        assert!(matches!(
            record_chunk(&mut tx, binding, version_id(0x48), 0)
                .await
                .expect_err("取消的传输不接受块"),
            TransferError::TransferClosed {
                state: TransferState::Cancelled
            }
        ));
        assert!(matches!(
            fail_transfer(&mut tx, binding, version_id(0x49), ErrorCode::RateLimited)
                .await
                .expect_err("失败的传输不接受再次失败"),
            TransferError::TransferClosed {
                state: TransferState::Failed
            }
        ));
        tx.rollback().await.expect("回滚");
        reopened.close().await;
    }

    #[tokio::test]
    async fn malformed_bitmap_rows_are_rejected_on_read() {
        let dir = TestDir::new("transfer-malformed");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x25).await;

        // 绕过 Rust 层写入“尾部 bit 非 0”的位图：长度对但编码不唯一，读取时按坏行拒绝。
        let mut tx = database.begin_write().await.expect("开始写事务");
        sqlx::query(
            "INSERT INTO transfers \
             (binding_id, version_id, chunk_size, total_size, bitmap, temp_path, state) \
             VALUES (?1, ?2, 4, 10, x'08', ?3, 'running')",
        )
        .bind(binding.as_bytes().as_slice())
        .bind(version_id(0x4a).as_bytes().as_slice())
        .bind(r"C:\cache\4a.part")
        .execute(&mut *tx)
        .await
        .expect("写坏行");
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let error = transfer(&mut conn, binding, version_id(0x4a))
            .await
            .expect_err("坏行必须被拒绝");
        assert!(
            matches!(error, TransferError::MalformedRow { column: "bitmap" }),
            "实际错误 {error:?}"
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn zero_length_content_needs_no_bitmap_and_unknown_binding_is_rejected() {
        let dir = TestDir::new("transfer-empty");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x26).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let mut empty = new_transfer(binding, 0x4b);
        empty.total_size = 0;
        empty.bitmap = Vec::new();
        let started = start_transfer(&mut tx, &empty).await.expect("开始空内容");
        assert_eq!(started.chunk_count(), 0);
        assert!(
            complete_transfer(&mut tx, binding, version_id(0x4b))
                .await
                .expect("空内容没有缺块"),
            "空内容可以直接完成"
        );

        let mut unknown = new_transfer(binding, 0x4c);
        unknown.binding = crate::test_support::binding_id(0xFC);
        assert!(
            start_transfer(&mut tx, &unknown).await.is_err(),
            "不存在的绑定必须被外键拒绝"
        );
        tx.commit().await.expect("提交");
        database.close().await;
    }
}
