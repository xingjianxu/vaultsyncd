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

use vaultsyncd_domain::ids::{EntryId, TxId, UserId, VaultId, VersionId};
use vaultsyncd_domain::version::{Revision, UtcTimestamp};
use vaultsyncd_server_core::api::operations::OperationFailure;
use vaultsyncd_server_core::commit::namespace::{NamespaceAfter, NamespaceBefore};
use vaultsyncd_server_core::history::trash::{
    CommittedTrashRestore, RestoredTrash, TrashRestorePorts, TrashRestoreRequest,
};
use vaultsyncd_server_core::history::version::{
    CommittedVersionRestore, RestoredVersion, VersionRestoreRequest,
};
use vaultsyncd_server_core::objects::ExpectedContent;
use vaultsyncd_server_core::ports::conflict_store::{OperationKey, RequestDigest};
use vaultsyncd_server_core::ports::put_store::{EntryKind, VaultFacts};
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
use vaultsyncd_server_store::trash as store_trash;
use vaultsyncd_server_store::vaults as store_vaults;
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

fn vault_facts_of(
    record: Option<store_vaults::VaultRecord>,
    vault: VaultId,
) -> Result<VaultFacts, RestoreStoreError> {
    record
        .map(|record| VaultFacts {
            epoch: record.epoch,
        })
        .ok_or(RestoreStoreError::UnknownVault { vault })
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
        vault: parse_id(value.get("vault_id")?.as_str()?)?,
        entry: parse_id(value.get("entry_id")?.as_str()?)?,
        version: parse_id(value.get("version_id")?.as_str()?)?,
        restored_from: parse_id(value.get("restored_from")?.as_str()?)?,
        content: ExpectedContent::new(
            vaultsyncd_server_core::objects::ContentDigest::from_hex(hash).ok()?,
            size,
        ),
        revision: Revision::from_decimal(value.get("revision")?.as_str()?).ok()?,
        seq: vaultsyncd_domain::version::VaultSeq::from_decimal(value.get("vault_seq")?.as_str()?)
            .ok()?,
        path: value.get("path")?.as_str()?.to_owned(),
    })
}

