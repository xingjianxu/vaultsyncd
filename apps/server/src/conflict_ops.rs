//! 冲突副本（`preserve_both`）的存储适配器（S052 续接：design §10.1—10.2、§15.3、T02）。
//!
//! 过期基线上的迟到内容不会被丢弃、也不会覆盖服务器 head：核心 [`commit_conflict`] 把它提交成一个
//! 独立的冲突条目（`sibling`）。本模块实现 `ConflictCommitStore`：
//!
//! - `conflict_head`：Root epoch + 源条目当前 revision 与内容摘要（目录没有内容）；
//! - `operation` / `begin_conflict`：`operations` 表的幂等记录（同一 OpId 重试拿到同一副本）；
//! - `apply_conflict`：单一事务里写冲突条目、版本、`conflicts` 映射（迁移 0008）、事件批次、操作结果与
//!   意图收尾。目标名称已被占用时返回 `PathOccupied`，绝不覆盖。
//!
//! [`commit_conflict`]: vaultsyncd_server_core::commit::conflict::commit_conflict

use std::sync::Arc;

use vaultsyncd_domain::ids::{EntryId, RootId, TxId, UserId, VersionId};
use vaultsyncd_domain::version::{Revision, UtcTimestamp};
use vaultsyncd_platform_linux_server::files::RootHandle;
use vaultsyncd_server_core::commit::conflict::{
    CommittedConflict, ConflictRequest, commit_conflict,
};
use vaultsyncd_server_core::commit::intent::PutPhase;
use vaultsyncd_server_core::objects::ExpectedContent;
use vaultsyncd_server_core::ports::conflict_store::{
    ConflictBegin, ConflictCommitStore, ConflictHead, ConflictRecord, ConflictStoreError,
    ConflictWrite, HeadSource, OperationKey, RecordedOperation, RequestDigest,
};
use vaultsyncd_server_core::ports::put_store::EntryKind;
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::entries as store_entries;
use vaultsyncd_server_store::entries::NewEntry;
use vaultsyncd_server_store::intents as store_intents;
use vaultsyncd_server_store::intents::FsIntentPhase;
use vaultsyncd_server_store::operations as store_operations;
use vaultsyncd_server_store::roots as store_roots;
use vaultsyncd_server_store::versions as store_versions;
use vaultsyncd_server_store::versions::{NewVersion, VersionOrigin};

use crate::executor::{SqlIntentLog, WorkTreeTarget};
use crate::uploads::random_bytes;
use vaultsyncd_server_core::api::operations::OperationFailure;

fn conflict_error(detail: impl std::fmt::Display) -> ConflictStoreError {
    ConflictStoreError::Storage {
        detail: detail.to_string(),
    }
}

fn store_key(key: OperationKey) -> store_operations::OperationKey {
    store_operations::OperationKey {
        root: key.root,
        device: key.device,
        op: key.op,
    }
}

fn store_digest(digest: RequestDigest) -> store_operations::RequestDigest {
    store_operations::RequestDigest::from_bytes(*digest.as_bytes())
}

fn result_from_json(text: &str) -> Option<CommittedConflict> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(CommittedConflict {
        op: value.get("op_id")?.as_str()?.parse().ok()?,
        source: value.get("source_entry_id")?.as_str()?.parse().ok()?,
        sibling: value.get("sibling_entry_id")?.as_str()?.parse().ok()?,
        path: value.get("path")?.as_str()?.to_owned(),
        content: ExpectedContent::new(
            vaultsyncd_server_core::objects::ContentDigest::from_hex(value.get("hash")?.as_str()?)
                .ok()?,
            value.get("size")?.as_str()?.parse().ok()?,
        ),
        revision: value.get("revision")?.as_str()?.parse().ok()?,
        seq: value.get("root_seq")?.as_str()?.parse().ok()?,
    })
}

fn result_json(result: &CommittedConflict) -> String {
    // 同时是 wire `OperationResult` 的形状（`conflict` 对象给客户端指路）与重放解析的输入；
    // 多出来的 hash/size/path 只供服务端重建 `CommittedConflict`，客户端按数据处理。
    serde_json::json!({
        "op_id": result.op.to_hex(),
        "state": "committed",
        "entry_id": result.source.to_hex(),
        "revision": result.revision.to_string(),
        "root_seq": result.seq.to_string(),
        "conflict": {
            "conflict_entry_id": result.sibling.to_hex(),
            "entry_id": result.source.to_hex(),
            "reason": "stale_base",
        },
        "source_entry_id": result.source.to_hex(),
        "sibling_entry_id": result.sibling.to_hex(),
        "path": result.path,
        "hash": result.content.digest.to_hex(),
        "size": result.content.bytes.to_string(),
    })
    .to_string()
}

