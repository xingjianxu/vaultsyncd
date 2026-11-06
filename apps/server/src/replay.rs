//! 基于持久关联与现场证据的启动补完；失败保留意图，最终准入仍由 recovery::assess 决定。
use crate::{
    commands::{SqlDeleteStore, SqlFsIntentLog, SqlNamespaceStore, WorkTreeNamespace},
    executor::{SqlIntentLog, SqlPutStore, WorkTreeTarget},
    journal::Binding,
};
use std::{path::Path, sync::Arc};
use vaultsyncd_domain::{
    ids::{EntryId, UserId},
    version::{Revision, UtcTimestamp},
};
use vaultsyncd_platform_linux_server::files::{ResolveMode, RootHandle};
use vaultsyncd_server_core::{
    commit::intent::PutPhase,
    objects::{ExpectedContent, ObjectLayout},
    ports::{
        conflict_store::{ConflictCommitStore, ConflictWrite, OperationKey, RequestDigest},
        intent_log::IntentLog,
        put_store::{PutCommitStore, PutWrite},
        restore_store::{RestoreStore, VersionRestoreWrite},
        target::{PublishRequest, TargetPort},
    },
    recovery::{RecoveryDecision, classify_put},
};
use vaultsyncd_server_store::{
    db::Database,
    intents::{FsIntent, FsIntentKind},
    operations,
};

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn fact<T: std::str::FromStr>(binding: &Binding, field: &str) -> Result<T, String> {
    binding
        .context
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("恢复上下文缺少 {field}"))?
        .parse()
        .map_err(|_| format!("恢复字段 {field} 不合法"))
}