/// 操作记录里的结果 JSON → 回收站恢复结果。
fn trash_result_from_json(text: &str) -> Option<CommittedTrashRestore> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(CommittedTrashRestore {
        op: parse_id(value.get("op_id")?.as_str()?)?,
        vault: parse_id(value.get("vault_id")?.as_str()?)?,
        entry: parse_id(value.get("entry_id")?.as_str()?)?,
        path: value.get("path")?.as_str()?.to_owned(),
        name: value.get("name")?.as_str()?.to_owned(),
        renamed: value.get("renamed")?.as_bool()?,
        revision: Revision::from_decimal(value.get("revision")?.as_str()?).ok()?,
        seq: vaultsyncd_domain::version::VaultSeq::from_decimal(value.get("vault_seq")?.as_str()?)
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
        vault: key.vault,
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
    sqlx::query("UPDATE operations SET result_json = ?1 WHERE vault_id = ?2 AND device_id = ?3 AND op_id = ?4 AND state = 'committed'")
        .bind(result)
        .bind(key.vault.as_bytes().as_slice())
        .bind(key.device.as_bytes().as_slice())
        .bind(key.op.as_bytes().as_slice())
        .execute(&mut **tx).await?;
    Ok(())
}

fn version_result_json(result: &CommittedVersionRestore) -> String {
    serde_json::json!({
        "op_id": result.op.to_hex(),
        "vault_id": result.vault.to_hex(),
        "entry_id": result.entry.to_hex(),
        "version_id": result.version.to_hex(),
        "restored_from": result.restored_from.to_hex(),
        "hash": result.content.digest.to_hex(),
        "size": result.content.bytes.to_string(),
        "revision": result.revision.to_string(),
        "vault_seq": result.seq.to_string(),
        "path": result.path,
        "state": "committed",
    })
    .to_string()
}

fn trash_result_json(result: &CommittedTrashRestore) -> String {
    serde_json::json!({
        "op_id": result.op.to_hex(),
        "vault_id": result.vault.to_hex(),
        "entry_id": result.entry.to_hex(),
        "path": result.path,
        "name": result.name,
        "renamed": result.renamed,
        "revision": result.revision.to_string(),
        "vault_seq": result.seq.to_string(),
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
    local_generation: Option<vaultsyncd_domain::version::Generation>,
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
            local_generation: None,
        }
    }

    /// 客户端本地代际：写进持久结果，重放与重启补完都回显它。
    #[must_use]
    pub fn with_local_generation(
        mut self,
        generation: Option<vaultsyncd_domain::version::Generation>,
    ) -> Self {
        self.local_generation = generation;
        self
    }
}

impl RestoreStore for SqlRestoreStore {
    async fn vault_facts(&mut self, vault: VaultId) -> Result<VaultFacts, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_vaults::vault_in_scope(&mut conn, self.owner, vault)
            .await
            .map_err(restore_error)?;
        vault_facts_of(record, vault)
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
        vault: VaultId,
        entry: EntryId,
        version: VersionId,
    ) -> Result<Facts, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_entries::entry_in_scope(&mut conn, self.owner, entry)
            .await
            .map_err(restore_error)?
            .ok_or(RestoreStoreError::EntryMissing { entry })?;
        if record.vault != vault || record.deleted_at.is_some() {
            return Err(RestoreStoreError::EntryMissing { entry });
        }
        let parent = record.parent.unwrap_or(entry);
        let facts = store_versions::version_facts(&mut conn, self.owner, vault, version)
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
                vault,
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
        if write.key != self.key || write.digest != self.digest {
            return Err(restore_error("提交的操作键或摘要不是本存储装配的那一个"));
        }
        let mut tx = self.database.begin_write().await.map_err(restore_error)?;
        crate::journal::ensure_digest(
            &mut tx,
            self.owner,
            &store_key(write.key),
            &store_digest(write.digest),
        )
        .await
        .map_err(restore_error)?;
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
            vault: self.key.vault,
            entry: write.entry,
            version,
            restored_from: write.restored_from,
            content: write.content,
            revision: next,
            seq: vaultsyncd_domain::version::VaultSeq::ZERO,
            path: write.path.clone(),
        };
        let committed = store_operations::commit_operation(
            &mut tx,
            self.owner,
            &store_key(self.key),
            tx_id,
            &events,
            &crate::journal::with_echo(version_result_json(&result), self.local_generation),
            write.at,
        )
        .await
        .map_err(operation_failure)?;
        let mut result = result;
        result.seq = committed.batch.seq;
        persist_result_seq(
            &mut tx,
            self.key,
            &crate::journal::with_echo(version_result_json(&result), self.local_generation),
        )
        .await
        .map_err(restore_error)?;
        store_versions::record_version(
            &mut tx,
            self.owner,
            &NewVersion {
                id: version,
                vault: write.key.vault,
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
            write.at,
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
    local_generation: Option<vaultsyncd_domain::version::Generation>,
}

impl SqlTrashRestoreStore {
    /// 以数据库、账号与幂等键装配（保留期由核心按服务端时间判定）。
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
            local_generation: None,
        }
    }

    /// 客户端本地代际：随恢复计划一起持久化，重放与重启补完都回显同一个值。
    #[must_use]
    pub fn with_local_generation(
        mut self,
        generation: Option<vaultsyncd_domain::version::Generation>,
    ) -> Self {
        self.local_generation = generation;
        self
    }
}

fn kind_text(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Directory => "directory",
    }
}

