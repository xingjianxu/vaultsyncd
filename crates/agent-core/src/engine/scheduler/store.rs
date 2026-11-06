//! SQLite 短事务适配：失败计数、退避和离线事实一起提交。
use super::{Priority, TaskStatus};
use crate::ports::{LocalErrorKind, PortError, state::QueuedOperation};
use sqlx::Row;
use vaultsyncd_client_store::{db::Database, queue};
use vaultsyncd_domain::{ids::OpId, version::UtcTimestamp};

pub(super) fn error(e: impl std::fmt::Display) -> PortError {
    PortError::local(LocalErrorKind::Write, format!("调度状态：{e}"))
}
pub(super) async fn ensure(
    db: &Database,
    op: &QueuedOperation,
    priority: Priority,
) -> Result<(), PortError> {
    sqlx::query("INSERT INTO operation_schedule(op_id,priority) VALUES (?1,?2) ON CONFLICT(op_id) DO NOTHING")
        .bind(op.op.as_bytes().as_slice()).bind(priority as i64).execute(db.pool()).await.map_err(error)?;
    Ok(())
}
pub(super) async fn status(db: &Database, op: &QueuedOperation) -> Result<TaskStatus, PortError> {
    let row = sqlx::query(
        "SELECT priority,due_at,cancelled,halted,offline FROM operation_schedule WHERE op_id=?1",
    )
    .bind(op.op.as_bytes().as_slice())
    .fetch_one(db.pool())
    .await
    .map_err(error)?;
    let priority = match row.get::<i64, _>("priority") {
        0 => Priority::UserRead,
        1 => Priority::Metadata,
        2 => Priority::Background,
        3 => Priority::Hash,
        _ => return Err(PortError::protocol("未知调度优先级")),
    };
    Ok(TaskStatus {
        op: op.op,
        binding: op.binding,
        priority,
        due: UtcTimestamp::from_db_integer(row.get("due_at")).map_err(error)?,
        cancelled: row.get("cancelled"),
        halted: row.get("halted"),
        offline: row.get("offline"),
        attempts: op.attempts,
        error: op.last_error,
    })
}
pub(super) async fn priority(db: &Database, op: OpId, priority: Priority) -> Result<(), PortError> {
    let result=sqlx::query("INSERT INTO operation_schedule(op_id,priority) VALUES (?1,?2) ON CONFLICT(op_id) DO UPDATE SET priority=excluded.priority")
        .bind(op.as_bytes().as_slice()).bind(priority as i64).execute(db.pool()).await.map_err(error)?;
    if result.rows_affected() != 1 {
        return Err(PortError::protocol("调度操作不存在"));
    }
    Ok(())
}
pub(super) async fn cancel(db: &Database, op: OpId) -> Result<(), PortError> {
    let result = sqlx::query("UPDATE operation_schedule SET cancelled=1 WHERE op_id=?1")
        .bind(op.as_bytes().as_slice())
        .execute(db.pool())
        .await
        .map_err(error)?;
    if result.rows_affected() != 1 {
        return Err(PortError::protocol("取消操作不存在"));
    }
    Ok(())
}
pub(super) async fn pending(db: &Database, op: OpId, due: UtcTimestamp) -> Result<(), PortError> {
    let hit = sqlx::query("UPDATE operation_schedule SET due_at=?1,offline=0 WHERE op_id=?2")
        .bind(due.to_db_integer())
        .bind(op.as_bytes().as_slice())
        .execute(db.pool())
        .await
        .map_err(error)?;
    if hit.rows_affected() != 1 {
        return Err(PortError::protocol("pending操作的调度记录丢失"));
    }
    Ok(())
}
pub(super) async fn failure(
    db: &Database,
    op: &QueuedOperation,
    code: vaultsyncd_domain::error::ErrorCode,
    due: UtcTimestamp,
    halted: bool,
    offline: bool,
) -> Result<(), PortError> {
    let mut tx = db.begin_write().await.map_err(error)?;
    // CAS 防止并行/过时调度器重复记同一次结果；任一写失败均回滚。
    let current: Option<i64> =
        sqlx::query_scalar("SELECT attempts FROM pending_ops WHERE op_id=?1")
            .bind(op.op.as_bytes().as_slice())
            .fetch_optional(&mut *tx)
            .await
            .map_err(error)?;
    if current != Some(i64::from(op.attempts)) {
        return Err(PortError::protocol("操作失败计数已改变"));
    }
    if !queue::record_attempt_failure(&mut tx, op.op, code)
        .await
        .map_err(error)?
    {
        return Err(PortError::protocol("失败操作丢失"));
    }
    let result =
        sqlx::query("UPDATE operation_schedule SET due_at=?1,halted=?2,offline=?3 WHERE op_id=?4")
            .bind(due.to_db_integer())
            .bind(halted)
            .bind(offline)
            .bind(op.op.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .map_err(error)?;
    if result.rows_affected() != 1 {
        return Err(PortError::protocol("失败操作的调度记录丢失"));
    }
    tx.commit().await.map_err(error)
}
pub(super) async fn complete(db: &Database, op: &QueuedOperation) -> Result<(), PortError> {
    crate::recovery::resync::ensure_operation(db, op.binding, op.op).await?;
    let mut tx = db.begin_write().await.map_err(error)?;
    // 只有动作已Committed才解除依赖；保留每个依赖方本身，不级联删除。
    sqlx::query("UPDATE pending_ops SET depends_on=NULL WHERE depends_on=?1")
        .bind(op.op.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .map_err(error)?;
    if !queue::complete(&mut tx, op.op).await.map_err(error)? {
        return Err(PortError::protocol("完成操作丢失"));
    }
    tx.commit().await.map_err(error)
}
