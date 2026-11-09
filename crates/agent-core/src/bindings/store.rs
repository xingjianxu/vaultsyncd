//! 首次绑定的 SQLite 适配。日志、临时身份映射、ACK、基线与 dirty 更新均为短事务。
use std::sync::Arc;

use sqlx::{Sqlite, Transaction};
use vaultsyncd_client_store::bindings::{self, BindingRecord};
use vaultsyncd_client_store::db::Database;
use vaultsyncd_client_store::entries::{self, BaselineEntry, EntryKind, LocalEntry, RemoteEntry};
use vaultsyncd_client_store::queue::{self, NewPendingOp, PendingOp};
use vaultsyncd_domain::ids::{BindingId, EntryId, OpId, VersionId};
use vaultsyncd_domain::state::{Dirty, Residency};
use vaultsyncd_domain::version::{Generation, Revision};
use vaultsyncd_protocol::generated::{EntryKind as WireKind, OperationResult, OperationState};
use vaultsyncd_transfer::ports::{LocalErrorKind, PortError, PortFuture};

use super::import::{
    Journal, baseline, entry_id, generation, invalid, name, number, op_id, remote_paths,
};
use crate::executor::upload::{UploadCommit, UploadCommitDisposition, UploadCommitStore};

pub(super) struct ImportStore {
    database: Arc<Database>,
}
impl ImportStore {
    pub fn new(database: Arc<Database>) -> Self {
        Self { database }
    }
    pub async fn binding(&self, id: BindingId) -> Result<BindingRecord, PortError> {
        let mut conn = self.database.pool().acquire().await.map_err(db)?;
        bindings::binding(&mut conn, id)
            .await
            .map_err(db)?
            .ok_or_else(|| invalid("绑定不存在"))
    }
    pub async fn other_paths(&self, id: BindingId) -> Result<Vec<String>, PortError> {
        sqlx::query_scalar("SELECT local_path FROM bindings WHERE id <> ?1 UNION ALL SELECT cache_path FROM bindings WHERE id <> ?1")
            .bind(id.as_bytes().as_slice())
            .fetch_all(self.database.pool())
            .await
            .map_err(db)
    }
    pub async fn ensure_not_resyncing(&self, binding: BindingId) -> Result<(), PortError> {
        crate::recovery::resync::ensure_unblocked(&self.database, binding).await
    }
    pub async fn ensure_fresh(&self, binding: BindingId) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        ensure_fresh(&mut tx, binding).await?;
        tx.commit().await.map_err(db)
    }
    pub async fn load(&self, binding: BindingId) -> Result<Option<Journal>, PortError> {
        let row: Option<(String, i64)> =
            sqlx::query_as("SELECT journal, revision FROM binding_imports WHERE binding_id = ?")
                .bind(binding.as_bytes().as_slice())
                .fetch_optional(self.database.pool())
                .await
                .map_err(db)?;
        row.map(|(text, revision)| decode(&text, revision))
            .transpose()
    }
    pub async fn create(&self, binding: BindingId, journal: &Journal) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        ensure_fresh(&mut tx, binding).await?;
        sqlx::query("INSERT INTO binding_imports(binding_id, journal) VALUES (?, ?)")
            .bind(binding.as_bytes().as_slice())
            .bind(encode(journal)?)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)
    }
    pub async fn save(&self, binding: BindingId, journal: &mut Journal) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        save(&mut tx, binding, journal).await?;
        tx.commit().await.map_err(db)
    }
    pub async fn prepare(
        &self,
        binding: &BindingRecord,
        journal: &mut Journal,
    ) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        for item in journal.items.as_ref().ok_or_else(|| invalid("缺少清单"))? {
            if let (Some(local), Some(temporary)) = (&item.local, &item.temporary) {
                entries::put_local(
                    &mut tx,
                    binding.id,
                    &LocalEntry {
                        entry: entry_id(temporary)?,
                        generation: generation(local.generation)?,
                        dirty: Dirty::Dirty,
                        residency: Residency::Materialized,
                        fingerprint: local.hash.map(|hash| hash.to_vec()),
                        file_identity: None,
                    },
                )
                .await
                .map_err(db)?;
            }
            if let Some(remote) = &item.remote {
                put_remote(&mut tx, binding.id, &baseline(remote)?).await?;
            }
            if item.upload {
                queue::enqueue(
                    &mut tx,
                    &NewPendingOp {
                        op: op_id(&item.op)?,
                        binding: binding.id,
                        kind: "upload_new_entry".to_owned(),
                        entry: None,
                        baseline_revision: None,
                        baseline_version: None,
                        depends_on: None,
                        pinned_version: None,
                    },
                )
                .await
                .map_err(db)?;
            }
        }
        save(&mut tx, binding.id, journal).await?;
        tx.commit().await.map_err(db)
    }
    pub async fn queued(&self, op: OpId) -> Result<PendingOp, PortError> {
        let mut conn = self.database.pool().acquire().await.map_err(db)?;
        queue::pending_op(&mut conn, op)
            .await
            .map_err(db)?
            .ok_or_else(|| invalid("上传待执行操作消失"))
    }
    pub async fn resolve_parent(
        &self,
        binding: BindingId,
        target: &str,
    ) -> Result<EntryId, PortError> {
        let journal = self
            .load(binding)
            .await?
            .ok_or_else(|| invalid("导入日志消失"))?;
        resolve_parent(&journal, target)
    }
    pub async fn ack(
        &self,
        binding: BindingId,
        op: OpId,
        result: &OperationResult,
    ) -> Result<UploadCommitDisposition, PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        let mut journal = load(&mut tx, binding).await?;
        let index = journal
            .items
            .as_ref()
            .ok_or_else(|| invalid("ACK 前未保存导入清单"))?
            .iter()
            .position(|item| item.op == op.to_string())
            .ok_or_else(|| invalid("ACK 不属于导入清单"))?;
        let item = journal.items.as_ref().expect("清单")[index].clone();
        let local = item
            .local
            .as_ref()
            .ok_or_else(|| invalid("ACK 缺本地事实"))?;
        if result.op_id != item.op
            || result.echo_local_generation.as_deref()
                != Some(local.generation.to_string().as_str())
        {
            return Err(invalid("ACK 操作或 generation 与导入清单不符"));
        }
        match result.state {
            OperationState::Pending => {
                return Err(PortError::unavailable("首次导入提交仍 pending", None));
            }
            OperationState::Rejected => {
                return Err(PortError::rejected(
                    result
                        .error_code
                        .as_ref()
                        .and_then(|code| {
                            serde_json::to_value(code).ok()?.as_str().map(str::to_owned)
                        })
                        .unwrap_or_else(|| "NEEDS_RECOVERY".to_owned()),
                    result
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "服务器拒绝首次导入".to_owned()),
                ));
            }
            OperationState::Committed => {}
        }
        if let Some(previous) = &item.result {
            let mut previous = previous.clone();
            previous.replayed = result.replayed;
            if &previous != result {
                return Err(invalid("同一 OpId 收到不同 committed 结果"));
            }
            if result.conflict.is_some() {
                return Err(invalid("冲突 ACK 已保留，需取得最终路径后恢复"));
            }
            let stable = entry_id(
                result
                    .entry_id
                    .as_deref()
                    .ok_or_else(|| invalid("已保存 ACK 缺 EntryId"))?,
            )?;
            let current = entries::local_entry(&mut tx, binding, stable)
                .await
                .map_err(db)?;
            let expected = generation(local.generation)?;
            return Ok(
                if current.is_some_and(|entry| entry.generation == expected) {
                    UploadCommitDisposition::Applied
                } else {
                    UploadCommitDisposition::StaleGeneration
                },
            );
        }
        let committed_entry = result
            .conflict
            .as_ref()
            .map(|c| c.conflict_entry_id.as_str())
            .or(result.entry_id.as_deref())
            .ok_or_else(|| invalid("committed 缺 EntryId"))?;
        let stable = entry_id(committed_entry)?;
        let revision = Revision::new(number(
            result
                .revision
                .as_deref()
                .ok_or_else(|| invalid("committed 缺 revision"))?,
        )?)
        .map_err(db)?;
        number(
            result
                .vault_seq
                .as_deref()
                .ok_or_else(|| invalid("committed 缺 vault_seq"))?,
        )?;
        let version = result
            .version_id
            .as_deref()
            .map(VersionId::from_hex)
            .transpose()
            .map_err(db)?;
        if (local.kind == WireKind::File) != version.is_some() {
            return Err(invalid("committed 文件版本形状错误"));
        }
        if let Some(conflict) = &result.conflict {
            entry_id(&conflict.entry_id)?;
        }
        let temporary = entry_id(
            item.temporary
                .as_deref()
                .ok_or_else(|| invalid("缺临时身份"))?,
        )?;
        map_id(
            &mut tx,
            binding,
            temporary,
            stable,
            op,
            &serde_json::to_string(result).map_err(db)?,
        )
        .await?;
        let mut disposition = UploadCommitDisposition::StaleGeneration;
        // 冲突 ACK 保留所有身份与原结果，但没有最终路径字段，不能猜路径并清 dirty。
        if result.conflict.is_none() {
            let base = BaselineEntry {
                entry: stable,
                parent: Some(resolve_parent(&journal, &item.target)?),
                name: name(&item.target)?.to_owned(),
                kind: if local.kind == WireKind::File {
                    EntryKind::File
                } else {
                    EntryKind::Directory
                },
                revision,
                content_version: version,
                deleted: false,
            };
            entries::put_baseline(&mut tx, binding, &base)
                .await
                .map_err(db)?;
            put_remote(&mut tx, binding, &base).await?;
            disposition = remap_local(
                &mut tx,
                binding,
                temporary,
                stable,
                generation(local.generation)?,
            )
            .await?;
        }
        journal.items.as_mut().expect("清单")[index].result = Some(result.clone());
        save(&mut tx, binding, &mut journal).await?;
        tx.commit().await.map_err(db)?;
        if result.conflict.is_some() {
            return Err(invalid(
                "并发提交产生冲突，ACK 已保留，需取得最终路径后恢复",
            ));
        }
        Ok(disposition)
    }
    pub async fn finish_item(
        &self,
        binding: &BindingRecord,
        journal: &mut Journal,
        index: usize,
        installed: Option<Generation>,
    ) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        let item = journal.items.as_ref().ok_or_else(|| invalid("缺清单"))?[index].clone();
        if let Some(remote) = &item.remote {
            let base = baseline(remote)?;
            if let Some(current) = entries::baseline_entry(&mut tx, binding.id, base.entry)
                .await
                .map_err(db)?
            {
                if current.revision > base.revision
                    || (current.revision == base.revision && current != base)
                {
                    return Err(invalid("首次安装收尾不能倒退或改写已确认基线"));
                }
            }
            entries::put_baseline(&mut tx, binding.id, &base)
                .await
                .map_err(db)?;
            if let Some(generation) = installed {
                // S059 已在发布事务保存摘要/身份；旧导入收尾不能清除发布后出现的新 dirty。
                // 无已有行时保留 S058 其它平台安装端口的兼容路径（未知指纹仍不授权覆盖）。
                let existing = entries::local_entry(&mut tx, binding.id, base.entry)
                    .await
                    .map_err(db)?;
                if existing.is_none() {
                    entries::put_local(
                        &mut tx,
                        binding.id,
                        &LocalEntry {
                            entry: base.entry,
                            generation,
                            dirty: Dirty::Clean,
                            residency: if binding.mode == bindings::SyncMode::OnDemand
                                && remote.kind == WireKind::File
                            {
                                Residency::MetadataOnly
                            } else {
                                Residency::Materialized
                            },
                            fingerprint: None,
                            file_identity: None,
                        },
                    )
                    .await
                    .map_err(db)?;
                }
            } else if !item.upload {
                let temporary = entry_id(
                    item.temporary
                        .as_deref()
                        .ok_or_else(|| invalid("同内容绑定缺临时身份"))?,
                )?;
                let generation = generation(
                    item.local
                        .as_ref()
                        .ok_or_else(|| invalid("同内容绑定缺本地事实"))?
                        .generation,
                )?;
                map_id(
                    &mut tx,
                    binding.id,
                    temporary,
                    base.entry,
                    op_id(&item.op)?,
                    &serde_json::to_string(remote).map_err(db)?,
                )
                .await?;
                remap_local(&mut tx, binding.id, temporary, base.entry, generation).await?;
            }
        }
        if item.upload {
            if item.result.is_none() {
                return Err(invalid("上传未 committed，不可完成路径"));
            }
            queue::complete(&mut tx, op_id(&item.op)?)
                .await
                .map_err(db)?;
        }
        journal.items.as_mut().expect("清单")[index].complete = true;
        save(&mut tx, binding.id, journal).await?;
        tx.commit().await.map_err(db)
    }
    pub async fn finish(
        &self,
        binding: &BindingRecord,
        journal: &mut Journal,
    ) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        let high_water = number(&journal.snapshot.high_water_seq)? as i64;
        let vault = journal
            .entries
            .iter()
            .find(|entry| entry.parent_id.is_none())
            .ok_or_else(|| invalid("缺合成根"))?;
        let base = baseline(vault)?;
        entries::put_baseline(&mut tx, binding.id, &base)
            .await
            .map_err(db)?;
        put_remote(&mut tx, binding.id, &base).await?;
        // 快照应用的专用初始化围栏；后续游标只能由 S057 事件事务推进。
        let hit = sqlx::query("UPDATE bindings SET received_seq = ?, applied_seq = ? WHERE id = ? AND received_seq = 0 AND applied_seq = 0")
            .bind(high_water).bind(high_water).bind(binding.id.as_bytes().as_slice()).execute(&mut *tx).await.map_err(db)?.rows_affected();
        if hit != 1 {
            return Err(invalid("导入期间事件游标被其它执行器推进"));
        }
        journal.complete = true;
        save(&mut tx, binding.id, journal).await?;
        tx.commit().await.map_err(db)
    }
}