/// 整组恢复计划的持久形态：冻结清单（条目身份、种类、内容版本、revision）、实际路径/另名决定与
/// 每个意图的身份。恢复只信这份快照，不再按路径前缀或最新操作行推断。
pub(crate) fn plan_context(
    plan: &TrashRestoreWrite,
    local_generation: Option<vaultsyncd_domain::version::Generation>,
) -> serde_json::Value {
    serde_json::json!({
        "entry": plan.entry.to_hex(),
        "revision": plan.expected_revision.to_string(),
        "path": plan.path,
        "name": plan.name,
        "renamed": plan.renamed,
        "intents": plan.intents.iter().map(|id| id.to_hex()).collect::<Vec<_>>(),
        "directory_intents": plan.directory_intents.iter().map(|id| id.to_hex()).collect::<Vec<_>>(),
        "local_generation": local_generation.map(|value| value.to_string()),
        "entries": plan.entries.iter().map(|entry| serde_json::json!({
            "entry": entry.entry.to_hex(),
            "parent": entry.parent.to_hex(),
            "name": entry.name,
            "path": entry.path,
            "kind": kind_text(entry.kind),
            "content_version": entry.content_version.map(|version| version.to_hex()),
            "revision": entry.expected_revision.to_string(),
        })).collect::<Vec<_>>(),
    })
}

