//! 恢复命令的适配器（S052 续接）：历史版本恢复与回收站恢复的 SQLx 实现。
//!
//! 规则在 `server-core` 的 `history::{version,trash}`；本模块只实现端口：
//!
//! - [`SqlRestoreStore`]（`RestoreStore`）：读条目基线/版本内容/保留期，比较并写地追加一个新版本
//!   （引用旧版本同一对象，旧版本行一字不改），并把事件批次、操作结果与意图收尾放进同一个事务。
//! - [`SqlTrashRestoreStore`]（`TrashRestoreStore`）：读回收站子树与名称占用、把清单写回命名空间
//!   （清软删除、改回父目录/名称、推进 revision、删清单行与墓碑），同一事务提交。
//!
//! 两者都把 `operations` 的 `pending → committed` 与业务事实放在同一事务里：重试拿到同一份结果，
//! 失败不会留下“已恢复”的假象。

use vaultsyncd_domain::ids::{EntryId, RootId, TxId, UserId, VersionId};
use vaultsyncd_domain::version::{Revision, UtcTimestamp};
use vaultsyncd_server_core::api::operations::OperationFailure;
use vaultsyncd_server_core::history::trash::{
    CommittedTrashRestore, RestoredTrash, TrashRestorePorts, TrashRestoreRequest,
};
use vaultsyncd_server_core::history::version::{
    CommittedVersionRestore, RestoredVersion, VersionRestoreRequest,
};
use vaultsyncd_server_core::objects::ExpectedContent;
use vaultsyncd_server_core::ports::conflict_store::{OperationKey, RequestDigest};
use vaultsyncd_server_core::ports::put_store::{EntryKind, RootFacts};
use vaultsyncd_server_core::ports::restore_store::{
    RecordedTrashRestore, RecordedVersionRestore, RestoreBegin, RestoreStore, RestoreStoreError,
    TrashEntryFacts, TrashRestoreEntry, TrashRestoreRecord, TrashRestoreStore, TrashRestoreWrite,
    TrashSubtree, VersionFacts as Facts, VersionRestoreRecord, VersionRestoreWrite,
};
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::entries as store_entries;
use vaultsyncd_server_store::intents as store_intents;
use vaultsyncd_server_store::intents::FsIntentPhase;
use vaultsyncd_server_store::operations as store_operations;
use vaultsyncd_server_store::roots as store_roots;
use vaultsyncd_server_store::trash as store_trash;
use vaultsyncd_server_store::versions as store_versions;
use vaultsyncd_server_store::versions::{NewVersion, VersionOrigin};

use crate::uploads::random_bytes;

/// 内容历史保留期（design §10.3：删除/历史内容至少保留 30 天）。
const HISTORY_RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;

fn restore_error(detail: impl std::fmt::Display) -> RestoreStoreError {
    RestoreStoreError::Storage {
        detail: detail.to_string(),
    }
}

async fn connect(
    database: &Database,
) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, RestoreStoreError> {
    database.pool().acquire().await.map_err(restore_error)
}

fn operation_failure(error: store_operations::OperationError) -> RestoreStoreError {
    match error {
        store_operations::OperationError::DigestMismatch { .. } => {
            RestoreStoreError::DuplicateOperation
        }
        store_operations::OperationError::OutOfScope
        | store_operations::OperationError::UnknownOperation { .. } => {
            RestoreStoreError::OutOfScope
        }
        other => restore_error(other),
    }
}

fn root_facts_of(
    record: Option<store_roots::RootRecord>,
    root: RootId,
) -> Result<RootFacts, RestoreStoreError> {
    record
        .map(|record| RootFacts {
            epoch: record.epoch,
        })
        .ok_or(RestoreStoreError::UnknownRoot { root })
}

fn content_from_version(version: &store_versions::VersionFacts) -> ExpectedContent {
    ExpectedContent::new(
        vaultsyncd_server_core::objects::ContentDigest::from_bytes(version.object_hash),
        version.size,
    )
}

