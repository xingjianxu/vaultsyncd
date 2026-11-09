//! 事件批次、接收游标与应用游标的事务内读写（`docs/design.md` §7.3、§9.3、I09）。
//!
//! 边界：本模块只持久化“收到了哪个批次、批次里有什么、要落地哪些任务、落到哪一步了”。
//! 它不拉事件、不解析事件的业务语义、不碰文件系统、不执行任务：wire 变更列表由应用层映射成
//! [`RemoteEntry`] 与 [`NewPendingOp`] 后交给这里。
//!
//! 不变量：
//!
//! - **批次与工作同事务**：[`receive_batch`] 在调用方给的 `BEGIN IMMEDIATE` 事务里依次写
//!   `received_changes`、`remote_entries`、`pending_ops`，**最后**才推进接收游标（design §9.3、
//!   I09：不能先推进游标再写任务）。任何一步失败都会让调用方回滚整个事务，游标因此不会领先于
//!   已持久化的批次与任务。
//! - **一个 VaultSeq 就是一个不可拆分批次**：`(binding_id, seq)` 是主键，批次不以条目分页；
//!   序号从 1 起，游标 0 表示“还没有批次”。
//! - **重复批次幂等**：同一 `(binding, seq)` 与同一 `tx_id`/事件列表重复到达时只返回已存记录，
//!   不重复入队（design §9.3：重复、丢失、乱序都由拉取与去重修复）；同一序号却是不同批次则
//!   是服务器/客户端状态不一致，报 [`EventError::ConflictingBatch`]，不静默覆盖。
//! - **应用游标只是“连续已应用前缀”**：它不会越过任何一个未应用的批次。某文件失败不阻止接收
//!   后续事件，但相关依赖与应用进度不得越过它（design §9.3、§8.4）。应用游标由
//!   [`mark_batch_applied`] 在同一事务里推进，与接收游标是两个独立的进展。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{BindingId, ID_LEN, OpId, TxId};
use vaultsyncd_domain::version::VaultSeq;

use crate::entries::{self, EntryError, RemoteEntry};
use crate::queue::{self, NewPendingOp, QueueError};

/// 一个已持久化批次的应用结果（`received_changes.state`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchState {
    /// 已持久收到，尚未全部应用到本地。
    Received,
    /// 批次内条目都已落盘并更新基线。
    Applied,
    /// 至少一个条目停止自动推进；原因见 [`ReceivedBatch::last_error`]。
    Blocked,
}

impl BatchState {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Applied => "applied",
            Self::Blocked => "blocked",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜默认状态。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "received" => Some(Self::Received),
            "applied" => Some(Self::Applied),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

/// 一个已持久化的事件批次（`received_changes` 一行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedBatch {
    /// 批次序号（VaultSeq，从 1 起）。
    pub seq: VaultSeq,
    /// 服务端事务批次身份。
    pub tx: TxId,
    /// 收到的原始事件列表（JSON 数组文本）。
    pub events_json: String,
    /// 应用结果。
    pub state: BatchState,
    /// 阻塞原因；只有 [`BatchState::Blocked`] 有。
    pub last_error: Option<ErrorCode>,
}

/// 某个绑定的接收与应用游标。
///
/// `received` 是已持久收到批次的最大 VaultSeq，`applied` 是这些批次里连续应用完成的最大 VaultSeq；
/// 0 表示“还没有”。两者不同步是正常状态：远端事件已收到但文件尚未落盘（design §9.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursors {
    /// 接收游标。
    pub received: VaultSeq,
    /// 应用游标。
    pub applied: VaultSeq,
}

/// 待收下的一批事件及其工作。
///
/// 不变量（违反时 [`receive_batch`] 报错且不写入）：`seq` 大于 0；`events_json` 是非空的 JSON
/// 数组文本；每条 `changes` 的位置形状合法（见 [`entries::put_remote`]）；每条 `tasks` 属于同一
/// 绑定、按依赖顺序给出（见 [`queue::enqueue`]）。
#[derive(Clone, Debug)]
pub struct IncomingBatch {
    /// 所属绑定。
    pub binding: BindingId,
    /// 批次序号；同一批次只收一次。
    pub seq: VaultSeq,
    /// 服务端事务批次身份。
    pub tx: TxId,
    /// 原始事件列表（JSON 数组文本，wire 变更列表原样保存）。
    pub events_json: String,
    /// 本批次带来的远端状态，写入 `remote_entries`（同事务）。
    pub changes: Vec<RemoteEntry>,
    /// 本批次需要落地的工作，按依赖顺序入队 `pending_ops`（同事务）。
    pub tasks: Vec<NewPendingOp>,
}

