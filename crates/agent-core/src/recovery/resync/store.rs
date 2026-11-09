//! 重建日志、隔离与原子游标发布。网络/扫描不在写事务中。
use super::*;
use sqlx::{Sqlite, Transaction};

pub(super) async fn start(
    database: &Database,
    binding: BindingId,
    reason: ResyncReason,
) -> Result<Journal, PortError> {
    let mut tx = database.begin_write().await.map_err(error)?;
    if let Some(journal) = load(&mut tx, binding).await? {
        if journal.reason != reason {
            return Err(error("已有不同原因的重建，必须先处理旧现场"));
        }
        tx.commit().await.map_err(error)?;
        return Ok(journal);
    }
    let bound = bindings::binding(&mut tx, binding)
        .await
        .map_err(error)?
        .ok_or_else(|| error("绑定不存在"))?;
    if let ResyncReason::ServerRollback { new_epoch } = reason {
        ServerEpoch::new(new_epoch).map_err(error)?;
        if new_epoch == bound.server_epoch.get() {
            return Err(error("服务器回退必须更换 epoch"));
        }
    }
    let importing: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM binding_imports WHERE binding_id = ? AND complete = 0",
    )
    .bind(binding.as_bytes().as_slice())
    .fetch_one(&mut *tx)
    .await
    .map_err(error)?;
    if importing != 0 {
        return Err(error("首次导入尚未完成，不能改用旧设备重建"));
    }
    let old_baseline = bases(&mut tx, binding, false).await?;
    if old_baseline.is_empty() {
        return Err(error("无共同基线，必须走首次导入而不是旧设备重建"));
    }
    let old_remote = bases(&mut tx, binding, true).await?;
    let (received, applied): (i64, i64) =
        sqlx::query_as("SELECT received_seq, applied_seq FROM bindings WHERE id = ?")
            .bind(binding.as_bytes().as_slice())
            .fetch_one(&mut *tx)
            .await
            .map_err(error)?;
    let events: Vec<SavedEvent> = sqlx::query_as("SELECT seq, tx_id AS tx, events_json AS events, state, last_error_code AS last_error FROM received_changes WHERE binding_id = ? ORDER BY seq")
        .bind(binding.as_bytes().as_slice()).fetch_all(&mut *tx).await.map_err(error)?;
    let mut journal = Journal {
        format: 1,
        resync_id: 0,
        revision: 0,
        binding: binding.to_string(),
        reason,
        old_epoch: bound.server_epoch.get(),
        old_received: received,
        old_applied: applied,
        old_baseline,
        old_remote,
        old_events: events,
        vault_identity: None,
        snapshot: None,
        entries: Vec::new(),
        cursor: None,
        snapshot_complete: false,
        items: None,
        complete: false,
    };
    journal.resync_id =
        sqlx::query("INSERT INTO binding_resyncs(binding_id, journal) VALUES (?, ?)")
            .bind(binding.as_bytes().as_slice())
            .bind(encode(&journal)?)
            .execute(&mut *tx)
            .await
            .map_err(error)?
            .last_insert_rowid();
    quarantine(&mut tx, &journal).await?;
    tx.commit().await.map_err(error)?;
    Ok(journal)
}

pub(super) async fn latest(
    database: &Database,
    binding: BindingId,
) -> Result<Option<Journal>, PortError> {
    let row: Option<(i64, i64, String, bool)> = sqlx::query_as("SELECT resync_id, revision, journal, complete FROM binding_resyncs WHERE binding_id = ? ORDER BY resync_id DESC LIMIT 1")
        .bind(binding.as_bytes().as_slice()).fetch_optional(database.pool()).await.map_err(error)?;
    row.map(|(resync_id, revision, text, complete)| {
        let mut journal: Journal = serde_json::from_str(&text).map_err(error)?;
        if journal.format != 1 || journal.binding()? != binding || journal.complete != complete {
            return Err(error("无效重建日志"));
        }
        journal.resync_id = resync_id;
        journal.revision = revision;
        Ok(journal)
    })
    .transpose()
}