/// 操作记录里的结果 JSON → 历史版本恢复结果（首次提交与重放共用同一份形状）。
fn version_result_from_json(text: &str) -> Option<CommittedVersionRestore> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let hash = value.get("hash")?.as_str()?;
    let size: u64 = value.get("size")?.as_str()?.parse().ok()?;
    Some(CommittedVersionRestore {
        op: parse_id(value.get("op_id")?.as_str()?)?,
        root: parse_id(value.get("root_id")?.as_str()?)?,
        entry: parse_id(value.get("entry_id")?.as_str()?)?,
        version: parse_id(value.get("version_id")?.as_str()?)?,
        restored_from: parse_id(value.get("restored_from")?.as_str()?)?,
        content: ExpectedContent::new(
            vaultsyncd_server_core::objects::ContentDigest::from_hex(hash).ok()?,
            size,
        ),
        revision: Revision::from_decimal(value.get("revision")?.as_str()?).ok()?,
        seq: vaultsyncd_domain::version::RootSeq::from_decimal(value.get("root_seq")?.as_str()?)
            .ok()?,
        path: value.get("path")?.as_str()?.to_owned(),
    })
}

/// 操作记录里的结果 JSON → 回收站恢复结果。
fn trash_result_from_json(text: &str) -> Option<CommittedTrashRestore> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(CommittedTrashRestore {
        op: parse_id(value.get("op_id")?.as_str()?)?,
        root: parse_id(value.get("root_id")?.as_str()?)?,
        entry: parse_id(value.get("entry_id")?.as_str()?)?,
        path: value.get("path")?.as_str()?.to_owned(),
        name: value.get("name")?.as_str()?.to_owned(),
        renamed: value.get("renamed")?.as_bool()?,
        revision: Revision::from_decimal(value.get("revision")?.as_str()?).ok()?,
        seq: vaultsyncd_domain::version::RootSeq::from_decimal(value.get("root_seq")?.as_str()?)
            .ok()?,
        entries: value.get("entries")?.as_str()?.parse().ok()?,
    })
}

fn parse_id<T: std::str::FromStr>(text: &str) -> Option<T> {
    text.parse().ok()
}

/// 核心幂等键 → 存储层幂等键（字段一一对应，类型分属两个 crate）。
fn store_key(key: OperationKey) -> store_operations::OperationKey {
    store_operations::OperationKey {
        root: key.root,
        device: key.device,
        op: key.op,
    }
}

/// 核心请求摘要 → 存储层摘要（同一份字节）。
fn store_digest(digest: RequestDigest) -> store_operations::RequestDigest {
    store_operations::RequestDigest::from_bytes(*digest.as_bytes())
}

/// 在同一业务事务中把已经分配的序号写入结果，避免重放得到占位的零序号。
pub(crate) async fn persist_result_seq(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    key: OperationKey,
    result: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE operations SET result_json = ?1 WHERE root_id = ?2 AND device_id = ?3 AND op_id = ?4 AND state = 'committed'")
        .bind(result)
        .bind(key.root.as_bytes().as_slice())
        .bind(key.device.as_bytes().as_slice())
        .bind(key.op.as_bytes().as_slice())
        .execute(&mut **tx).await?;
    Ok(())
}

fn version_result_json(result: &CommittedVersionRestore) -> String {
    serde_json::json!({
        "op_id": result.op.to_hex(),
        "root_id": result.root.to_hex(),
        "entry_id": result.entry.to_hex(),
        "version_id": result.version.to_hex(),
        "restored_from": result.restored_from.to_hex(),
        "hash": result.content.digest.to_hex(),
        "size": result.content.bytes.to_string(),
        "revision": result.revision.to_string(),
        "root_seq": result.seq.to_string(),
        "path": result.path,
        "state": "committed",
    })
    .to_string()
}

fn trash_result_json(result: &CommittedTrashRestore) -> String {
    serde_json::json!({
        "op_id": result.op.to_hex(),
        "root_id": result.root.to_hex(),
        "entry_id": result.entry.to_hex(),
        "path": result.path,
        "name": result.name,
        "renamed": result.renamed,
        "revision": result.revision.to_string(),
        "root_seq": result.seq.to_string(),
        "entries": result.entries.to_string(),
        "state": "committed",
    })
    .to_string()
}

/// 历史版本恢复的存储实现。
pub struct SqlRestoreStore {
    database: Database,
    owner: UserId,
    key: OperationKey,
    digest: RequestDigest,
}

impl SqlRestoreStore {
    /// 以数据库、账号与本次操作的幂等键装配。
    #[must_use]
    pub fn new(
        database: Database,
        owner: UserId,
        key: OperationKey,
        digest: RequestDigest,
    ) -> Self {
        Self {
            database,
            owner,
            key,
            digest,
        }
    }
}