/// 事件批次读写的失败。
#[derive(Debug)]
pub enum EventError {
    /// SQLite 层面失败（含外键与 `CHECK` 拒绝）。
    Db(sqlx::Error),
    /// 绑定不存在；游标与批次都只能挂在已有绑定上。
    UnknownBinding {
        /// 缺失的绑定。
        binding: BindingId,
    },
    /// 批次序号 0 不是批次（0 只是“还没有批次”的游标值）。
    InvalidSeq,
    /// 事件列表不是非空 JSON 数组文本。
    InvalidEvents {
        /// 违反的具体规则。
        reason: &'static str,
    },
    /// 同一 `(binding, seq)` 已有不同 `tx_id` 或不同事件列表：这是状态不一致，不是重复。
    ConflictingBatch {
        /// 冲突的批次序号。
        seq: VaultSeq,
    },
    /// 新批次的序号不大于接收游标：这个位置已经收过了，不能用另一批覆盖。
    StaleBatch {
        /// 被拒绝的批次序号。
        seq: VaultSeq,
        /// 当前接收游标。
        received: VaultSeq,
    },
    /// 任务的绑定与批次绑定不一致：一个批次只落一个绑定的工作。
    TaskBindingMismatch {
        /// 出问题的操作。
        op: OpId,
    },
    /// 远端状态写入失败（位置形状、外键或坏值）。
    Entry(EntryError),
    /// 任务入队失败（重复、缺失依赖或坏值）。
    Queue(QueueError),
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl From<EntryError> for EventError {
    fn from(error: EntryError) -> Self {
        Self::Entry(error)
    }
}

impl From<QueueError> for EventError {
    fn from(error: QueueError) -> Self {
        Self::Queue(error)
    }
}

impl std::fmt::Display for EventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::UnknownBinding { binding } => write!(f, "绑定 {binding} 不存在"),
            Self::InvalidSeq => f.write_str("批次序号必须大于 0"),
            Self::InvalidEvents { reason } => write!(f, "事件列表不合法：{reason}"),
            Self::ConflictingBatch { seq } => {
                write!(f, "批次 {seq} 已存在且内容不同，不能覆盖")
            }
            Self::StaleBatch { seq, received } => {
                write!(
                    f,
                    "批次 {seq} 不大于接收游标 {received}，不能用它覆盖已收位置"
                )
            }
            Self::TaskBindingMismatch { op } => {
                write!(f, "任务 {op} 的绑定与批次绑定不一致")
            }
            Self::Entry(source) => write!(f, "远端状态写入失败：{source}"),
            Self::Queue(source) => write!(f, "任务入队失败：{source}"),
            Self::MalformedRow { column } => {
                write!(f, "数据库列 {column} 的值不符合类型前提")
            }
        }
    }
}

impl std::error::Error for EventError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::Entry(source) => Some(source),
            Self::Queue(source) => Some(source),
            Self::UnknownBinding { .. }
            | Self::InvalidSeq
            | Self::InvalidEvents { .. }
            | Self::ConflictingBatch { .. }
            | Self::StaleBatch { .. }
            | Self::TaskBindingMismatch { .. }
            | Self::MalformedRow { .. } => None,
        }
    }
}

/// `received_changes` 的列顺序。
const BATCH_COLUMNS: &str = "seq, tx_id, events_json, state, last_error_code";