pub(super) async fn pending(
    database: &Database,
    binding: BindingId,
) -> Result<Option<Journal>, PortError> {
    let mut tx = database.begin_write().await.map_err(error)?;
    let journal = load(&mut tx, binding).await?;
    tx.commit().await.map_err(error)?;
    Ok(journal)
}
async fn load(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
) -> Result<Option<Journal>, PortError> {
    let row: Option<(i64, i64, String)> = sqlx::query_as("SELECT resync_id, revision, journal FROM binding_resyncs WHERE binding_id = ? AND complete = 0")
        .bind(binding.as_bytes().as_slice()).fetch_optional(&mut **tx).await.map_err(error)?;
    row.map(|(resync_id, revision, text)| {
        let mut journal: Journal = serde_json::from_str(&text).map_err(error)?;
        if journal.format != 1 || journal.binding()? != binding || journal.complete {
            return Err(error("无效重建日志"));
        }
        journal.resync_id = resync_id;
        journal.revision = revision;
        Ok(journal)
    })
    .transpose()
}
pub(super) async fn save(database: &Database, journal: &mut Journal) -> Result<(), PortError> {
    let mut tx = database.begin_write().await.map_err(error)?;
    save_tx(&mut tx, journal).await?;
    quarantine(&mut tx, journal).await?;
    tx.commit().await.map_err(error)?;
    journal.revision += 1;
    Ok(())
}
async fn save_tx(tx: &mut Transaction<'_, Sqlite>, journal: &Journal) -> Result<(), PortError> {
    let result = sqlx::query("UPDATE binding_resyncs SET journal = ?, revision = revision + 1, complete = ? WHERE resync_id = ? AND revision = ? AND complete = 0")
        .bind(encode(journal)?).bind(journal.complete).bind(journal.resync_id).bind(journal.revision)
        .execute(&mut **tx).await.map_err(error)?;
    if result.rows_affected() != 1 {
        return Err(error("重建日志已被其它运行者修改"));
    }
    Ok(())
}
async fn quarantine(tx: &mut Transaction<'_, Sqlite>, journal: &Journal) -> Result<(), PortError> {
    sqlx::query("INSERT OR IGNORE INTO resync_quarantined_ops(op_id, resync_id) SELECT op_id, ? FROM pending_ops WHERE binding_id = ?")
        .bind(journal.resync_id).bind(journal.binding()?.as_bytes().as_slice()).execute(&mut **tx).await.map_err(error)?;
    Ok(())
}
pub(super) async fn has_intents(
    database: &Database,
    binding: BindingId,
) -> Result<bool, PortError> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM local_intents WHERE binding_id = ?")
        .bind(binding.as_bytes().as_slice())
        .fetch_one(database.pool())
        .await
        .map_err(error)?;
    Ok(count != 0)
}
pub(super) async fn finish(database: &Database, journal: &mut Journal) -> Result<(), PortError> {
    let binding = journal.binding()?;
    let snapshot = journal
        .snapshot
        .as_ref()
        .ok_or_else(|| error("缺固定快照"))?;
    let high_water = number(&snapshot.high_water_seq)? as i64;
    let remotes: Vec<_> = journal
        .entries
        .iter()
        .map(remote)
        .collect::<Result<_, _>>()?;
    paths(&journal.entries)?;
    let mut tx = database.begin_write().await.map_err(error)?;
    let cursors: (i64, i64, i64) =
        sqlx::query_as("SELECT server_epoch, received_seq, applied_seq FROM bindings WHERE id = ?")
            .bind(binding.as_bytes().as_slice())
            .fetch_one(&mut *tx)
            .await
            .map_err(error)?;
    if cursors
        != (
            journal.old_epoch as i64,
            journal.old_received,
            journal.old_applied,
        )
    {
        return Err(error("重建期间游标/epoch 改变"));
    }
    if serde_json::to_string(&bases(&mut tx, binding, false).await?).map_err(error)?
        != serde_json::to_string(&journal.old_baseline).map_err(error)?
    {
        return Err(error("重建期间旧基线改变"));
    }
    let intents: i64 =
        sqlx::query_scalar("SELECT count(*) FROM local_intents WHERE binding_id = ?")
            .bind(binding.as_bytes().as_slice())
            .fetch_one(&mut *tx)
            .await
            .map_err(error)?;
    if intents != 0 {
        return Err(error("旧本地意图尚未核对"));
    }
    let items = journal
        .items
        .as_ref()
        .ok_or_else(|| error("未核对本地内容"))?;
    for item in items {
        if item.disposition.needs_review() {
            return Err(error("仍有恢复候选/待核对条目"));
        }
        if let Some(entry) = &item.old_entry {
            let observed = entries::local_entry(&mut tx, binding, id(entry)?)
                .await
                .map_err(error)?
                .map(LocalStamp::from);
            if observed != item.local_stamp {
                return Err(error("核对之后出现本地新状态/代际，保留围栏"));
            }
        }
    }
    quarantine(&mut tx, journal).await?;
    // 只有此事务清掉旧事件的活动命名空间；完整旧事件仍在日志里，epoch 下相同 seq 不会去重旧事件。
    sqlx::query("DELETE FROM received_changes WHERE binding_id = ?")
        .bind(binding.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .map_err(error)?;
    sqlx::query("DELETE FROM remote_entries WHERE binding_id = ?")
        .bind(binding.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .map_err(error)?;
    for row in &remotes {
        entries::put_remote(&mut tx, binding, row)
            .await
            .map_err(error)?;
        entries::put_baseline(
            &mut tx,
            binding,
            &BaselineEntry {
                entry: row.entry,
                parent: row.parent,
                name: row.name.clone(),
                kind: row.kind,
                revision: row.revision,
                content_version: row.content_version,
                deleted: false,
            },
        )
        .await
        .map_err(error)?;
    }
    for item in items.iter().filter(|item| {
        matches!(
            item.disposition,
            Disposition::RetiredClean | Disposition::RetiredAbsent
        )
    }) {
        let old = journal
            .old_baseline
            .iter()
            .find(|old| Some(&old.entry) == item.old_entry.as_ref())
            .ok_or_else(|| error("缺历史基线"))?;
        let mut retired = old.baseline()?;
        retired.deleted = true;
        entries::put_baseline(&mut tx, binding, &retired)
            .await
            .map_err(error)?;
    }
    sqlx::query(
        "UPDATE bindings SET server_epoch = ?, received_seq = ?, applied_seq = ? WHERE id = ?",
    )
    .bind(journal.epoch() as i64)
    .bind(high_water)
    .bind(high_water)
    .bind(binding.as_bytes().as_slice())
    .execute(&mut *tx)
    .await
    .map_err(error)?;
    let mut completed = journal.clone();
    completed.complete = true;
    save_tx(&mut tx, &completed).await?;
    #[cfg(test)]
    super::tests::checkpoint("before-commit", 94);
    tx.commit().await.map_err(error)?;
    #[cfg(test)]
    super::tests::checkpoint("committed", 95);
    journal.complete = true;
    journal.revision += 1;
    Ok(())
}
async fn bases(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    is_remote: bool,
) -> Result<Vec<SavedBase>, PortError> {
    let table = if is_remote {
        "remote_entries"
    } else {
        "baseline_entries"
    };
    let ids: Vec<Vec<u8>> = sqlx::query_scalar(&format!(
        "SELECT entry_id FROM {table} WHERE binding_id = ? ORDER BY entry_id"
    ))
    .bind(binding.as_bytes().as_slice())
    .fetch_all(&mut **tx)
    .await
    .map_err(error)?;
    let mut result = Vec::new();
    for bytes in ids {
        let entry = EntryId::from_bytes(bytes.try_into().map_err(|_| error("无效条目身份长度"))?);
        let base = if is_remote {
            let r = entries::remote_entry(tx, binding, entry)
                .await
                .map_err(error)?
                .ok_or_else(|| error("远端行消失"))?;
            BaselineEntry {
                entry: r.entry,
                parent: r.parent,
                name: r.name,
                kind: r.kind,
                revision: r.revision,
                content_version: r.content_version,
                deleted: r.deleted,
            }
        } else {
            entries::baseline_entry(tx, binding, entry)
                .await
                .map_err(error)?
                .ok_or_else(|| error("基线消失"))?
        };
        result.push(SavedBase::new(base));
    }
    Ok(result)
}
fn encode(journal: &Journal) -> Result<String, PortError> {
    serde_json::to_string(journal).map_err(error)
}