impl RestoreStore for SqlRestoreStore {
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_roots::root_in_scope(&mut conn, self.owner, root)
            .await
            .map_err(restore_error)?;
        root_facts_of(record, root)
    }

    async fn operation(
        &mut self,
        key: OperationKey,
    ) -> Result<Option<RecordedVersionRestore>, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_operations::operation_in_scope(&mut conn, self.owner, &store_key(key))
            .await
            .map_err(operation_failure)?;
        Ok(record.map(|record| RecordedVersionRestore {
            digest: RequestDigest::from_bytes(*record.digest.as_bytes()),
            result: match record.state {
                store_operations::OperationState::Committed => record
                    .result_json
                    .as_deref()
                    .and_then(version_result_from_json),
                _ => None,
            },
        }))
    }

    async fn begin_restore(
        &mut self,
        key: OperationKey,
        digest: RequestDigest,
    ) -> Result<RestoreBegin, RestoreStoreError> {
        let mut tx = self.database.begin_write().await.map_err(restore_error)?;
        let began = store_operations::begin_operation(
            &mut tx,
            self.owner,
            &store_operations::NewOperation {
                key: store_key(key),
                digest: store_digest(digest),
            },
        )
        .await
        .map_err(operation_failure)?;
        tx.commit().await.map_err(restore_error)?;
        Ok(match began {
            store_operations::BeginOperation::Started => RestoreBegin::Started,
            store_operations::BeginOperation::Replayed(_) => RestoreBegin::Recorded,
        })
    }

    async fn version_facts(
        &mut self,
        root: RootId,
        entry: EntryId,
        version: VersionId,
    ) -> Result<Facts, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_entries::entry_in_scope(&mut conn, self.owner, entry)
            .await
            .map_err(restore_error)?
            .ok_or(RestoreStoreError::EntryMissing { entry })?;
        if record.root != root || record.deleted_at.is_some() {
            return Err(RestoreStoreError::EntryMissing { entry });
        }
        let parent = record.parent.unwrap_or(entry);
        let facts = store_versions::version_facts(&mut conn, self.owner, root, version)
            .await
            .map_err(restore_error)?
            .ok_or(RestoreStoreError::VersionMissing { version })?;
        if facts.entry != entry {
            return Err(RestoreStoreError::VersionMissing { version });
        }
        let retained_until = UtcTimestamp::from_unix_seconds(
            facts
                .created_at
                .as_unix_seconds()
                .saturating_add(HISTORY_RETENTION_SECONDS),
        )
        .map_err(restore_error)?;
        Ok(Facts {
            entry: vaultsyncd_server_core::ports::put_store::EntryBaseline {
                entry,
                root,
                parent,
                name: record.name,
                revision: record.revision,
                kind: match record.kind {
                    store_entries::EntryKind::File => EntryKind::File,
                    store_entries::EntryKind::Directory => EntryKind::Directory,
                },
            },
            content: content_from_version(&facts),
            retained_until,
        })
    }

    async fn apply_version_restore(
        &mut self,
        write: &VersionRestoreWrite,
    ) -> Result<VersionRestoreRecord, RestoreStoreError> {
        let mut tx = self.database.begin_write().await.map_err(restore_error)?;
        let intent = store_intents::intent_in_scope(&mut tx, self.owner, write.intent_id)
            .await
            .map_err(restore_error)?
            .ok_or(RestoreStoreError::IllegalIntentTransition {
                from: vaultsyncd_server_core::commit::intent::PutPhase::Prepared,
                to: vaultsyncd_server_core::commit::intent::PutPhase::Finalized,
            })?;
        if intent.phase != FsIntentPhase::Published {
            return Err(RestoreStoreError::IllegalIntentTransition {
                from: crate::commands::phase_text(intent.phase),
                to: vaultsyncd_server_core::commit::intent::PutPhase::Finalized,
            });
        }
        let current = store_entries::entry_in_scope(&mut tx, self.owner, write.entry)
            .await
            .map_err(restore_error)?
            .ok_or(RestoreStoreError::EntryMissing { entry: write.entry })?;
        if current.revision != write.expected_revision {
            return Err(RestoreStoreError::RevisionChanged {
                expected: write.expected_revision,
                actual: current.revision,
            });
        }
        let next = write.expected_revision.next().map_err(restore_error)?;
        let version = VersionId::from_bytes(random_bytes());
        let tx_id = TxId::from_bytes(random_bytes());
        let events = serde_json::json!([{
            "kind": "restore",
            "entry_id": write.entry.to_hex(),
            "path": write.path,
            "revision": next.to_string(),
            "version_id": version.to_hex(),
            "restored_from": write.restored_from.to_hex(),
            "hash": write.content.digest.to_hex(),
            "size": write.content.bytes.to_string(),
            "origin": "client",
        }])
        .to_string();
        let result = CommittedVersionRestore {
            op: self.key.op,
            root: self.key.root,
            entry: write.entry,
            version,
            restored_from: write.restored_from,
            content: write.content,
            revision: next,
            seq: vaultsyncd_domain::version::RootSeq::ZERO,
            path: write.path.clone(),
        };
        let committed = store_operations::commit_operation(
            &mut tx,
            self.owner,
            &store_key(self.key),
            tx_id,
            &events,
            &version_result_json(&result),
            write.at,
        )
        .await
        .map_err(operation_failure)?;
        let mut result = result;
        result.seq = committed.batch.seq;
        persist_result_seq(&mut tx, self.key, &version_result_json(&result))
            .await
            .map_err(restore_error)?;
        store_versions::record_version(
            &mut tx,
            self.owner,
            &NewVersion {
                id: version,
                root: write.key.root,
                entry: write.entry,
                object_hash: *write.content.digest.as_bytes(),
                size: write.content.bytes,
                created_seq: committed.batch.seq,
                origin: VersionOrigin::Client,
                at: write.at,
            },
        )
        .await
        .map_err(restore_error)?;
        store_entries::advance_entry_revision(
            &mut tx,
            self.owner,
            write.entry,
            write.expected_revision,
            next,
        )
        .await
        .map_err(restore_error)?;
        store_intents::advance_intent_phase(
            &mut tx,
            self.owner,
            write.intent_id,
            FsIntentPhase::Finalized,
            write.at,
        )
        .await
        .map_err(restore_error)?;
        tx.commit().await.map_err(restore_error)?;
        let _ = &self.digest;
        Ok(VersionRestoreRecord {
            intent_id: write.intent_id,
            phase: vaultsyncd_server_core::commit::intent::PutPhase::Finalized,
            tx: committed.batch.tx,
            result,
        })
    }
}

