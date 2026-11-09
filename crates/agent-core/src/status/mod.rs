//! 客户端多维持久状态；pending、未知内容和恢复围栏均不等于同步完成。
pub mod diagnostics;
pub mod issues;
use sqlx::Row;
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::ids::BindingId;
use vaultsyncd_transfer::ports::PortError;
/// 独立子任务计数；不能用一个绿色状态隐藏仍在执行的任务。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BindingStatus {
    /// 本地未确认内容数量。
    pub dirty: u64,
    /// 元数据占位、部分驻留、完整驻留数量。
    pub residency: [u64; 3],
    /// 尚未执行完的队列。
    pub pending: u64,
    /// 失败队列数量。
    pub failed: u64,
    /// 持续可见的普通断网任务。
    pub offline: u64,
    /// 未完成传输。
    pub transfers: u64,
    /// 开放本地意图。
    pub intents: u64,
    /// 未完成导入、应用、释放和旧设备重建。
    pub recovery: u64,
    /// received与applied间隔。
    pub event_lag: u64,
    /// 模式补齐仍未完成。
    pub mode_pending: bool,
    /// 暂停。
    pub paused: bool,
    /// 已解除绑定。
    pub detached: bool,
    /// 未核对的完整内容/未重协调条目。
    pub unknown: u64,
}
impl BindingStatus {
    /// 只有所有业务维度均无未完成事实时才完成。
    pub fn synchronized(&self) -> bool {
        !self.detached
            && [
                self.dirty,
                self.pending,
                self.transfers,
                self.intents,
                self.recovery,
                self.event_lag,
                self.unknown,
            ]
            .iter()
            .all(|n| *n == 0)
            && !self.mode_pending
    }
}
/// 同一SQLite读事务获取状态，避免跨表展示不同时间的事实。
pub async fn snapshot(db: &Database, binding: BindingId) -> Result<BindingStatus, PortError> {
    let mut tx = db.pool().begin().await.map_err(error)?;
    let b = binding.as_bytes().as_slice();
    let row = sqlx::query("SELECT received_seq,applied_seq FROM bindings WHERE id=?")
        .bind(b)
        .fetch_one(&mut *tx)
        .await
        .map_err(error)?;
    let received: i64 = row.try_get(0).map_err(error)?;
    let applied: i64 = row.try_get(1).map_err(error)?;
    if applied > received {
        return Err(PortError::protocol("应用游标超过接收游标"));
    }
    let mut s = BindingStatus {
        event_lag: (received - applied) as u64,
        ..Default::default()
    };
    for row in sqlx::query(
        "SELECT dirty,residency,fingerprint,file_identity FROM local_entries WHERE binding_id=?",
    )
    .bind(b)
    .fetch_all(&mut *tx)
    .await
    .map_err(error)?
    {
        if row.get::<String, _>("dirty") != "clean" {
            s.dirty += 1;
        }
        let index = match row.get::<&str, _>("residency") {
            "metadata_only" => 0,
            "partial" => 1,
            "materialized" => 2,
            _ => return Err(PortError::protocol("未知驻留")),
        };
        s.residency[index] += 1;
        if index == 2
            && (row
                .get::<Option<Vec<u8>>, _>("fingerprint")
                .is_none_or(|v| v.is_empty())
                || row
                    .get::<Option<Vec<u8>>, _>("file_identity")
                    .is_none_or(|v| v.is_empty()))
        {
            s.unknown += 1;
        }
    }
    for (table, filter, target) in [
        ("pending_ops", "1", &mut s.pending),
        ("transfers", "state<>'completed'", &mut s.transfers),
        ("local_intents", "1", &mut s.intents),
    ] {
        *target = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT count(*) FROM {table} WHERE binding_id=? AND {filter}"
        ))
        .bind(b)
        .fetch_one(&mut *tx)
        .await
        .map_err(error)? as u64;
    }
    for (table, filter) in [
        ("binding_imports", "complete=0"),
        ("binding_resyncs", "complete=0"),
        ("controlled_applies", "completed=0"),
        ("controlled_releases", "completed=0"),
    ] {
        s.recovery += sqlx::query_scalar::<_, i64>(&format!(
            "SELECT count(*) FROM {table} WHERE binding_id=? AND {filter}"
        ))
        .bind(b)
        .fetch_one(&mut *tx)
        .await
        .map_err(error)? as u64;
    }
    s.failed = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM pending_ops WHERE binding_id=? AND last_error_code IS NOT NULL",
    )
    .bind(b)
    .fetch_one(&mut *tx)
    .await
    .map_err(error)? as u64;
    s.offline=sqlx::query_scalar::<_,i64>("SELECT count(*) FROM operation_schedule s JOIN pending_ops p USING(op_id) WHERE p.binding_id=? AND s.offline=1").bind(b).fetch_one(&mut *tx).await.map_err(error)? as u64;
    if let Some(row) =
        sqlx::query("SELECT paused,detached,mode_pending FROM binding_controls WHERE binding_id=?")
            .bind(b)
            .fetch_optional(&mut *tx)
            .await
            .map_err(error)?
    {
        s.paused = row.get(0);
        s.detached = row.get(1);
        s.mode_pending = row.get(2);
    }
    s.unknown+=sqlx::query_scalar::<_,i64>("SELECT count(*) FROM binding_participation WHERE binding_id=? AND state='reconcile_required'").bind(b).fetch_one(&mut *tx).await.map_err(error)? as u64;
    let vault_count:i64=sqlx::query_scalar("SELECT count(*) FROM baseline_entries WHERE binding_id=? AND parent_entry_id IS NULL AND kind='directory' AND deleted=0").bind(b).fetch_one(&mut *tx).await.map_err(error)?;
    if vault_count != 1 {
        s.unknown += 1;
    }
    let mode: String = sqlx::query_scalar("SELECT mode FROM bindings WHERE id=?")
        .bind(b)
        .fetch_one(&mut *tx)
        .await
        .map_err(error)?;
    if mode == "full" {
        s.mode_pending |= !crate::bindings::control::mode_progress_in(&mut tx, binding)
            .await?
            .complete;
    }
    tx.commit().await.map_err(error)?;
    Ok(s)
}
pub(crate) fn error(e: impl std::fmt::Display) -> PortError {
    PortError::protocol(format!("状态/恢复：{e}"))
}
#[cfg(test)]
pub(crate) mod tests;