/// 从持久上下文还原整组计划；任何字段缺失或不合法都报错，由调用方保持停写。
pub(crate) fn plan_from_context(
    context: &serde_json::Value,
    key: OperationKey,
    digest: RequestDigest,
    at: UtcTimestamp,
) -> Result<TrashRestoreWrite, String> {
    fn text<'a>(value: &'a serde_json::Value, field: &str) -> Result<&'a str, String> {
        value
            .get(field)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("恢复计划缺少 {field}"))
    }
    fn id<T: std::str::FromStr>(value: &serde_json::Value, field: &str) -> Result<T, String> {
        text(value, field)?
            .parse()
            .map_err(|_| format!("恢复计划字段 {field} 不合法"))
    }
    fn ids(
        value: &serde_json::Value,
        field: &str,
    ) -> Result<Vec<vaultsyncd_domain::ids::IntentId>, String> {
        value
            .get(field)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| format!("恢复计划缺少 {field}"))?
            .iter()
            .map(|item| {
                item.as_str()
                    .and_then(|text| text.parse().ok())
                    .ok_or_else(|| format!("恢复计划 {field} 含非法身份"))
            })
            .collect()
    }
    let entries = context
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "恢复计划缺少 entries".to_owned())?
        .iter()
        .map(|item| {
            Ok(TrashRestoreEntry {
                entry: id(item, "entry")?,
                parent: id(item, "parent")?,
                name: text(item, "name")?.to_owned(),
                path: text(item, "path")?.to_owned(),
                kind: match text(item, "kind")? {
                    "file" => EntryKind::File,
                    "directory" => EntryKind::Directory,
                    other => return Err(format!("恢复计划含未知种类 {other}")),
                },
                content_version: match item.get("content_version") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(value) => Some(
                        value
                            .as_str()
                            .and_then(|text| text.parse().ok())
                            .ok_or_else(|| "恢复计划内容版本不合法".to_owned())?,
                    ),
                },
                expected_revision: Revision::from_decimal(text(item, "revision")?)
                    .map_err(|error| error.to_string())?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let write = TrashRestoreWrite {
        intents: ids(context, "intents")?,
        directory_intents: ids(context, "directory_intents")?,
        key,
        digest,
        entry: id(context, "entry")?,
        expected_revision: Revision::from_decimal(text(context, "revision")?)
            .map_err(|error| error.to_string())?,
        path: text(context, "path")?.to_owned(),
        name: text(context, "name")?.to_owned(),
        renamed: context
            .get("renamed")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| "恢复计划缺少 renamed".to_owned())?,
        entries,
        at,
    };
    let files = write
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
        .count();
    if write.intents.len() != files
        || write.directory_intents.len() != write.entries.len() - files
        || write.entries.first().map(|entry| entry.entry) != Some(write.entry)
    {
        return Err("恢复计划的意图数量与清单不一致".to_owned());
    }
    Ok(write)
}

impl TrashRestoreStore for SqlTrashRestoreStore {
    async fn vault_facts(&mut self, vault: VaultId) -> Result<VaultFacts, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_vaults::vault_in_scope(&mut conn, self.owner, vault)
            .await
            .map_err(restore_error)?;
        vault_facts_of(record, vault)
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
        plan: &TrashRestoreWrite,
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
        if matches!(began, store_operations::BeginOperation::Started) {
            // 同一事务：操作 pending、整组计划、全部目录的 prepared 意图。三者要么都在要么都不在。
            let binding = crate::journal::Binding::new(
                store_key(key),
                "trash_restore",
                plan_context(plan, self.local_generation),
            );
            binding.persist(&mut tx).await.map_err(restore_error)?;
            let directories = plan
                .entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::Directory);
            for (entry, intent) in directories.zip(&plan.directory_intents) {
                store_intents::begin_intent(
                    &mut tx,
                    self.owner,
                    &store_intents::NewFsIntent {
                        intent_id: *intent,
                        vault: key.vault,
                        kind: store_intents::FsIntentKind::Create,
                        target_path: entry.path.clone(),
                        staging_path: None,
                        expected_before: NamespaceBefore::Absent.to_json(),
                        desired_after: NamespaceAfter {
                            entry: entry.entry,
                            parent: entry.parent,
                            path: entry.path.clone(),
                            kind: EntryKind::Directory,
                        }
                        .to_json(),
                    },
                    plan.at,
                )
                .await
                .map_err(restore_error)?;
                binding
                    .attach(&mut tx, *intent)
                    .await
                    .map_err(restore_error)?;
            }
        }
        tx.commit().await.map_err(restore_error)?;
        Ok(match began {
            store_operations::BeginOperation::Started => RestoreBegin::Started,
            store_operations::BeginOperation::Replayed(_) => RestoreBegin::Recorded,
        })
    }

    async fn trash_subtree(
        &mut self,
        vault: VaultId,
        entry: EntryId,
    ) -> Result<Option<TrashSubtree>, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let rows = store_trash::trash_entries(&mut conn, self.owner, vault)
            .await
            .map_err(restore_error)?;
        let Some(vault_row) = rows.iter().find(|row| row.entry == entry) else {
            return Ok(None);
        };
        // 过期的条目不在这里当成“不存在”：核心按保留期判 `VERSION_GONE`（410，历史已过期），
        // 客户端才能区分“已被恢复/清单变了”与“保留期已过”。
        // 范围由删除时持久化的“批次归属”决定，而不是旧路径前缀：同一路径下另一轮删除的条目
        // （先恢复/重建再删除、同名新目录）属于别的批次，绝不会被选入。没有归属的行（外部采纳
        // 的单条删除）只恢复它自己，其后代各自是独立的回收站条目。
        let membership: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT deletion_id FROM trash_membership WHERE entry_id = ?1")
                .bind(entry.as_bytes().as_slice())
                .fetch_optional(&mut *conn)
                .await
                .map_err(restore_error)?;
        let batch: std::collections::BTreeSet<EntryId> = match membership {
            None => std::iter::once(entry).collect(),
            Some(deletion) => {
                let members: Vec<Vec<u8>> = sqlx::query_scalar(
                    "SELECT entry_id FROM trash_membership WHERE deletion_id = ?1",
                )
                .bind(deletion)
                .fetch_all(&mut *conn)
                .await
                .map_err(restore_error)?;
                members
                    .into_iter()
                    .filter_map(|bytes| <[u8; 16]>::try_from(bytes).ok())
                    .map(EntryId::from_bytes)
                    .collect()
            }
        };
        // 沿父子身份从根向下取闭包（只在该批次里走）。
        let mut selected: Vec<&store_trash::TrashEntry> = vec![vault_row];
        let mut frontier = vec![entry];
        while let Some(parent) = frontier.pop() {
            for row in rows.iter().filter(|row| {
                row.parent == parent && row.entry != entry && batch.contains(&row.entry)
            }) {
                if selected.iter().all(|seen| seen.entry != row.entry) {
                    selected.push(row);
                    frontier.push(row.entry);
                }
            }
        }
        let mut entries: Vec<TrashEntryFacts> = selected
            .into_iter()
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

    async fn parent_path(
        &mut self,
        vault: VaultId,
        parent: EntryId,
    ) -> Result<Option<String>, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_entries::entry_in_scope(&mut conn, self.owner, parent)
            .await
            .map_err(restore_error)?;
        let Some(record) =
            record.filter(|record| record.vault == vault && record.deleted_at.is_none())
        else {
            return Ok(None);
        };
        if record.parent.is_none() {
            return Ok(Some(String::new()));
        }
        store_entries::path_of(&mut conn, self.owner, parent)
            .await
            .map_err(restore_error)
    }

    async fn parent_kind(
        &mut self,
        vault: VaultId,
        parent: EntryId,
    ) -> Result<Option<EntryKind>, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_entries::entry_in_scope(&mut conn, self.owner, parent)
            .await
            .map_err(restore_error)?;
        Ok(record
            .filter(|record| record.vault == vault && record.deleted_at.is_none())
            .map(|record| match record.kind {
                store_entries::EntryKind::File => EntryKind::File,
                store_entries::EntryKind::Directory => EntryKind::Directory,
            }))
    }

    async fn name_occupant(
        &mut self,
        vault: VaultId,
        parent: EntryId,
        name: &str,
    ) -> Result<Option<EntryId>, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let occupant =
            store_entries::active_child_by_name(&mut conn, self.owner, vault, parent, name)
                .await
                .map_err(restore_error)?;
        Ok(occupant.map(|record| record.id))
    }

    async fn content_facts(
        &mut self,
        vault: VaultId,
        version: VersionId,
    ) -> Result<Option<ExpectedContent>, RestoreStoreError> {
        let mut conn = connect(&self.database).await?;
        let facts = store_versions::version_facts(&mut conn, self.owner, vault, version)
            .await
            .map_err(restore_error)?;
        Ok(facts.map(|facts| content_from_version(&facts)))
    }

    async fn apply_trash_restore(
        &mut self,
        write: &TrashRestoreWrite,
    ) -> Result<TrashRestoreRecord, RestoreStoreError> {
        if write.key != self.key || write.digest != self.digest {
            return Err(restore_error("提交的操作键或摘要不是本存储装配的那一个"));
        }
        let mut tx = self.database.begin_write().await.map_err(restore_error)?;
        crate::journal::ensure_digest(
            &mut tx,
            self.owner,
            &store_key(write.key),
            &store_digest(write.digest),
        )
        .await
        .map_err(restore_error)?;
        // 目标名称在事务内再核对一次：并发新建在这里被唯一索引或显式检查拒绝。
        let vault_parent = write
            .entries
            .first()
            .map(|entry| entry.parent)
            .unwrap_or(write.entry);
        let occupant = store_entries::active_child_by_name(
            &mut tx,
            self.owner,
            self.key.vault,
            vault_parent,
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
        let mut restored_events = Vec::with_capacity(write.entries.len());
        for entry in &write.entries {
            let next = entry.expected_revision.next().map_err(restore_error)?;
            let content: Option<(Vec<u8>, i64)> = if let Some(version) = entry.content_version {
                Some(sqlx::query_as("SELECT object_hash,size FROM versions WHERE vault_id=? AND entry_id=? AND id=?")
                    .bind(self.key.vault.as_bytes().as_slice()).bind(entry.entry.as_bytes().as_slice()).bind(version.as_bytes().as_slice())
                    .fetch_one(&mut *tx).await.map_err(restore_error)?)
            } else {
                None
            };
            restored_events.push(serde_json::json!({
                "kind": "restore", "entry_id": entry.entry.to_hex(), "parent_id":entry.parent.to_hex(),
                "name":entry.name, "path":entry.path, "revision":next.to_string(), "origin":"client",
                "version_id":entry.content_version.map(|v|v.to_hex()),
                "hash":content.as_ref().map(|(hash,_)|hash.iter().map(|b|format!("{b:02x}")).collect::<String>()),
                "size":content.map(|(_,size)|size.to_string()),
            }));
        }
        let events = serde_json::Value::Array(restored_events).to_string();
        let result = CommittedTrashRestore {
            op: self.key.op,
            vault: self.key.vault,
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
            seq: vaultsyncd_domain::version::VaultSeq::ZERO,
            entries: write.entries.len() as u64,
        };
        let committed = store_operations::commit_operation(
            &mut tx,
            self.owner,
            &store_key(self.key),
            tx_id,
            &events,
            &crate::journal::with_echo(trash_result_json(&result), self.local_generation),
            write.at,
        )
        .await
        .map_err(operation_failure)?;
        let mut result = result;
        result.seq = committed.batch.seq;
        persist_result_seq(
            &mut tx,
            self.key,
            &crate::journal::with_echo(trash_result_json(&result), self.local_generation),
        )
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
                write.at,
            )
            .await
            .map_err(restore_error)?;
            store_trash::remove_trash_entry(&mut tx, self.owner, self.key.vault, entry.entry)
                .await
                .map_err(restore_error)?;
            store_trash::remove_tombstone(&mut tx, self.owner, self.key.vault, entry.entry)
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
        // 目录意图：核心已核对目录存在（或补建完成），这里把 prepared 经 published 一并收尾。
        for intent in &write.directory_intents {
            let record = store_intents::intent_in_scope(&mut tx, self.owner, *intent)
                .await
                .map_err(restore_error)?
                .ok_or(RestoreStoreError::IllegalIntentTransition {
                    from: vaultsyncd_server_core::commit::intent::PutPhase::Prepared,
                    to: vaultsyncd_server_core::commit::intent::PutPhase::Finalized,
                })?;
            if record.phase == FsIntentPhase::Prepared {
                store_intents::advance_intent_phase(
                    &mut tx,
                    self.owner,
                    *intent,
                    FsIntentPhase::Published,
                    write.at,
                )
                .await
                .map_err(restore_error)?;
            }
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
    /// 目标在 Vault 内的相对路径（最后一段是条目当前名称）。
    pub target_path: String,
    /// 客户端本地代际（持久回显）。
    pub local_generation: Option<vaultsyncd_domain::version::Generation>,
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
    let handle = executor.vault_handle(owner, key.vault).await?;
    let layout = crate::object_layout::layout(executor.state_dir(), key.vault);
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
    let mut store = SqlRestoreStore::new(executor.database().clone(), owner, key, digest)
        .with_local_generation(command.local_generation);
    let mut log = crate::executor::SqlIntentLog::new(executor.database().clone(), owner).bound(
        crate::journal::Binding::new(store_key(key), "version_restore", serde_json::json!({
            "entry": command.entry.to_hex(), "revision": command.base_revision.to_string(), "restored_from": command.version.to_hex(),
            "local_generation": command.local_generation.map(|value| value.to_string()),
        })));
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
    executor.ensure_no_unfinished(owner, key.vault).await?;
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
        Err(failure) => {
            let mapped = crate::commands::restore_failure(&failure);
            executor
                .reject_failure(owner, store_key(key), &mapped)
                .await;
            Err(mapped)
        }
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
    /// 客户端本地代际（持久回显）。
    pub local_generation: Option<vaultsyncd_domain::version::Generation>,
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
    let handle = executor.vault_handle(owner, key.vault).await?;
    let layout = crate::object_layout::layout(executor.state_dir(), key.vault);
    let mut target = crate::executor::WorkTreeTarget::new(handle.clone(), layout);
    let mut ns = crate::commands::WorkTreeNamespace::new(handle);
    let mut store = SqlTrashRestoreStore::new(executor.database().clone(), owner, key, digest)
        .with_local_generation(command.local_generation);
    // 计划与目录意图由 begin_restore 一并落盘；文件意图在发布前创建时只与该操作挂钩。
    let mut log = crate::executor::SqlIntentLog::new(executor.database().clone(), owner)
        .bound(crate::journal::Binding::linked(store_key(key)));
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
    executor.ensure_no_unfinished(owner, key.vault).await?;
    let outcome =
        vaultsyncd_server_core::history::trash::restore_trash(&mut gate, &mut ports, at, &request)
            .await;
    drop(gate);
    match outcome {
        Ok(restored) => Ok(restored),
        Err(failure) => {
            let mapped = crate::commands::restore_failure(&failure);
            executor
                .reject_failure(owner, store_key(key), &mapped)
                .await;
            Err(mapped)
        }
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