/// 回收站恢复的存储实现。
pub struct SqlTrashRestoreStore {
    database: Database,
    owner: UserId,
    key: OperationKey,
    digest: RequestDigest,
    now: UtcTimestamp,
}

impl SqlTrashRestoreStore {
    /// 以数据库、账号、幂等键与当前时间装配（保留期判定用服务端时间）。
    #[must_use]
    pub fn new(
        database: Database,
        owner: UserId,
        key: OperationKey,
        digest: RequestDigest,
        now: UtcTimestamp,
    ) -> Self {
        Self {
            database,
            owner,
            key,
            digest,
            now,
        }
    }
}

impl TrashRestoreStore for SqlTrashRestoreStore {
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_roots::root_in_scope(&mut conn, self.owner, root)
            .await
            .map_err(restore_error)?;
        root_facts_of(record, root)
    }

    async fn operation(
        &mut self,
        key: OperationKey,
    ) -> Result<Option<RecordedTrashRestore>, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_operations::operation_in_scope(&mut conn, self.owner, &store_key(key))
            .await
            .map_err(operation_failure)?;
        Ok(record.map(|record| RecordedTrashRestore {
            digest: RequestDigest::from_bytes(*record.digest.as_bytes()),
            result: match record.state {
                store_operations::OperationState::Committed => record
                    .result_json
                    .as_deref()
                    .and_then(trash_result_from_json),
                _ => None,
            },
        }))
    }

    async fn begin_restore(
        &mut self,
        key: OperationKey,
        digest: RequestDigest,
    ) -> Result<RestoreBegin, RestoreStoreError> {
        let mut tx = self.database.begin_write().await.map_err(restore_error)?;
        let began = store_operations::begin_operation(
            &mut tx,
            self.owner,
            &store_operations::NewOperation {
                key: store_key(key),
                digest: store_digest(digest),
            },
        )
        .await
        .map_err(operation_failure)?;
        tx.commit().await.map_err(restore_error)?;
        Ok(match began {
            store_operations::BeginOperation::Started => RestoreBegin::Started,
            store_operations::BeginOperation::Replayed(_) => RestoreBegin::Recorded,
        })
    }

    async fn trash_subtree(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<Option<TrashSubtree>, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let rows = store_trash::trash_entries(&mut conn, self.owner, root)
            .await
            .map_err(restore_error)?;
        let Some(root_row) = rows.iter().find(|row| row.entry == entry) else {
            return Ok(None);
        };
        if root_row.expires_at < self.now {
            return Ok(None);
        }
        let prefix = format!("{}/", root_row.path);
        let mut entries: Vec<TrashEntryFacts> = rows
            .iter()
            .filter(|row| {
                row.entry == entry || row.path == root_row.path || row.path.starts_with(&prefix)
            })
            .map(|row| TrashEntryFacts {
                entry: row.entry,
                parent: row.parent,
                path: row.path.clone(),
                kind: match row.kind {
                    store_entries::EntryKind::File => EntryKind::File,
                    store_entries::EntryKind::Directory => EntryKind::Directory,
                },
                revision: row.revision,
                content_version: row.content_version,
                deleted_at: row.deleted_at,
                expires_at: row.expires_at,
            })
            .collect();
        entries.sort_by(|left, right| {
            left.path
                .matches('/')
                .count()
                .cmp(&right.path.matches('/').count())
                .then_with(|| left.path.cmp(&right.path))
        });
        if entries.first().map(|item| item.entry) != Some(entry) {
            return Err(RestoreStoreError::Rejected {
                code: "trash_subtree_order",
                detail: "回收站清单的根不在第一位".to_owned(),
            });
        }
        Ok(Some(TrashSubtree { entries }))
    }

    async fn parent_kind(
        &mut self,
        root: RootId,
        parent: EntryId,
    ) -> Result<Option<EntryKind>, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_entries::entry_in_scope(&mut conn, self.owner, parent)
            .await
            .map_err(restore_error)?;
        Ok(record
            .filter(|record| record.root == root && record.deleted_at.is_none())
            .map(|record| match record.kind {
                store_entries::EntryKind::File => EntryKind::File,
                store_entries::EntryKind::Directory => EntryKind::Directory,
            }))
    }

    async fn name_occupant(
        &mut self,
        root: RootId,
        parent: EntryId,
        name: &str,
    ) -> Result<Option<EntryId>, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let occupant =
            store_entries::active_child_by_name(&mut conn, self.owner, root, parent, name)
                .await
                .map_err(restore_error)?;
        Ok(occupant.map(|record| record.id))
    }

    async fn content_facts(
        &mut self,
        root: RootId,
        version: VersionId,
    ) -> Result<Option<ExpectedContent>, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let facts = store_versions::version_facts(&mut conn, self.owner, root, version)
            .await
            .map_err(restore_error)?;
        Ok(facts.map(|facts| content_from_version(&facts)))
    }

    async fn apply_trash_restore(
        &mut self,
        write: &TrashRestoreWrite,
    ) -> Result<TrashRestoreRecord, RestoreStoreError> {
        let mut tx = self.database.begin_write().await.map_err(restore_error)?;
        // 目标名称在事务内再核对一次：并发新建在这里被唯一索引或显式检查拒绝。
        let root_parent = write
            .entries
            .first()
            .map(|entry| entry.parent)
            .unwrap_or(write.entry);
        let occupant = store_entries::active_child_by_name(
            &mut tx,
            self.owner,
            self.key.root,
            root_parent,
            &write.name,
        )
        .await
        .map_err(restore_error)?;
        if occupant.is_some_and(|record| record.id != write.entry) {
            return Err(RestoreStoreError::NameOccupied {
                path: write.path.clone(),
            });
        }
        let tx_id = TxId::from_bytes(random_bytes());
        let events = serde_json::Value::Array(
            write
                .entries
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "kind": "restore",
                        "entry_id": entry.entry.to_hex(),
                        "path": entry.path,
                        "revision": entry.expected_revision.to_string(),
                        "origin": "client",
                    })
                })
                .collect(),
        )
        .to_string();
        let result = CommittedTrashRestore {
            op: self.key.op,
            root: self.key.root,
            entry: write.entry,
            path: write.path.clone(),
            name: write.name.clone(),
            renamed: write.renamed,
            revision: write
                .entries
                .first()
                .map(|entry| entry.expected_revision)
                .unwrap_or(Revision::ZERO)
                .next()
                .map_err(restore_error)?,
            seq: vaultsyncd_domain::version::RootSeq::ZERO,
            entries: write.entries.len() as u64,
        };
        let committed = store_operations::commit_operation(
            &mut tx,
            self.owner,
            &store_key(self.key),
            tx_id,
            &events,
            &trash_result_json(&result),
            write.at,
        )
        .await
        .map_err(operation_failure)?;
        let mut result = result;
        result.seq = committed.batch.seq;
        persist_result_seq(&mut tx, self.key, &trash_result_json(&result))
            .await
            .map_err(restore_error)?;
        for entry in &write.entries {
            let next = entry.expected_revision.next().map_err(restore_error)?;
            let revision = if entry.entry == write.entry {
                result.revision
            } else {
                next
            };
            store_entries::restore_entry(
                &mut tx,
                self.owner,
                entry.entry,
                entry.parent,
                &entry.name,
                &entry.name,
                entry.expected_revision,
                revision,
            )
            .await
            .map_err(restore_error)?;
            store_trash::remove_trash_entry(&mut tx, self.owner, self.key.root, entry.entry)
                .await
                .map_err(restore_error)?;
            store_trash::remove_tombstone(&mut tx, self.owner, self.key.root, entry.entry)
                .await
                .map_err(restore_error)?;
        }
        for intent in &write.intents {
            store_intents::advance_intent_phase(
                &mut tx,
                self.owner,
                *intent,
                FsIntentPhase::Finalized,
                write.at,
            )
            .await
            .map_err(restore_error)?;
        }
        tx.commit().await.map_err(restore_error)?;
        let _ = &self.digest;
        Ok(TrashRestoreRecord {
            intents: write.intents.clone(),
            phase: vaultsyncd_server_core::commit::intent::PutPhase::Finalized,
            tx: committed.batch.tx,
            result,
        })
    }
}