impl UploadCommitStore for ImportStore {
    fn commit_upload<'a>(
        &'a self,
        commit: &'a UploadCommit,
    ) -> PortFuture<'a, Result<UploadCommitDisposition, PortError>> {
        Box::pin(async move {
            let journal = self
                .load(commit.binding)
                .await?
                .ok_or_else(|| invalid("上传缺持久化导入清单"))?;
            let local = journal
                .items
                .as_ref()
                .and_then(|items| items.iter().find(|item| item.op == commit.op.to_string()))
                .and_then(|item| item.local.as_ref())
                .ok_or_else(|| invalid("上传 ACK 不属于清单"))?;
            if commit.generation != generation(local.generation)?
                || local.hash != Some(commit.upload.content.digest)
                || local.size != Some(commit.upload.content.size)
            {
                return Err(invalid("已提交上传内容与原导入清单不符，保留现场"));
            }
            self.ack(commit.binding, commit.op, &commit.result).await
        })
    }
}
async fn ensure_fresh(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
) -> Result<(), PortError> {
    for table in [
        "baseline_entries",
        "remote_entries",
        "local_entries",
        "received_changes",
        "pending_ops",
        "local_intents",
    ] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {table} WHERE binding_id = ?"
        ))
        .bind(binding.as_bytes().as_slice())
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
        if count != 0 {
            return Err(invalid(
                "绑定已有基线/事件/本地状态，必须走旧基线重建，不能首次合并",
            ));
        }
    }
    let cursors: (i64, i64) =
        sqlx::query_as("SELECT received_seq, applied_seq FROM bindings WHERE id = ?")
            .bind(binding.as_bytes().as_slice())
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    if cursors != (0, 0) {
        return Err(invalid("旧游标绑定不能走首次合并"));
    }
    Ok(())
}
async fn load(tx: &mut Transaction<'_, Sqlite>, binding: BindingId) -> Result<Journal, PortError> {
    let (text, revision): (String, i64) =
        sqlx::query_as("SELECT journal, revision FROM binding_imports WHERE binding_id = ?")
            .bind(binding.as_bytes().as_slice())
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    decode(&text, revision)
}
async fn save(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    journal: &mut Journal,
) -> Result<(), PortError> {
    let hit = sqlx::query("UPDATE binding_imports SET journal = ?, revision = revision + 1, complete = ? WHERE binding_id = ? AND revision = ?")
        .bind(encode(journal)?).bind(journal.complete).bind(binding.as_bytes().as_slice()).bind(journal.revision)
        .execute(&mut **tx).await.map_err(db)?.rows_affected();
    if hit != 1 {
        return Err(invalid("导入日志被其它运行者改变，请重新加载后恢复"));
    }
    journal.revision += 1;
    Ok(())
}
fn decode(text: &str, revision: i64) -> Result<Journal, PortError> {
    let mut journal: Journal = serde_json::from_str(text).map_err(db)?;
    journal.revision = revision;
    Ok(journal)
}
fn encode(journal: &Journal) -> Result<String, PortError> {
    serde_json::to_string(journal).map_err(db)
}
fn resolve_parent(journal: &Journal, target: &str) -> Result<EntryId, PortError> {
    if let Some((parent, _)) = target.rsplit_once('/') {
        if let Some(item) = journal.items.as_ref().and_then(|items| {
            items
                .iter()
                .find(|item| item.target == parent && item.local.is_some())
        }) {
            if item.upload {
                return item
                    .result
                    .as_ref()
                    .filter(|result| result.conflict.is_none())
                    .and_then(|result| result.entry_id.as_deref())
                    .ok_or_else(|| invalid("父目录尚未 committed"))
                    .and_then(entry_id);
            }
        }
        let remote = remote_paths(&journal.entries)?;
        return remote
            .get(parent)
            .ok_or_else(|| invalid("目标缺已映射父目录"))
            .and_then(|entry| entry_id(&entry.entry_id));
    }
    journal
        .entries
        .iter()
        .find(|entry| entry.parent_id.is_none())
        .ok_or_else(|| invalid("缺合成根"))
        .and_then(|entry| entry_id(&entry.entry_id))
}
async fn map_id(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    temporary: EntryId,
    stable: EntryId,
    op: OpId,
    result: &str,
) -> Result<(), PortError> {
    sqlx::query("INSERT INTO import_id_map(binding_id, temporary_id, entry_id, op_id, result_json) VALUES (?, ?, ?, ?, ?)")
        .bind(binding.as_bytes().as_slice()).bind(temporary.as_bytes().as_slice()).bind(stable.as_bytes().as_slice())
        .bind(op.as_bytes().as_slice()).bind(result).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}
