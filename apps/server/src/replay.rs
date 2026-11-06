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
        let mut attempted = std::collections::HashSet::new();
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
                // 一次整组恢复挂着多个意图，只按操作补完一次；失败也不在同一轮对其余意图重试。
                if binding.kind == "trash_restore" && !attempted.insert(binding.op) {
                    return Ok(());
                }
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
            let mut store = SqlNamespaceStore::new(db.clone(), owner, binding.key(), digest)
                .with_local_generation(crate::journal::echo_of(&binding.context)?);
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
            // 重启补完同样不能删掉尚未采纳的内容：先按冻结清单核对现场，通不过就保持停写。
            let before = vaultsyncd_server_core::commit::delete::DeleteSnapshot::from_json(
                &intent.expected_before,
            )
            .map_err(text)?;
            crate::commands::verify_delete_scope(
                db,
                owner,
                &handle,
                ObjectLayout::new(state.join("objects")),
                binding.root,
                before.entry,
            )
            .await?;
            let mut log = SqlFsIntentLog::new(db.clone(), owner).only(intent.intent_id);
            let mut ns = WorkTreeNamespace::new(handle);
            let mut store = SqlDeleteStore::new(db.clone(), owner, binding.key(), digest)
                .with_local_generation(crate::journal::echo_of(&binding.context)?);
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
        "trash_restore" => {
            replay_trash_restore(db, state, owner, handle, &binding, digest, at).await?;
        }
        "put" | "version_restore" | "conflict" => {
            if intent.kind != FsIntentKind::PutContent {
                return Err("内容上下文与意图种类不符".to_owned());
            }
            let put = crate::executor::put_intent_of(intent).map_err(text)?;
            if binding.context["create_file"] == true {
                crate::create_file::validate(db, owner, &binding, digest, &put)
                    .await
                    .map_err(text)?;
            }
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
                "put" if binding.context["create_file"] == true => {
                    crate::create_file::finish(
                        db,
                        owner,
                        &binding,
                        digest,
                        put.intent_id,
                        content,
                        at,
                    )
                    .await
                    .map_err(text)?;
                }
                "put" => {
                    let mut store = SqlPutStore {
                        database: db.clone(),
                        owner,
                        key: binding.key(),
                        digest,
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
                    )
                    .with_local_generation(crate::journal::echo_of(&binding.context)?);
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
                        crate::conflict_ops::SqlConflictStore::new(db.clone(), owner, key)
                            .with_local_generation(crate::journal::echo_of(&binding.context)?);
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

/// 目录里允许出现的“不属于计划”的名字：发布端口遗留的暂存文件。
fn is_staging_leftover(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy()
        .starts_with(vaultsyncd_platform_linux_server::files::publish::STAGING_PREFIX)
}

/// 按持久化的整组计划补完回收站恢复：逐条目核对现场，目录缺则补建、已有则只接受“内容都在计划内”，
/// 文件按各自意图的前后像重做或确认，最后用同一个元数据事务收尾。无法证明属于本计划的现场
/// （外部内容、第三种内容、缺意图）一律报错保持停写，不覆盖也不合并。
async fn replay_trash_restore(
    db: &Database,
    state: &Path,
    owner: UserId,
    handle: Arc<RootHandle>,
    binding: &Binding,
    digest: operations::RequestDigest,
    at: UtcTimestamp,
) -> Result<(), String> {
    use vaultsyncd_server_core::ports::{
        namespace::{NamespacePort, PathState},
        restore_store::TrashRestoreStore,
        target::TargetObservation,
    };
    let key = OperationKey {
        root: binding.root,
        device: binding.device,
        op: binding.op,
    };
    let core_digest = RequestDigest::from_bytes(*digest.as_bytes());
    let plan = crate::restore_ops::plan_from_context(&binding.context, key, core_digest, at)?;
    let mut namespace = WorkTreeNamespace::new(Arc::clone(&handle));
    let mut target = WorkTreeTarget::new(
        Arc::clone(&handle),
        ObjectLayout::new(state.join("objects")),
    );
    let mut log = SqlIntentLog::new(db.clone(), owner).bound(Binding::linked(binding.key()));
    let mut store =
        crate::restore_ops::SqlTrashRestoreStore::new(db.clone(), owner, key, core_digest)
            .with_local_generation(crate::journal::echo_of(&binding.context)?);
    let planned_paths: std::collections::HashSet<&str> = plan
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    let (mut files, mut directories) = (0usize, 0usize);
    for entry in &plan.entries {
        match entry.kind {
            vaultsyncd_server_core::ports::put_store::EntryKind::Directory => {
                let intent = plan.directory_intents[directories];
                directories += 1;
                let mut conn = db.pool().acquire().await.map_err(text)?;
                let record =
                    vaultsyncd_server_store::intents::intent_in_scope(&mut conn, owner, intent)
                        .await
                        .map_err(text)?
                        .ok_or_else(|| format!("目录 {} 缺少计划内的意图", entry.path))?;
                drop(conn);
                if record.kind != FsIntentKind::Create || record.target_path != entry.path {
                    return Err(format!("目录意图与计划不一致：{}", entry.path));
                }
                match namespace.inspect(&entry.path).map_err(text)? {
                    PathState::Absent => {
                        namespace.create_directory(&entry.path).map_err(text)?;
                    }
                    PathState::Directory { .. } => {
                        for child in handle.list_dir(Path::new(&entry.path)).map_err(text)? {
                            let name = child.name.to_string_lossy();
                            let child_path = format!("{}/{name}", entry.path);
                            if !is_staging_leftover(&child.name)
                                && !planned_paths.contains(child_path.as_str())
                            {
                                return Err(format!(
                                    "目录 {} 里有不属于恢复计划的内容 {name}，不合并",
                                    entry.path
                                ));
                            }
                        }
                    }
                    other => return Err(format!("目录 {} 的现场是 {other}", entry.path)),
                }
            }
            vaultsyncd_server_core::ports::put_store::EntryKind::File => {
                let intent = plan.intents[files];
                files += 1;
                let version = entry
                    .content_version
                    .ok_or_else(|| format!("文件 {} 的计划没有内容版本", entry.path))?;
                let content = store
                    .content_facts(binding.root, version)
                    .await
                    .map_err(text)?
                    .ok_or_else(|| format!("版本 {version} 的内容已不可读"))?;
                match log.load_put(intent).await.map_err(text)? {
                    Some(put) => {
                        if put.target_path != entry.path
                            || put.after.digest != content.digest
                            || put.after.size != content.bytes
                        {
                            return Err(format!("文件意图与计划不一致：{}", entry.path));
                        }
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
                        if put.phase == PutPhase::Prepared {
                            log.advance_put(intent, PutPhase::Published, at)
                                .await
                                .map_err(text)?;
                        }
                    }
                    None => {
                        // 意图尚未创建：必须证明现场仍是空缺，才能在计划身份下继续发布。
                        let observed = target.observe(&entry.path).map_err(text)?;
                        if !matches!(observed, TargetObservation::Absent) {
                            return Err(format!("文件 {} 没有意图却已有内容，不覆盖", entry.path));
                        }
                        vaultsyncd_server_core::commit::intent::publish_put(
                            &mut log,
                            &mut target,
                            at,
                            &vaultsyncd_server_core::commit::intent::NewPutIntent {
                                intent_id: intent,
                                root: binding.root,
                                target_path: entry.path.clone(),
                                staging_path: vaultsyncd_server_core::history::trash::staging_name(
                                    intent,
                                ),
                                before:
                                    vaultsyncd_server_core::commit::intent::TargetBefore::Absent,
                                after: vaultsyncd_server_core::commit::intent::TargetAfter {
                                    digest: content.digest,
                                    size: content.bytes,
                                },
                            },
                        )
                        .await
                        .map_err(text)?;
                    }
                }
            }
        }
    }
    store.apply_trash_restore(&plan).await.map_err(text)?;
    Ok(())
}