/// 历史版本恢复命令的输入。
#[derive(Clone, Debug)]
pub struct VersionRestoreCommand {
    /// 被恢复内容的条目。
    pub entry: EntryId,
    /// 要恢复的旧版本。
    pub version: VersionId,
    /// 客户端观察到的条目 revision。
    pub base_revision: Revision,
    /// 目标在 Root 内的相对路径（最后一段是条目当前名称）。
    pub target_path: String,
}

/// 执行一次历史版本恢复（发布旧内容为新的当前版本，旧版本行不变）。
///
/// # Errors
///
/// 见 [`OperationFailure`]：形状、基线、版本过期、租约/发布/存储失败。
pub async fn execute_version_restore(
    executor: &crate::executor::ExecutorContext,
    owner: UserId,
    key: OperationKey,
    digest: RequestDigest,
    command: &VersionRestoreCommand,
    expected_epoch: vaultsyncd_domain::version::ServerEpoch,
    at: UtcTimestamp,
) -> Result<RestoredVersion, OperationFailure> {
    let handle = executor.root_handle(owner, key.root).await?;
    let layout = executor.objects();
    let mut target = crate::executor::WorkTreeTarget::new(handle, layout);
    let before = match crate::executor::observe_before(
        &mut target,
        &command.target_path,
        command.entry,
        command.base_revision,
    ) {
        Ok(before) => before,
        Err(failure) => return Err(failure),
    };
    let mut store = SqlRestoreStore::new(executor.database().clone(), owner, key, digest);
    let mut log = crate::executor::SqlIntentLog::new(executor.database().clone(), owner);
    let mut leases = crate::content::DbLeases::new(executor.database(), owner, at);
    let request = VersionRestoreRequest {
        intent_id: vaultsyncd_domain::ids::IntentId::from_bytes(random_bytes()),
        op: key,
        digest,
        entry: command.entry,
        version: command.version,
        expected_epoch,
        expected_revision: command.base_revision,
        target_path: command.target_path.clone(),
        staging_path: format!("restore-{}", key.op.to_hex()),
        before,
    };
    let mut gate = executor.commit_gate().lock().await;
    executor.ensure_no_unfinished(owner, key.root).await?;
    let outcome = vaultsyncd_server_core::history::version::restore_version(
        &mut gate,
        &mut store,
        &mut log,
        &mut target,
        &mut leases,
        at,
        &request,
    )
    .await;
    drop(gate);
    match outcome {
        Ok(restored) => Ok(restored),
        Err(failure) => Err(crate::commands::map_core_failure(
            failure.error_code(),
            failure.to_string(),
        )),
    }
}