async fn remap_local(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    temporary: EntryId,
    stable: EntryId,
    expected: Generation,
) -> Result<UploadCommitDisposition, PortError> {
    let mut local = entries::local_entry(tx, binding, temporary)
        .await
        .map_err(db)?
        .ok_or_else(|| invalid("临时本地条目消失"))?;
    let disposition = if local.generation == expected {
        local.dirty = Dirty::Clean;
        UploadCommitDisposition::Applied
    } else {
        UploadCommitDisposition::StaleGeneration
    };
    local.entry = stable;
    entries::put_local(tx, binding, &local).await.map_err(db)?;
    if temporary != stable {
        entries::remove_local(tx, binding, temporary)
            .await
            .map_err(db)?;
    }
    Ok(disposition)
}
async fn put_remote(
    tx: &mut Transaction<'_, Sqlite>,
    binding: BindingId,
    base: &BaselineEntry,
) -> Result<(), PortError> {
    entries::put_remote(
        tx,
        binding,
        &RemoteEntry {
            entry: base.entry,
            parent: base.parent,
            name: base.name.clone(),
            kind: base.kind,
            revision: base.revision,
            content_version: base.content_version,
            deleted: false,
        },
    )
    .await
    .map_err(db)
}
fn db(error: impl std::fmt::Display) -> PortError {
    PortError::local(LocalErrorKind::Write, error.to_string())
}