/// 收下一个事件批次并在同一事务里写完它的工作，最后推进接收游标。
///
/// 顺序固定为：批次行 → 远端状态 → 应用任务 → 接收游标。游标最后写，是因为它可以被当作
/// “这个位置之前都已持久化”的证据（I09）；三步任何一步失败，调用方必须回滚整个事务，
/// 已写入的部分随之消失，游标保持原值。
///
/// 重复收到同一批次（同序号、同事务、同事件列表）返回已存记录且**不重复入队**；同序号却是
/// 不同批次报 [`EventError::ConflictingBatch`]；序号不大于接收游标报
/// [`EventError::StaleBatch`]。
pub async fn receive_batch(
    tx: &mut Transaction<'_, Sqlite>,
    batch: &IncomingBatch,
) -> Result<ReceivedBatch, EventError> {
    if batch.seq == VaultSeq::ZERO {
        return Err(EventError::InvalidSeq);
    }
    validate_events_json(&batch.events_json)?;
    for task in &batch.tasks {
        if task.binding != batch.binding {
            return Err(EventError::TaskBindingMismatch { op: task.op });
        }
    }

    let current = cursors(tx, batch.binding).await?;
    if let Some(existing) = received_batch(tx, batch.binding, batch.seq).await? {
        if existing.tx == batch.tx && existing.events_json == batch.events_json {
            return Ok(existing);
        }
        return Err(EventError::ConflictingBatch { seq: batch.seq });
    }
    if batch.seq <= current.received {
        return Err(EventError::StaleBatch {
            seq: batch.seq,
            received: current.received,
        });
    }

    sqlx::query(
        "INSERT INTO received_changes (binding_id, seq, tx_id, events_json, state) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(batch.binding.as_bytes().as_slice())
    .bind(batch.seq.to_db_integer())
    .bind(batch.tx.as_bytes().as_slice())
    .bind(&batch.events_json)
    .bind(BatchState::Received.as_text())
    .execute(&mut **tx)
    .await
    .map_err(EventError::Db)?;

    for change in &batch.changes {
        entries::put_remote(tx, batch.binding, change).await?;
    }
    for task in &batch.tasks {
        queue::enqueue(tx, task).await?;
    }

    // 游标最后推进：它只在这批的批次行、远端状态与任务都已写入后才前进（I09）。
    let advanced = sqlx::query("UPDATE bindings SET received_seq = ?1 WHERE id = ?2")
        .bind(batch.seq.to_db_integer())
        .bind(batch.binding.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(EventError::Db)?;
    if advanced.rows_affected() == 0 {
        return Err(EventError::UnknownBinding {
            binding: batch.binding,
        });
    }

    Ok(ReceivedBatch {
        seq: batch.seq,
        tx: batch.tx,
        events_json: batch.events_json.clone(),
        state: BatchState::Received,
        last_error: None,
    })
}

/// 读一个绑定的一对游标；绑定不存在报 [`EventError::UnknownBinding`]。
pub async fn cursors(
    conn: &mut SqliteConnection,
    binding: BindingId,
) -> Result<Cursors, EventError> {
    let row: Option<(i64, i64)> =
        sqlx::query_as("SELECT received_seq, applied_seq FROM bindings WHERE id = ?1")
            .bind(binding.as_bytes().as_slice())
            .fetch_optional(conn)
            .await
            .map_err(EventError::Db)?;
    let (received, applied) = row.ok_or(EventError::UnknownBinding { binding })?;
    Ok(Cursors {
        received: seq_from_db(received, "received_seq")?,
        applied: seq_from_db(applied, "applied_seq")?,
    })
}

/// 读一个批次；不存在返回 `None`。
pub async fn received_batch(
    conn: &mut SqliteConnection,
    binding: BindingId,
    seq: VaultSeq,
) -> Result<Option<ReceivedBatch>, EventError> {
    let row = sqlx::query_as::<_, BatchRow>(&format!(
        "SELECT {BATCH_COLUMNS} FROM received_changes WHERE binding_id = ?1 AND seq = ?2"
    ))
    .bind(binding.as_bytes().as_slice())
    .bind(seq.to_db_integer())
    .fetch_optional(conn)
    .await
    .map_err(EventError::Db)?;
    row.map(BatchRow::into_record).transpose()
}

/// 列出所有尚未应用完的批次（按序号升序）：重启后的重放清单。
///
/// 包含 `received` 与 `blocked` 两种状态：阻塞的批次同样必须先被处理，否则应用进度无法越过它。
pub async fn unapplied_batches(
    conn: &mut SqliteConnection,
    binding: BindingId,
) -> Result<Vec<ReceivedBatch>, EventError> {
    let rows = sqlx::query_as::<_, BatchRow>(&format!(
        "SELECT {BATCH_COLUMNS} FROM received_changes \
         WHERE binding_id = ?1 AND state <> ?2 ORDER BY seq"
    ))
    .bind(binding.as_bytes().as_slice())
    .bind(BatchState::Applied.as_text())
    .fetch_all(conn)
    .await
    .map_err(EventError::Db)?;
    rows.into_iter().map(BatchRow::into_record).collect()
}

/// 标记一个批次已应用，并把应用游标推进到“连续已应用前缀”的末端。返回是否命中该批次。
///
/// 应用游标单独于接收游标（design §9.3）：接收早已到位时，只有文件落盘、基线更新完成的批次才
/// 会让它前进。中间存在未应用（或阻塞）的批次时，游标停在那之前，不会被后面的批次带过去。
pub async fn mark_batch_applied(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    seq: VaultSeq,
) -> Result<bool, EventError> {
    let updated = sqlx::query(
        "UPDATE received_changes SET state = ?1, last_error_code = NULL \
         WHERE binding_id = ?2 AND seq = ?3 AND state <> ?1",
    )
    .bind(BatchState::Applied.as_text())
    .bind(binding.as_bytes().as_slice())
    .bind(seq.to_db_integer())
    .execute(&mut **tx)
    .await
    .map_err(EventError::Db)?;
    if updated.rows_affected() == 0 {
        return Ok(false);
    }
    advance_applied_prefix(tx, binding).await?;
    Ok(true)
}

/// 标记一个批次阻塞（有条目停止自动推进）并记下原因。返回是否命中该批次。
///
/// 阻塞不撤掉批次行：`unapplied_batches` 仍会列出它，应用游标停在它之前；处理完原因后仍可用
/// [`mark_batch_applied`] 把它转为已应用。
pub async fn mark_batch_blocked(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    seq: VaultSeq,
    code: ErrorCode,
) -> Result<bool, EventError> {
    let updated = sqlx::query(
        "UPDATE received_changes SET state = ?1, last_error_code = ?2 \
         WHERE binding_id = ?3 AND seq = ?4",
    )
    .bind(BatchState::Blocked.as_text())
    .bind(code.as_str())
    .bind(binding.as_bytes().as_slice())
    .bind(seq.to_db_integer())
    .execute(&mut **tx)
    .await
    .map_err(EventError::Db)?;
    Ok(updated.rows_affected() > 0)
}

/// 把应用游标推进到连续已应用批次的末端，返回推进后的游标。
async fn advance_applied_prefix(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
) -> Result<Cursors, EventError> {
    // 第一个未应用批次的序号；没有这样的批次说明收到过的都已应用。
    let blocking: Option<i64> = sqlx::query_scalar(
        "SELECT seq FROM received_changes \
         WHERE binding_id = ?1 AND state <> ?2 ORDER BY seq LIMIT 1",
    )
    .bind(binding.as_bytes().as_slice())
    .bind(BatchState::Applied.as_text())
    .fetch_optional(&mut **tx)
    .await
    .map_err(EventError::Db)?;

    let current = cursors(tx, binding).await?;
    let target = match blocking {
        Some(seq) => {
            let blocking = seq_from_db(seq, "seq")?;
            if blocking > current.received {
                current.received
            } else {
                // 已应用前缀止于该批次之前；批次序号从 1 起，没有下溢。
                VaultSeq::new(blocking.get() - 1)
                    .map_err(|_| EventError::MalformedRow { column: "seq" })?
            }
        }
        None => current.received,
    };
    if target <= current.applied {
        return Ok(current);
    }

    sqlx::query("UPDATE bindings SET applied_seq = ?1 WHERE id = ?2")
        .bind(target.to_db_integer())
        .bind(binding.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(EventError::Db)?;
    Ok(Cursors {
        received: current.received,
        applied: target,
    })
}

/// 事件列表的形状检查。完整 JSON 合法性由数据库 `CHECK (json_valid(...))` 兜底：本层不拥有
/// JSON 编解码器，只在这里把最常见的错误给出明确原因。
fn validate_events_json(text: &str) -> Result<(), EventError> {
    let trimmed = text.trim();
    if trimmed.len() <= 2 || !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Err(EventError::InvalidEvents {
            reason: "必须是非空的 JSON 数组文本（例如 [{\"…\"}]）",
        });
    }
    Ok(())
}

fn seq_from_db(value: i64, column: &'static str) -> Result<VaultSeq, EventError> {
    VaultSeq::from_db_integer(value).map_err(|_| EventError::MalformedRow { column })
}

/// 数据库行到 [`ReceivedBatch`] 的中间形态。
#[derive(sqlx::FromRow)]
struct BatchRow {
    seq: i64,
    tx_id: Vec<u8>,
    events_json: String,
    state: String,
    last_error_code: Option<String>,
}

impl BatchRow {
    fn into_record(self) -> Result<ReceivedBatch, EventError> {
        let tx: [u8; ID_LEN] = self
            .tx_id
            .as_slice()
            .try_into()
            .map_err(|_| EventError::MalformedRow { column: "tx_id" })?;
        Ok(ReceivedBatch {
            seq: seq_from_db(self.seq, "seq")?,
            tx: TxId::from_bytes(tx),
            events_json: self.events_json,
            state: BatchState::from_text(&self.state)
                .ok_or(EventError::MalformedRow { column: "state" })?,
            last_error: self
                .last_error_code
                .map(|text| {
                    ErrorCode::from_code(&text).ok_or(EventError::MalformedRow {
                        column: "last_error_code",
                    })
                })
                .transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use vaultsyncd_domain::ids::{EntryId, VersionId};
    use vaultsyncd_domain::version::Revision;

    use crate::entries::EntryKind;
    use crate::test_support::{
        TestDir, binding_id, entry_id, op_id, open_database, seed_binding, tx_id, version_id,
    };

    const EVENTS_ONE: &str = r#"[{"entry":"one"}]"#;
    const EVENTS_TWO: &str = r#"[{"entry":"two"}]"#;

    fn seq(value: u64) -> VaultSeq {
        VaultSeq::new(value).expect("批次序号")
    }

    fn revision(value: u64) -> Revision {
        Revision::new(value).expect("修订")
    }

    fn remote(entry: EntryId, name: &str, version: VersionId) -> RemoteEntry {
        RemoteEntry {
            entry,
            parent: Some(entry_id(0x01)),
            name: name.to_owned(),
            kind: EntryKind::File,
            revision: revision(4),
            content_version: Some(version),
            deleted: false,
        }
    }

    fn task(marker: u8, binding: BindingId, entry: EntryId) -> NewPendingOp {
        NewPendingOp {
            op: op_id(marker),
            binding,
            kind: "download_content".to_owned(),
            entry: Some(entry),
            baseline_revision: Some(revision(3)),
            baseline_version: Some(version_id(0x41)),
            depends_on: None,
            pinned_version: None,
        }
    }

    fn batch(marker: u8, binding: BindingId, seq_value: u64) -> IncomingBatch {
        IncomingBatch {
            binding,
            seq: seq(seq_value),
            tx: tx_id(marker),
            events_json: EVENTS_ONE.to_owned(),
            changes: vec![remote(entry_id(0x11), "a.txt", version_id(0x41))],
            tasks: vec![task(0x31, binding, entry_id(0x11))],
        }
    }

    #[tokio::test]
    async fn receiving_a_batch_persists_events_tasks_and_both_cursors() {
        let dir = TestDir::new("receive");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x21).await;

        let mut incoming = batch(0x31, binding, 5);
        incoming
            .changes
            .push(remote(entry_id(0x12), "dir/b.txt", version_id(0x42)));
        let mut dependent = task(0x32, binding, entry_id(0x12));
        dependent.depends_on = Some(op_id(0x31));
        incoming.tasks.push(dependent);

        let mut tx = database.begin_write().await.expect("开始写事务");
        let stored = receive_batch(&mut tx, &incoming).await.expect("接收批次");
        assert_eq!(stored.seq, seq(5));
        assert_eq!(stored.tx, tx_id(0x31));
        assert_eq!(stored.events_json, EVENTS_ONE);
        assert_eq!(stored.state, BatchState::Received);
        assert_eq!(stored.last_error, None);

        // 接收游标已到本批，应用游标仍是 0：接收不等于应用。
        assert_eq!(
            cursors(&mut tx, binding).await.expect("读游标"),
            Cursors {
                received: seq(5),
                applied: VaultSeq::ZERO,
            }
        );
        tx.commit().await.expect("提交");

        // 远端状态与任务都在同一个事务里落库，且任务保持依赖顺序。
        let mut conn = database.pool().acquire().await.expect("取连接");
        let stored_remote = entries::remote_entry(&mut conn, binding, entry_id(0x12))
            .await
            .expect("读远端")
            .expect("远端行必须存在");
        assert_eq!(stored_remote.name, "dir/b.txt");
        assert_eq!(stored_remote.content_version, Some(version_id(0x42)));
        let queued = queue::pending_ops_for_binding(&mut conn, binding)
            .await
            .expect("读队列");
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0].op, op_id(0x31));
        assert_eq!(queued[1].depends_on, Some(op_id(0x31)));
        assert_eq!(
            unapplied_batches(&mut conn, binding)
                .await
                .expect("列未应用批次"),
            vec![stored]
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn failed_transaction_leaves_cursor_and_queue_untouched() {
        let dir = TestDir::new("receive-rollback");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x22).await;

        // 依赖指向本批次里不存在的操作：入队在写完批次行之后失败。
        let mut incoming = batch(0x41, binding, 1);
        let mut dangling = task(0x42, binding, entry_id(0x11));
        dangling.depends_on = Some(op_id(0x43));
        incoming.tasks.push(dangling);

        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = receive_batch(&mut tx, &incoming)
            .await
            .expect_err("缺失依赖必须让整个批次失败");
        assert!(matches!(error, EventError::Queue(_)), "实际错误 {error:?}");

        // 事务内已写入的批次行还在（顺序是批次行先写），但游标没有前进：
        // 游标在最后一步才写，失败时停在原值。
        assert!(
            received_batch(&mut tx, binding, seq(1))
                .await
                .expect("读批次")
                .is_some()
        );
        assert_eq!(
            cursors(&mut tx, binding).await.expect("读游标").received,
            VaultSeq::ZERO,
            "任务失败时接收游标不得前进"
        );
        tx.rollback().await.expect("回滚");

        // 回滚后数据库回到接收前：没有批次、没有远端状态、没有任务、游标为 0。
        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            received_batch(&mut conn, binding, seq(1))
                .await
                .expect("读批次"),
            None
        );
        assert_eq!(
            entries::remote_entry(&mut conn, binding, entry_id(0x11))
                .await
                .expect("读远端"),
            None
        );
        assert!(
            queue::pending_ops_for_binding(&mut conn, binding)
                .await
                .expect("读队列")
                .is_empty()
        );
        assert_eq!(
            cursors(&mut conn, binding).await.expect("读游标"),
            Cursors {
                received: VaultSeq::ZERO,
                applied: VaultSeq::ZERO,
            }
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn duplicate_batch_is_idempotent_conflicting_or_stale_batches_are_rejected() {
        let dir = TestDir::new("receive-duplicate");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x23).await;

        let incoming = batch(0x51, binding, 2);
        let mut tx = database.begin_write().await.expect("开始写事务");
        let first = receive_batch(&mut tx, &incoming).await.expect("首次接收");
        let again = receive_batch(&mut tx, &incoming).await.expect("重复接收");
        assert_eq!(again, first, "重复批次返回已存记录");

        // 同一序号、不同事件列表：状态不一致，不覆盖。
        let mut conflicting = incoming.clone();
        conflicting.events_json = EVENTS_TWO.to_owned();
        assert!(matches!(
            receive_batch(&mut tx, &conflicting)
                .await
                .expect_err("同序号不同内容必须被拒绝"),
            EventError::ConflictingBatch { .. }
        ));

        // 序号不大于游标的新批次：这个位置已经收过。
        let mut stale = batch(0x52, binding, 1);
        stale.events_json = EVENTS_TWO.to_owned();
        stale.changes.clear();
        stale.tasks.clear();
        assert!(matches!(
            receive_batch(&mut tx, &stale)
                .await
                .expect_err("不比游标新的批次必须被拒绝"),
            EventError::StaleBatch { .. }
        ));
        tx.commit().await.expect("提交");

        // 重复批次没有把任务入队两次。
        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            queue::pending_ops_for_binding(&mut conn, binding)
                .await
                .expect("读队列")
                .len(),
            1
        );
        assert_eq!(
            cursors(&mut conn, binding).await.expect("读游标").received,
            seq(2)
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn invalid_batches_are_rejected_before_any_write() {
        let dir = TestDir::new("receive-invalid");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x24).await;

        let mut tx = database.begin_write().await.expect("开始写事务");

        let mut zero_seq = batch(0x61, binding, 1);
        zero_seq.seq = VaultSeq::ZERO;
        assert!(matches!(
            receive_batch(&mut tx, &zero_seq)
                .await
                .expect_err("序号 0 不是批次"),
            EventError::InvalidSeq
        ));

        for invalid in ["", "[]", "not-json", "{\"a\":1}"] {
            let mut bad_events = batch(0x62, binding, 1);
            bad_events.events_json = invalid.to_owned();
            assert!(
                matches!(
                    receive_batch(&mut tx, &bad_events)
                        .await
                        .expect_err("非 JSON 数组必须被拒绝"),
                    EventError::InvalidEvents { .. }
                ),
                "text={invalid:?}"
            );
        }

        let mut other_binding = batch(0x63, binding, 1);
        other_binding
            .tasks
            .push(task(0x64, binding_id(0xFE), entry_id(0x11)));
        assert!(matches!(
            receive_batch(&mut tx, &other_binding)
                .await
                .expect_err("任务绑定必须与批次一致"),
            EventError::TaskBindingMismatch { .. }
        ));

        let mut unknown_binding = batch(0x65, binding, 1);
        unknown_binding.binding = binding_id(0xFD);
        unknown_binding.tasks.clear();
        assert!(matches!(
            receive_batch(&mut tx, &unknown_binding)
                .await
                .expect_err("绑定必须存在"),
            EventError::UnknownBinding { .. }
        ));
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            cursors(&mut conn, binding).await.expect("读游标"),
            Cursors {
                received: VaultSeq::ZERO,
                applied: VaultSeq::ZERO,
            }
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn applied_cursor_moves_only_over_the_contiguous_applied_prefix() {
        let dir = TestDir::new("applied");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x25).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        for (marker, seq_value) in [(0x71, 1), (0x72, 2), (0x73, 3)] {
            let mut incoming = batch(marker, binding, seq_value);
            incoming.changes.clear();
            incoming.tasks.clear();
            receive_batch(&mut tx, &incoming).await.expect("接收批次");
        }
        assert!(
            mark_batch_applied(&mut tx, binding, seq(1))
                .await
                .expect("标记第一批已应用")
        );
        assert_eq!(
            cursors(&mut tx, binding).await.expect("读游标"),
            Cursors {
                received: seq(3),
                applied: seq(1),
            },
            "应用游标只越过连续已应用前缀"
        );

        // 第三批先应用：第二批还没应用，游标不得越过它。
        assert!(
            mark_batch_applied(&mut tx, binding, seq(3))
                .await
                .expect("标记第三批已应用")
        );
        assert_eq!(
            cursors(&mut tx, binding).await.expect("读游标").applied,
            seq(1)
        );

        // 第二批阻塞：仍是未应用，游标停在 1，且原因被记录。
        assert!(
            mark_batch_blocked(&mut tx, binding, seq(2), ErrorCode::BaseMismatch)
                .await
                .expect("标记阻塞")
        );
        let blocked = received_batch(&mut tx, binding, seq(2))
            .await
            .expect("读批次")
            .expect("批次必须存在");
        assert_eq!(blocked.state, BatchState::Blocked);
        assert_eq!(blocked.last_error, Some(ErrorCode::BaseMismatch));
        assert_eq!(
            unapplied_batches(&mut tx, binding)
                .await
                .expect("列未应用批次")
                .iter()
                .map(|batch| batch.seq)
                .collect::<Vec<_>>(),
            vec![seq(2)]
        );

        // 阻塞处理后可以转为已应用，游标随之越过 2 和 3。
        assert!(
            mark_batch_applied(&mut tx, binding, seq(2))
                .await
                .expect("解除阻塞")
        );
        assert_eq!(
            cursors(&mut tx, binding).await.expect("读游标"),
            Cursors {
                received: seq(3),
                applied: seq(3),
            }
        );
        assert!(
            !mark_batch_applied(&mut tx, binding, seq(9))
                .await
                .expect("不存在的批次不命中")
        );
        tx.commit().await.expect("提交");
        database.close().await;
    }

    #[tokio::test]
    async fn restart_keeps_unapplied_batches_cursors_and_tasks() {
        let dir = TestDir::new("restart");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x26).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let first = batch(0x81, binding, 1);
        receive_batch(&mut tx, &first).await.expect("接收第一批");
        let mut second = batch(0x82, binding, 2);
        second.events_json = EVENTS_TWO.to_owned();
        second.changes = vec![remote(entry_id(0x12), "b.txt", version_id(0x42))];
        second.tasks = vec![task(0x33, binding, entry_id(0x12))];
        receive_batch(&mut tx, &second).await.expect("接收第二批");
        mark_batch_applied(&mut tx, binding, seq(1))
            .await
            .expect("标记第一批已应用");
        tx.commit().await.expect("提交");
        database.close().await;

        // 重启：批次、游标与未完成的下载任务都还在。
        let reopened = open_database(&dir).await;
        let mut conn = reopened.pool().acquire().await.expect("取连接");
        assert_eq!(
            cursors(&mut conn, binding).await.expect("读游标"),
            Cursors {
                received: seq(2),
                applied: seq(1),
            }
        );
        let pending = unapplied_batches(&mut conn, binding)
            .await
            .expect("列未应用批次");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].seq, seq(2));
        assert_eq!(pending[0].events_json, EVENTS_TWO);
        assert_eq!(pending[0].tx, tx_id(0x82));
        let queued = queue::pending_ops_for_binding(&mut conn, binding)
            .await
            .expect("读队列");
        assert_eq!(queued.len(), 2, "两个批次的任务都保留");
        assert_eq!(queued[1].op, op_id(0x33));
        drop(conn);
        reopened.close().await;
    }

    #[tokio::test]
    async fn unknown_error_codes_are_rejected_on_read() {
        let dir = TestDir::new("malformed");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x27).await;

        // 结构合法但错误码不属于 design §15.4：读取时按坏行拒绝，而不是当成可重试错误。
        let mut tx = database.begin_write().await.expect("开始写事务");
        sqlx::query(
            "INSERT INTO received_changes (binding_id, seq, tx_id, events_json, state, last_error_code) \
             VALUES (?1, 1, ?2, ?3, 'blocked', 'NOT_A_CODE')",
        )
        .bind(binding.as_bytes().as_slice())
        .bind(tx_id(0x91).as_bytes().as_slice())
        .bind(EVENTS_ONE)
        .execute(&mut *tx)
        .await
        .expect("写坏行");
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let error = received_batch(&mut conn, binding, seq(1))
            .await
            .expect_err("坏行必须被拒绝");
        assert!(
            matches!(
                error,
                EventError::MalformedRow {
                    column: "last_error_code"
                }
            ),
            "实际错误 {error:?}"
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn database_check_rejects_events_that_are_not_json_arrays() {
        let dir = TestDir::new("events-check");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x28).await;

        // Rust 层只做形状预检；真正的 JSON 合法性由 `CHECK (json_valid(...))` 与数组类型兜底。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let insert = sqlx::query(
            "INSERT INTO received_changes (binding_id, seq, tx_id, events_json, state) \
             VALUES (?1, 1, ?2, ?3, 'received')",
        )
        .bind(binding.as_bytes().as_slice())
        .bind(tx_id(0x91).as_bytes().as_slice())
        .bind("[not json]")
        .execute(&mut *tx)
        .await;
        assert!(insert.is_err(), "非 JSON 数组必须被数据库 CHECK 拒绝");
        tx.rollback().await.expect("回滚");
        database.close().await;
    }
}