/// 回收站恢复命令的输入。
#[derive(Clone, Debug)]
pub struct TrashRestoreCommand {
    /// 回收站里的条目。
    pub entry: EntryId,
    /// 客户端看到的清单 revision。
    pub base_revision: Revision,
    /// 当前只接受 `None`（回原父目录），显式换父目录需要先恢复后 move。
    pub parent: Option<EntryId>,
}

/// 执行一次回收站恢复（目录连同后代整棵放回；原位置被占用时另名）。
///
/// # Errors
///
/// 见 [`OperationFailure`]：清单过期、父目录不可用、名称占用、租约/发布/存储失败。
pub async fn execute_trash_restore(
    executor: &crate::executor::ExecutorContext,
    owner: UserId,
    key: OperationKey,
    digest: RequestDigest,
    command: &TrashRestoreCommand,
    expected_epoch: vaultsyncd_domain::version::ServerEpoch,
    at: UtcTimestamp,
) -> Result<RestoredTrash, OperationFailure> {
    if command.parent.is_some() {
        return Err(OperationFailure::InvalidRequest {
            detail: "回收站恢复按冻结清单的原父目录执行；改变位置请在恢复后提交 move".to_owned(),
        });
    }
    let handle = executor.root_handle(owner, key.root).await?;
    let layout = executor.objects();
    let mut target = crate::executor::WorkTreeTarget::new(handle.clone(), layout);
    let mut ns = crate::commands::WorkTreeNamespace::new(handle);
    let mut store = SqlTrashRestoreStore::new(executor.database().clone(), owner, key, digest, at);
    let mut log = crate::executor::SqlIntentLog::new(executor.database().clone(), owner);
    let mut leases = crate::content::DbLeases::new(executor.database(), owner, at);
    let request = TrashRestoreRequest {
        intent_seed: vaultsyncd_domain::ids::IntentId::from_bytes(random_bytes()),
        op: key,
        digest,
        entry: command.entry,
        expected_epoch,
        expected_revision: command.base_revision,
    };
    let mut ports = TrashRestorePorts {
        store: &mut store,
        log: &mut log,
        target: &mut target,
        namespace: &mut ns,
        leases: &mut leases,
    };
    let mut gate = executor.commit_gate().lock().await;
    executor.ensure_no_unfinished(owner, key.root).await?;
    // 目录恢复的核心当前在首个文件意图前 mkdir；缺少整组目录的持久恢复计划。
    // 在 S052 补完该证据之前，必须显式未就绪，不能先改磁盘再假装可恢复。
    let mut conn = executor
        .database()
        .pool()
        .acquire()
        .await
        .map_err(|error| OperationFailure::Storage {
            detail: error.to_string(),
        })?;
    let rows = store_trash::trash_entries(&mut conn, owner, key.root)
        .await
        .map_err(|error| OperationFailure::Storage {
            detail: error.to_string(),
        })?;
    if rows
        .iter()
        .any(|row| row.entry == command.entry && row.kind == store_entries::EntryKind::Directory)
    {
        return Err(OperationFailure::ExecutorUnavailable);
    }
    drop(conn);
    let outcome =
        vaultsyncd_server_core::history::trash::restore_trash(&mut gate, &mut ports, at, &request)
            .await;
    drop(gate);
    match outcome {
        Ok(restored) => Ok(restored),
        Err(failure) => Err(crate::commands::map_core_failure(
            failure.error_code(),
            failure.to_string(),
        )),
    }
}

/// 由端口路径构造恢复请求里的 `TrashRestoreEntry`（供核心之外的使用方与测试复用）。
#[must_use]
pub fn trash_restore_entry(
    entry: EntryId,
    parent: EntryId,
    name: &str,
    path: &str,
    kind: EntryKind,
    content_version: Option<VersionId>,
    expected_revision: Revision,
) -> TrashRestoreEntry {
    TrashRestoreEntry {
        entry,
        parent,
        name: name.to_owned(),
        path: path.to_owned(),
        kind,
        content_version,
        expected_revision,
    }
}