/// 启动监听与观察器前逐条补完；任何无法证明的条目保持未完成并输出原因。
pub async fn registered(db: &Database, state_dir: &Path) -> Result<(), String> {
    let mut conn = db.pool().acquire().await.map_err(text)?;
    let roots = vaultsyncd_server_store::roots::all_roots(&mut conn)
        .await
        .map_err(text)?;
    drop(conn);
    for root in roots {
        let mut conn = db.pool().acquire().await.map_err(text)?;
        let intents =
            vaultsyncd_server_store::intents::unfinished_intents(&mut conn, root.owner, root.id)
                .await
                .map_err(text)?;
        drop(conn);
        if intents.is_empty() {
            continue;
        }
        let handle = match RootHandle::open(Path::new(&root.absolute_path), ResolveMode::Auto) {
            Ok(handle) => Arc::new(handle),
            Err(error) => {
                tracing::warn!(root=%root.id, error=%error, "恢复打不开 Root，保留停写");
                continue;
            }
        };
        for intent in intents {
            let result = async {
                let binding = crate::journal::for_intent(db, root.owner, intent.intent_id)
                    .await
                    .map_err(text)?
                    .ok_or_else(|| "旧意图未关联完整操作键，不能猜测".to_owned())?;
                let mut conn = db.pool().acquire().await.map_err(text)?;
                let operation =
                    operations::operation_in_scope(&mut conn, root.owner, &binding.key())
                        .await
                        .map_err(text)?
                        .ok_or_else(|| "恢复上下文没有对应操作".to_owned())?;
                if operation.state != operations::OperationState::Pending {
                    return Err("未完成意图的操作不是 pending".to_owned());
                }
                drop(conn);
                replay(
                    db,
                    state_dir,
                    root.owner,
                    Arc::clone(&handle),
                    intent,
                    binding,
                    operation.digest,
                    crate::uploads::server_now(),
                )
                .await
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(root=%root.id, error, "意图恢复未完成，保留证据和停写");
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn replay(
    db: &Database,
    state: &Path,
    owner: UserId,
    handle: Arc<RootHandle>,
    intent: FsIntent,
    binding: Binding,
    digest: operations::RequestDigest,
    at: UtcTimestamp,
) -> Result<(), String> {
    match binding.kind.as_str() {
        "namespace" => {
            let mut log = SqlFsIntentLog::new(db.clone(), owner).only(intent.intent_id);
            let mut ns = WorkTreeNamespace::new(handle);
            let mut store = SqlNamespaceStore::new(db.clone(), owner, binding.key(), digest);
            let report = vaultsyncd_server_core::commit::namespace::recover_namespace_root(
                &mut log,
                &mut ns,
                &mut store,
                at,
                binding.root,
            )
            .await
            .map_err(text)?;
            if report.pauses_writes() || report.needs_reconciliation() {
                return Err(format!("命名空间恢复需协调：{report:?}"));
            }
        }
        "delete" => {
            let mut log = SqlFsIntentLog::new(db.clone(), owner).only(intent.intent_id);
            let mut ns = WorkTreeNamespace::new(handle);
            let mut store = SqlDeleteStore::new(db.clone(), owner, binding.key(), digest);
            let report = vaultsyncd_server_core::commit::delete::recover_delete_root(
                &mut log,
                &mut ns,
                &mut store,
                at,
                binding.root,
            )
            .await
            .map_err(text)?;
            if report.pauses_writes() || report.needs_reconciliation() {
                return Err(format!("删除恢复需协调：{report:?}"));
            }
        }
        "put" | "version_restore" | "conflict" => {
            if intent.kind != FsIntentKind::PutContent {
                return Err("内容上下文与意图种类不符".to_owned());
            }
            let put = crate::executor::put_intent_of(intent).map_err(text)?;
            let mut target = WorkTreeTarget::new(handle, ObjectLayout::new(state.join("objects")));
            match classify_put(&put, target.observe(&put.target_path)) {
                RecoveryDecision::Redo => {
                    let published = target
                        .publish(&PublishRequest {
                            target_path: put.target_path.clone(),
                            staging_path: put.staging_path.clone(),
                            digest: put.after.digest,
                            size: put.after.size,
                        })
                        .map_err(text)?;
                    if !put.after.satisfied_by_published(&published) {
                        return Err("恢复发布后内容不符".to_owned());
                    }
                }
                RecoveryDecision::Complete => {}
                decision => return Err(format!("现场不满足恢复前提：{decision}")),
            }
            let mut log = SqlIntentLog::new(db.clone(), owner);
            if put.phase == PutPhase::Prepared {
                log.advance_put(put.intent_id, PutPhase::Published, at)
                    .await
                    .map_err(text)?;
            }
            let key = OperationKey {
                root: binding.root,
                device: binding.device,
                op: binding.op,
            };
            let core_digest = RequestDigest::from_bytes(*digest.as_bytes());
            let content = ExpectedContent::new(put.after.digest, put.after.size);
            match binding.kind.as_str() {
                "put" => {
                    let mut store = SqlPutStore {
                        database: db.clone(),
                        owner,
                        key: binding.key(),
                        local_generation: binding
                            .context
                            .get("local_generation")
                            .and_then(serde_json::Value::as_str)
                            .map(str::parse)
                            .transpose()
                            .map_err(text)?,
                    };
                    store
                        .apply_put(&PutWrite {
                            intent_id: put.intent_id,
                            root: binding.root,
                            entry: fact(&binding, "entry")?,
                            expected_revision: fact::<Revision>(&binding, "revision")?,
                            content,
                            target_path: put.target_path,
                            at,
                        })
                        .await
                        .map_err(text)?;
                }
                "version_restore" => {
                    let mut store = crate::restore_ops::SqlRestoreStore::new(
                        db.clone(),
                        owner,
                        key,
                        core_digest,
                    );
                    store
                        .apply_version_restore(&VersionRestoreWrite {
                            intent_id: put.intent_id,
                            key,
                            digest: core_digest,
                            entry: fact(&binding, "entry")?,
                            expected_revision: fact(&binding, "revision")?,
                            restored_from: fact(&binding, "restored_from")?,
                            content,
                            path: put.target_path,
                            at,
                        })
                        .await
                        .map_err(text)?;
                }
                "conflict" => {
                    let mut store =
                        crate::conflict_ops::SqlConflictStore::new(db.clone(), owner, key);
                    store
                        .apply_conflict(&ConflictWrite {
                            intent_id: put.intent_id,
                            key,
                            digest: core_digest,
                            source: fact::<EntryId>(&binding, "source")?,
                            sibling: fact(&binding, "sibling")?,
                            path: put.target_path,
                            content,
                            at,
                        })
                        .await
                        .map_err(text)?;
                }
                _ => unreachable!("先前已限定内容类型"),
            }
        }
        _ => return Err("未知或尚未支持的恢复上下文".to_owned()),
    }
    Ok(())
}