/// `ConflictCommitStore` 的 SQLx 实现。
pub struct SqlConflictStore {
    database: Database,
    owner: UserId,
    key: OperationKey,
    local_generation: Option<vaultsyncd_domain::version::Generation>,
}

impl SqlConflictStore {
    /// 以数据库、账号与本次操作的幂等键装配。
    #[must_use]
    pub fn new(database: Database, owner: UserId, key: OperationKey) -> Self {
        Self {
            database,
            owner,
            key,
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

impl ConflictCommitStore for SqlConflictStore {
    async fn conflict_head(
        &mut self,
        root: RootId,
        source: EntryId,
    ) -> Result<ConflictHead, ConflictStoreError> {
        let mut conn = self
            .database
            .pool()
            .acquire()
            .await
            .map_err(conflict_error)?;
        let root_record = store_roots::root_in_scope(&mut conn, self.owner, root)
            .await
            .map_err(conflict_error)?
            .ok_or(ConflictStoreError::UnknownRoot { root })?;
        let entry = store_entries::entry_in_scope(&mut conn, self.owner, source)
            .await
            .map_err(conflict_error)?;
        let source_facts = match entry {
            None => None,
            Some(record) if record.deleted_at.is_some() || record.root != root => None,
            Some(record) => {
                let content = if record.kind == store_entries::EntryKind::File {
                    store_versions::current_version(&mut conn, self.owner, root, source)
                        .await
                        .map_err(conflict_error)?
                        .map(|version| {
                            vaultsyncd_server_core::objects::ContentDigest::from_bytes(
                                version.object_hash,
                            )
                        })
                } else {
                    None
                };
                Some(HeadSource {
                    entry: record.id,
                    revision: record.revision,
                    kind: match record.kind {
                        store_entries::EntryKind::File => EntryKind::File,
                        store_entries::EntryKind::Directory => EntryKind::Directory,
                    },
                    content,
                })
            }
        };
        Ok(ConflictHead {
            epoch: root_record.epoch,
            source: source_facts,
        })
    }

    async fn operation(
        &mut self,
        key: OperationKey,
    ) -> Result<Option<RecordedOperation>, ConflictStoreError> {
        let mut conn = self
            .database
            .pool()
            .acquire()
            .await
            .map_err(conflict_error)?;
        let record = store_operations::operation_in_scope(&mut conn, self.owner, &store_key(key))
            .await
            .map_err(conflict_error)?;
        Ok(record.map(|record| RecordedOperation {
            digest: RequestDigest::from_bytes(*record.digest.as_bytes()),
            result: match record.state {
                store_operations::OperationState::Committed => {
                    record.result_json.as_deref().and_then(result_from_json)
                }
                _ => None,
            },
        }))
    }

    async fn begin_conflict(
        &mut self,
        key: OperationKey,
        digest: RequestDigest,
    ) -> Result<ConflictBegin, ConflictStoreError> {
        let mut tx = self.database.begin_write().await.map_err(conflict_error)?;
        let began = store_operations::begin_operation(
            &mut tx,
            self.owner,
            &store_operations::NewOperation {
                key: store_key(key),
                digest: store_digest(digest),
            },
        )
        .await
        .map_err(|error| match error {
            store_operations::OperationError::DigestMismatch { .. } => {
                ConflictStoreError::DuplicateOperation { op: key.op }
            }
            other => conflict_error(other),
        })?;
        tx.commit().await.map_err(conflict_error)?;
        match began {
            store_operations::BeginOperation::Started => Ok(ConflictBegin::Started),
            store_operations::BeginOperation::Replayed(record) => {
                let result = match record.state {
                    store_operations::OperationState::Committed => {
                        record.result_json.as_deref().and_then(result_from_json)
                    }
                    _ => None,
                };
                Ok(ConflictBegin::Recorded(RecordedOperation {
                    digest: RequestDigest::from_bytes(*record.digest.as_bytes()),
                    result,
                }))
            }
        }
    }

    async fn apply_conflict(
        &mut self,
        write: &ConflictWrite,
    ) -> Result<ConflictRecord, ConflictStoreError> {
        if write.key != self.key {
            return Err(conflict_error("提交的操作键不是本存储装配的操作键"));
        }
        let mut tx = self.database.begin_write().await.map_err(conflict_error)?;
        crate::journal::ensure_digest(
            &mut tx,
            self.owner,
            &store_key(write.key),
            &store_operations::RequestDigest::from_bytes(*write.digest.as_bytes()),
        )
        .await
        .map_err(conflict_error)?;
        let intent = store_intents::intent_in_scope(&mut tx, self.owner, write.intent_id)
            .await
            .map_err(conflict_error)?
            .ok_or(ConflictStoreError::IllegalIntentTransition {
                from: PutPhase::Prepared,
                to: PutPhase::Finalized,
            })?;
        if intent.phase != FsIntentPhase::Published {
            return Err(ConflictStoreError::IllegalIntentTransition {
                from: crate::commands::phase_text(intent.phase),
                to: PutPhase::Finalized,
            });
        }
        let source = store_entries::entry_in_scope(&mut tx, self.owner, write.source)
            .await
            .map_err(conflict_error)?
            .ok_or(ConflictStoreError::Rejected {
                code: "source_missing",
                detail: "冲突源条目不存在".to_owned(),
            })?;
        let parent = source.parent.ok_or(ConflictStoreError::Rejected {
            code: "source_root_entry",
            detail: "Root 根条目不能作为冲突源".to_owned(),
        })?;
        let name = write
            .path
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .ok_or(ConflictStoreError::Rejected {
                code: "invalid_conflict_path",
                detail: "冲突路径没有名称分量".to_owned(),
            })?;
        let revision = Revision::ZERO;
        let version = VersionId::from_bytes(random_bytes());
        let tx_id = TxId::from_bytes(random_bytes());
        let result = CommittedConflict {
            op: self.key.op,
            source: write.source,
            sibling: write.sibling,
            path: write.path.clone(),
            content: write.content,
            revision,
            seq: vaultsyncd_domain::version::RootSeq::ZERO,
        };
        let events = serde_json::json!([{
            "kind": "create",
            "entry_id": write.sibling.to_hex(),
            "path": write.path,
            "revision": revision.to_string(),
            "version_id":version.to_string(),
            "hash": write.content.digest.to_hex(),
            "size": write.content.bytes.to_string(),
            "origin": "server",
            "conflict_of": write.source.to_hex(),
        }])
        .to_string();
        let committed = store_operations::commit_operation(
            &mut tx,
            self.owner,
            &store_key(write.key),
            tx_id,
            &events,
            &crate::journal::with_echo(result_json(&result), self.local_generation),
            write.at,
        )
        .await
        .map_err(|error| match error {
            store_operations::OperationError::DigestMismatch { .. } => {
                ConflictStoreError::DuplicateOperation { op: write.key.op }
            }
            other => conflict_error(other),
        })?;
        let mut result = result;
        result.seq = committed.batch.seq;
        crate::restore_ops::persist_result_seq(
            &mut tx,
            write.key,
            &crate::journal::with_echo(result_json(&result), self.local_generation),
        )
        .await
        .map_err(conflict_error)?;
        store_entries::insert_entry(
            &mut tx,
            self.owner,
            &NewEntry {
                id: write.sibling,
                root: write.key.root,
                parent,
                name: name.to_owned(),
                name_key: name.to_owned(),
                kind: store_entries::EntryKind::File,
                revision,
                content_version: None,
            },
            write.at,
        )
        .await
        .map_err(|error| match error {
            store_entries::EntryError::DuplicateName { .. } => ConflictStoreError::PathOccupied {
                path: write.path.clone(),
            },
            other => conflict_error(other),
        })?;
        store_versions::record_version(
            &mut tx,
            self.owner,
            &NewVersion {
                id: version,
                root: write.key.root,
                entry: write.sibling,
                object_hash: *write.content.digest.as_bytes(),
                size: write.content.bytes,
                created_seq: committed.batch.seq,
                origin: VersionOrigin::Client,
                at: write.at,
            },
        )
        .await
        .map_err(conflict_error)?;
        let inserted = sqlx::query(
            "INSERT INTO conflicts \
             (root_id, device_id, op_id, source_entry_id, sibling_entry_id, path, reason, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'stale_base', ?7)",
        )
        .bind(write.key.root.as_bytes().as_slice())
        .bind(write.key.device.as_bytes().as_slice())
        .bind(write.key.op.as_bytes().as_slice())
        .bind(write.source.as_bytes().as_slice())
        .bind(write.sibling.as_bytes().as_slice())
        .bind(&write.path)
        .bind(write.at.to_db_integer())
        .execute(&mut *tx)
        .await;
        if let Err(error) = inserted {
            return Err(conflict_error(error));
        }
        store_intents::advance_intent_phase(
            &mut tx,
            self.owner,
            write.intent_id,
            FsIntentPhase::Finalized,
            write.at,
        )
        .await
        .map_err(conflict_error)?;
        tx.commit().await.map_err(conflict_error)?;
        Ok(ConflictRecord {
            intent_id: write.intent_id,
            phase: PutPhase::Finalized,
            conflict: result,
            tx: committed.batch.tx,
        })
    }
}

/// 冲突副本命令的输入。
#[derive(Clone, Debug)]
pub struct ConflictCommand {
    /// 客户端想覆盖的服务器 head 条目。
    pub entry: EntryId,
    /// 客户端计划时的条目 revision。
    pub base_revision: Revision,
    /// 迟到分支的内容（已 finalize 的对象）。
    pub content: ExpectedContent,
    /// 冲突条目在 Root 内的相对路径（含另名后的文件名）。
    pub target_path: String,
    /// 客户端本地代际（持久回显）。
    pub local_generation: Option<vaultsyncd_domain::version::Generation>,
}

/// 执行一次冲突副本提交（过期基线的迟到内容保留为独立条目）。
///
/// # Errors
///
/// 见 [`OperationFailure`]：基线未过期/内容相同（`RECONCILE_REQUIRED`）、目标占用、意图/发布/存储失败。
pub async fn execute_conflict(
    executor: &crate::executor::ExecutorContext,
    owner: UserId,
    key: OperationKey,
    digest: RequestDigest,
    command: &ConflictCommand,
    expected_epoch: vaultsyncd_domain::version::ServerEpoch,
    at: UtcTimestamp,
) -> Result<CommittedConflict, OperationFailure> {
    let handle: Arc<RootHandle> = executor.root_handle(owner, key.root).await?;
    let layout = executor.objects();
    let mut target = WorkTreeTarget::new(Arc::clone(&handle), layout);
    let mut store = SqlConflictStore::new(executor.database().clone(), owner, key)
        .with_local_generation(command.local_generation);
    let sibling = random_bytes();
    let mut log =
        SqlIntentLog::new(executor.database().clone(), owner).bound(crate::journal::Binding::new(
            store_key(key),
            "conflict",
            serde_json::json!({
                "source": command.entry.to_hex(), "sibling": EntryId::from_bytes(sibling).to_hex(),
                "local_generation": command.local_generation.map(|value| value.to_string()),
            }),
        ));
    let mut ids = RandomIds(sibling);
    let request = ConflictRequest {
        intent_id: vaultsyncd_domain::ids::IntentId::from_bytes(random_bytes()),
        op: key,
        digest,
        entry: command.entry,
        expected_epoch,
        expected_revision: command.base_revision,
        target_path: command.target_path.clone(),
        staging_path: format!("conflict-{}", key.op.to_hex()),
        content: command.content,
    };
    let mut gate = executor.commit_gate().lock().await;
    executor.ensure_no_unfinished(owner, key.root).await?;
    let outcome = commit_conflict(
        &mut gate,
        &mut store,
        &mut log,
        &mut target,
        &mut ids,
        at,
        &request,
    )
    .await;
    drop(gate);
    match outcome {
        Ok(committed) => Ok(committed),
        Err(failure) => {
            let mapped = crate::executor::map_put_failure(&failure);
            executor
                .reject_failure(owner, store_key(key), &mapped)
                .await;
            Err(mapped)
        }
    }
}

/// 冲突条目身份与另名用到的随机源。
struct RandomIds([u8; 16]);

impl vaultsyncd_domain::ids::IdSource for RandomIds {
    fn next_id_bytes(&mut self) -> [u8; 16] {
        self.0
    }
}
