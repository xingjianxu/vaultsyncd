//! Existing namespace commands consumed by the headless synchronizer, with durable physical fences.
#[path = "namespace_recovery.rs"]
mod recovery;
use crate::{
    local::error,
    work::{Worker, baseline, load_spec, metadata, new_op, paths},
};
pub(crate) use recovery::recover;
use std::collections::HashMap;
use vaultsyncd_agent_core::{
    executor::ActionOutcome,
    ports::{PortError, Ports, state::QueuedOperation},
};
use vaultsyncd_client_store::{
    bindings,
    entries::{self, BaselineEntry, EntryKind, LocalEntry},
    queue::{self, NewPendingOp},
};
use vaultsyncd_domain::{
    ids::{EntryId, OpId},
    state::{Dirty, Residency},
    version::{Generation, Revision},
};
use vaultsyncd_http_client::{ApiRequest, AuthMode};
use vaultsyncd_platform_windows::files::{EntryMetadata, controlled::Evidence};
use vaultsyncd_protocol::generated::{
    ConflictPolicy, OperationKind, OperationRequest, OperationState, SnapshotEntry, SubtreeToken,
};

fn physical(meta: &EntryMetadata) -> Vec<u8> {
    let mut id = meta.identity.volume.to_le_bytes().to_vec();
    id.extend(meta.identity.file);
    id
}
async fn stage(
    worker: &Worker,
    op: OpId,
    value: &serde_json::Value,
) -> Result<serde_json::Value, PortError> {
    let local = &worker.importer.local;
    let mut tx = local.db.begin_write().await.map_err(error)?;
    let old: Option<String> = sqlx::query_scalar("SELECT result FROM agent_actions WHERE op_id=?")
        .bind(op.as_bytes().as_slice())
        .fetch_one(&mut *tx)
        .await
        .map_err(error)?;
    if let Some(old) = old {
        let old = serde_json::from_str(&old).map_err(error)?;
        return Ok(old);
    }
    sqlx::query("UPDATE agent_actions SET result=? WHERE op_id=? AND result IS NULL")
        .bind(value.to_string())
        .bind(op.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .map_err(error)?;
    tx.commit().await.map_err(error)?;
    Ok(value.clone())
}
pub(crate) async fn discover(
    worker: &Worker,
    scan: &HashMap<String, EntryMetadata>,
) -> Result<bool, PortError> {
    let local = &worker.importer.local;
    let rules = crate::exclusions::Rules::load(&local.db, local.binding).await?;
    let controls = vaultsyncd_agent_core::bindings::control::BindingControls::new(local.db.clone());
    let rows = metadata(&worker.importer).await?;
    let locations = paths(&rows)?;
    let by_path: HashMap<_, _> = locations
        .iter()
        .map(|(id, p)| (p.clone(), id.clone()))
        .collect();
    let mut conn = local.db.pool().acquire().await.map_err(error)?;
    let binding = bindings::binding(&mut conn, local.binding)
        .await
        .map_err(error)?
        .ok_or_else(|| error("missing binding"))?;
    drop(conn);
    let mut order: Vec<_> = rows.values().filter(|r| r.parent_id.is_some()).collect();
    order.sort_by_key(|r| {
        (
            r.kind != vaultsyncd_protocol::generated::EntryKind::Directory,
            locations[&r.entry_id].matches('/').count(),
        )
    });
    'known: for row in order {
        let from = &locations[&row.entry_id];
        let known_entry = EntryId::from_hex(&row.entry_id).map_err(error)?;
        if controls
            .execution_gate(local.binding, Some(known_entry))
            .await?
            != vaultsyncd_agent_core::bindings::control::ExecutionGate::Ready
        {
            continue;
        }
        if scan.contains_key(from) {
            continue;
        }
        let entry = EntryId::from_hex(&row.entry_id).map_err(error)?;
        let mut conn = local.db.pool().acquire().await.map_err(error)?;
        let state = entries::local_entry(&mut conn, local.binding, entry)
            .await
            .map_err(error)?;
        drop(conn);
        let Some(state) = state else {
            return Err(error("unknown missing-object identity"));
        };
        let Some(old_identity) = state.file_identity else {
            return Err(error("missing durable physical identity"));
        };
        let moved = scan.iter().find(|(_, m)| physical(m) == old_identity);
        let (kind, path, parent, name, token) = if let Some((path, _)) = moved {
            let parts: Vec<_> = path.rsplitn(2, '/').collect();
            let parent_path = parts.get(1).copied().unwrap_or("");
            let Some(parent) = by_path.get(parent_path) else {
                continue;
            };
            (
                OperationKind::Move,
                path.clone(),
                Some(parent.clone()),
                Some(parts[0].into()),
                None,
            )
        } else {
            let token = if row.kind == vaultsyncd_protocol::generated::EntryKind::Directory {
                if scan.keys().any(|p| p.starts_with(&format!("{from}/"))) {
                    continue;
                }
                for (child_id, child_path) in &locations {
                    if child_path.starts_with(&format!("{from}/")) {
                        let mut conn = local.db.pool().acquire().await.map_err(error)?;
                        let child = entries::local_entry(
                            &mut conn,
                            local.binding,
                            EntryId::from_hex(child_id).map_err(error)?,
                        )
                        .await
                        .map_err(error)?;
                        drop(conn);
                        if child
                            .and_then(|s| s.file_identity)
                            .is_some_and(|id| scan.values().any(|m| physical(m) == id))
                        {
                            continue 'known;
                        }
                    }
                }
                Some(directory_token(worker, row, &rows, &locations).await?)
            } else {
                None
            };
            (OperationKind::Delete, from.clone(), None, None, token)
        };
        if !rules.includes(
            &path,
            row.kind == vaultsyncd_protocol::generated::EntryKind::Directory,
        )? {
            continue;
        }
        let request = OperationRequest {
            op_id: new_op().to_string(),
            device_id: binding.device.to_string(),
            server_epoch: binding.server_epoch.to_string(),
            kind,
            entry_id: Some(row.entry_id.clone()),
            base_revision: Some(row.revision.clone()),
            base_version_id: None,
            expected_absent: None,
            parent_id: parent,
            name,
            subtree_token: token,
            upload_id: None,
            version_id: None,
            local_generation: Some(state.generation.to_string()),
            conflict_policy: Some(ConflictPolicy::PreserveBoth),
        };
        enqueue(
            worker,
            request,
            &path,
            from,
            Some(entry),
            Some(row.clone()),
            old_identity,
        )
        .await?;
        return Ok(true);
    }
    let mut directories: Vec<_> = scan.iter().filter(|(_, m)| m.directory).collect();
    directories.sort_by_key(|(p, _)| p.matches('/').count());
    for (path, meta) in directories {
        if !rules.includes(path, true)? {
            continue;
        }
        if by_path.contains_key(path) {
            continue;
        }
        let parts: Vec<_> = path.rsplitn(2, '/').collect();
        let p = parts.get(1).copied().unwrap_or("");
        let Some(parent) = by_path.get(p) else {
            continue;
        };
        let request = OperationRequest {
            op_id: new_op().to_string(),
            device_id: binding.device.to_string(),
            server_epoch: binding.server_epoch.to_string(),
            kind: OperationKind::Create,
            entry_id: None,
            base_revision: None,
            base_version_id: None,
            expected_absent: Some(true),
            parent_id: Some(parent.clone()),
            name: Some(parts[0].into()),
            subtree_token: None,
            upload_id: None,
            version_id: None,
            local_generation: Some("0".into()),
            conflict_policy: Some(ConflictPolicy::PreserveBoth),
        };
        enqueue(worker, request, path, path, None, None, physical(meta)).await?;
        return Ok(true);
    }
    Ok(false)
}
async fn enqueue(
    worker: &Worker,
    request: OperationRequest,
    path: &str,
    from: &str,
    entry: Option<EntryId>,
    before: Option<SnapshotEntry>,
    identity: Vec<u8>,
) -> Result<(), PortError> {
    let local = &worker.importer.local;
    let pending:Vec<String>=sqlx::query_scalar("SELECT specification FROM agent_actions a JOIN pending_ops p ON p.op_id=a.op_id WHERE a.binding_id=?").bind(local.binding.as_bytes().as_slice()).fetch_all(local.db.pool()).await.map_err(error)?;
    for text in pending {
        let v: serde_json::Value = serde_json::from_str(&text).map_err(error)?;
        if v["kind"] == "namespace" && v["path"] == path {
            return Ok(());
        }
    }
    let op = OpId::from_hex(&request.op_id).map_err(error)?;
    let spec = serde_json::json!({"kind":"namespace","request":request,"path":path,"from":from,"before":before,"physical":identity});
    let mut tx = local.db.begin_write().await.map_err(error)?;
    queue::enqueue(
        &mut tx,
        &NewPendingOp {
            op,
            binding: local.binding,
            kind: if request.kind == OperationKind::Delete {
                "delete_entry"
            } else {
                "upload_move"
            }
            .into(),
            entry,
            baseline_revision: request
                .base_revision
                .as_deref()
                .map(Revision::from_decimal)
                .transpose()
                .map_err(error)?,
            baseline_version: None,
            depends_on: None,
            pinned_version: None,
        },
    )
    .await
    .map_err(error)?;
    sqlx::query("INSERT INTO agent_actions(op_id,binding_id,specification) VALUES(?,?,?)")
        .bind(op.as_bytes().as_slice())
        .bind(local.binding.as_bytes().as_slice())
        .bind(spec.to_string())
        .execute(&mut *tx)
        .await
        .map_err(error)?;
    tx.commit().await.map_err(error)?;
    Ok(())
}
async fn directory_token(
    worker: &Worker,
    row: &SnapshotEntry,
    known: &HashMap<String, SnapshotEntry>,
    locations: &HashMap<String, String>,
) -> Result<String, PortError> {
    let remote = &worker.importer.remote;
    let prefix = &locations[&row.entry_id];
    let compare = |current: Vec<SnapshotEntry>| -> Result<(), PortError> {
        let map: HashMap<_, _> = current
            .into_iter()
            .map(|e| (e.entry_id.clone(), e))
            .collect();
        let paths = paths(&map)?;
        let a: HashMap<_, _> = known
            .iter()
            .filter(|(id, _)| {
                locations[*id] == *prefix || locations[*id].starts_with(&format!("{prefix}/"))
            })
            .map(|(id, r)| {
                (
                    id.clone(),
                    (
                        r.revision.clone(),
                        r.parent_id.clone(),
                        r.name.clone(),
                        r.version_id.clone(),
                    ),
                )
            })
            .collect();
        let b: HashMap<_, _> = map
            .iter()
            .filter(|(id, _)| {
                paths[*id] == *prefix || paths[*id].starts_with(&format!("{prefix}/"))
            })
            .map(|(id, r)| {
                (
                    id.clone(),
                    (
                        r.revision.clone(),
                        r.parent_id.clone(),
                        r.name.clone(),
                        r.version_id.clone(),
                    ),
                )
            })
            .collect();
        if a != b {
            return Err(error("server subtree changed since local baseline"));
        }
        Ok(())
    };
    compare(remote_snapshot(remote).await?)?;
    let token: SubtreeToken = remote
        .client
        .send_json(
            ApiRequest::get(format!(
                "/v1/vaults/{}/entries/{}/subtree",
                remote.vault, row.entry_id
            ))
            .with_auth(AuthMode::Bearer),
        )
        .await
        .map_err(vaultsyncd_http_client::transfer_error)?;
    if token.vault_id != remote.vault.to_string() || token.entry_id != row.entry_id {
        return Err(error("subtree token identity mismatch"));
    }
    compare(remote_snapshot(remote).await?)?;
    Ok(token.token)
}
pub(crate) async fn remote_snapshot(
    remote: &crate::remote::Remote,
) -> Result<Vec<SnapshotEntry>, PortError> {
    let (s, _) = vaultsyncd_http_client::sync::snapshot(&remote.client, &remote.vault.to_string())
        .await
        .map_err(vaultsyncd_http_client::transfer_error)?;
    let mut rows = Vec::new();
    let mut cursor = None;
    loop {
        let page = vaultsyncd_http_client::sync::snapshot_page(
            &remote.client,
            &remote.vault.to_string(),
            &s.snapshot_id,
            cursor.as_deref(),
        )
        .await
        .map_err(vaultsyncd_http_client::transfer_error)?;
        rows.extend(page.entries);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(rows);
        }
    }
}
pub(crate) async fn outgoing(
    worker: &Worker,
    ports: &Ports,
    op: &QueuedOperation,
) -> Result<ActionOutcome, PortError> {
    let local = &worker.importer.local;
    let spec = load_spec(local, op.op).await?;
    let request: OperationRequest =
        serde_json::from_value(spec["request"].clone()).map_err(error)?;
    let result = match ports.remote.submit(&request).await {
        Ok(r) => r,
        Err(PortError::Unavailable { .. }) => ports.remote.operation(local.binding, op.op).await?,
        Err(e) => return Err(e),
    };
    if result.state == OperationState::Pending {
        return Ok(ActionOutcome::Pending {
            poll_after: Some(std::time::Duration::from_secs(1)),
        });
    }
    if result.state != OperationState::Committed || result.op_id != op.op.to_string() {
        return Err(error("namespace operation was not committed"));
    }
    let path = spec["path"]
        .as_str()
        .ok_or_else(|| error("missing namespace path"))?;
    let entry = EntryId::from_hex(
        result
            .entry_id
            .as_deref()
            .ok_or_else(|| error("missing namespace ACK identity"))?,
    )
    .map_err(error)?;
    let mut after = if request.kind == OperationKind::Create {
        BaselineEntry {
            entry,
            parent: request
                .parent_id
                .as_deref()
                .map(EntryId::from_hex)
                .transpose()
                .map_err(error)?,
            name: request
                .name
                .clone()
                .ok_or_else(|| error("missing directory name"))?,
            kind: EntryKind::Directory,
            revision: Revision::ZERO,
            content_version: None,
            deleted: false,
        }
    } else {
        baseline(&serde_json::from_value::<SnapshotEntry>(spec["before"].clone()).map_err(error)?)?
    };
    after.revision = Revision::from_decimal(
        result
            .revision
            .as_deref()
            .ok_or_else(|| error("missing namespace revision"))?,
    )
    .map_err(error)?;
    if request.kind == OperationKind::Move {
        after.parent = request
            .parent_id
            .as_deref()
            .map(EntryId::from_hex)
            .transpose()
            .map_err(error)?;
        after.name = request
            .name
            .clone()
            .ok_or_else(|| error("missing move name"))?;
    }
    if request.kind == OperationKind::Delete {
        if after.kind == EntryKind::Directory {
            // Its whole server transaction carries descendant tombstones. Retire the parent
            // through the received batch after children, retaining path anchors until then.
            let mut tx = local.db.begin_write().await.map_err(error)?;
            sqlx::query("UPDATE agent_actions SET result=? WHERE op_id=?")
                .bind(serde_json::to_string(&result).map_err(error)?)
                .bind(op.op.as_bytes().as_slice())
                .execute(&mut *tx)
                .await
                .map_err(error)?;
            tx.commit().await.map_err(error)?;
            return Ok(ActionOutcome::Completed);
        }
        after.deleted = true;
    }
    let observed = local.vault.inspect(path);
    let missing = observed
        .as_ref()
        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
    let expected: Vec<u8> = serde_json::from_value(spec["physical"].clone()).map_err(error)?;
    let mut local_after = None;
    if request.kind != OperationKind::Delete {
        let meta = observed.map_err(error)?;
        if physical(&meta) != expected {
            return Err(error("local namespace changed during server commit"));
        }
        if meta.directory {
            local_after = Some(LocalEntry {
                entry,
                generation: Generation::ZERO,
                dirty: Dirty::Clean,
                residency: Residency::Materialized,
                fingerprint: None,
                file_identity: Some(physical(&meta)),
            });
        } else if meta.cloud.as_ref().is_some_and(|c| c.on_disk < meta.size) {
            let mut conn = local.db.pool().acquire().await.map_err(error)?;
            let mut old = entries::local_entry(&mut conn, local.binding, entry)
                .await
                .map_err(error)?
                .ok_or_else(|| error("missing partial local state"))?;
            if meta.cloud.as_ref().is_some_and(|c| c.modified > 0) {
                old.dirty = Dirty::Dirty;
            }
            local_after = Some(old);
        } else {
            let current = local
                .observe_snapshot(path)
                .await?
                .ok_or_else(|| error("moved object missing"))?;
            let e = current.evidence();
            let old = spec["before"]["hash"].as_str();
            local_after = Some(LocalEntry {
                entry,
                generation: Generation::new(e.generation).map_err(error)?,
                dirty: if old == Some(&vaultsyncd_transfer::hash::to_hex(&e.digest)) {
                    Dirty::Clean
                } else {
                    Dirty::Dirty
                },
                residency: Residency::Materialized,
                fingerprint: Some(e.digest.to_vec()),
                file_identity: Some(e.identity()),
            });
        }
    }
    let mut tx = local.db.begin_write().await.map_err(error)?;
    entries::put_baseline(&mut tx, local.binding, &after)
        .await
        .map_err(error)?;
    if let Some(row) = local_after {
        entries::put_local(&mut tx, local.binding, &row)
            .await
            .map_err(error)?;
    } else if missing {
        entries::remove_local(&mut tx, local.binding, entry)
            .await
            .map_err(error)?;
    }
    sqlx::query("UPDATE agent_actions SET result=? WHERE op_id=?")
        .bind(serde_json::to_string(&result).map_err(error)?)
        .bind(op.op.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .map_err(error)?;
    tx.commit().await.map_err(error)?;
    Ok(ActionOutcome::Completed)
}
/// Apply remote namespace first; return true when the entire event is a namespace-only result.
pub(crate) async fn incoming(
    worker: &Worker,
    op: OpId,
    spec: &serde_json::Value,
    row: &SnapshotEntry,
) -> Result<bool, PortError> {
    let local = &worker.importer.local;
    let id = EntryId::from_hex(&row.entry_id).map_err(error)?;
    let target = spec["path"]
        .as_str()
        .ok_or_else(|| error("missing event target"))?;
    let mut conn = local.db.pool().acquire().await.map_err(error)?;
    let before = entries::baseline_entry(&mut conn, local.binding, id)
        .await
        .map_err(error)?;
    let state = entries::local_entry(&mut conn, local.binding, id)
        .await
        .map_err(error)?;
    drop(conn);
    let Some(before) = before else {
        return Ok(false);
    };
    let rows = metadata(&worker.importer).await?;
    let locations = paths(&rows)?;
    let from = locations
        .get(&row.entry_id)
        .cloned()
        .unwrap_or_else(|| target.into());
    let deleted = spec["deleted"] == true;
    if !deleted && from == target {
        return Ok(false);
    }
    if deleted && before.deleted {
        return Ok(true);
    }
    if deleted
        && local
            .vault
            .inspect(&from)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        let mut after = baseline(row)?;
        after.deleted = true;
        let mut tx = local.db.begin_write().await.map_err(error)?;
        entries::put_baseline(&mut tx, local.binding, &after)
            .await
            .map_err(error)?;
        entries::remove_local(&mut tx, local.binding, id)
            .await
            .map_err(error)?;
        tx.commit().await.map_err(error)?;
        return Ok(true);
    }
    let old = state.ok_or_else(|| error("unknown namespace local state"))?;
    let expected = old
        .file_identity
        .clone()
        .ok_or_else(|| error("unknown namespace physical identity"))?;
    let mut after = baseline(row)?;
    after.deleted = deleted;
    if deleted {
        match local.vault.inspect(&from) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(error(e)),
            Ok(meta) if meta.directory && physical(&meta) != expected => {}
            Ok(meta) if meta.directory => {
                match local.vault.remove_empty_directory(&from, meta.identity) {
                    Ok(()) => {}
                    Err(e) if e.raw_os_error() == Some(145) => {
                        if rows
                            .iter()
                            .any(|(id, _)| locations[id].starts_with(&format!("{from}/")))
                        {
                            return Err(PortError::rejected(
                                "SUBTREE_BUSY",
                                "children must retire first",
                            ));
                        }
                        // Only unconfirmed descendants remain: retain the directory for a new-identity reconciliation.
                    }
                    Err(e) => return Err(error(e)),
                }
            }
            Ok(meta) => {
                if physical(&meta) == expected
                    && old.dirty == Dirty::Clean
                    && meta.cloud.as_ref().is_some_and(|c| {
                        (c.on_disk < meta.size || meta.size == 0) && c.modified == 0
                    })
                {
                    stage(
                        worker,
                        op,
                        &serde_json::json!({"from":from,"physical":expected}),
                    )
                    .await?;
                    worker.importer.callbacks.cancel_entry(local.binding, id);
                    worker
                        .importer
                        .adapter
                        .lock()
                        .map_err(error)?
                        .remove_unmodified_placeholder(&from, &meta)
                        .map_err(error)?;
                    let mut tx = local.db.begin_write().await.map_err(error)?;
                    entries::put_baseline(&mut tx, local.binding, &after)
                        .await
                        .map_err(error)?;
                    entries::remove_local(&mut tx, local.binding, id)
                        .await
                        .map_err(error)?;
                    tx.commit().await.map_err(error)?;
                    return Ok(true);
                }
                let current = local
                    .observe_snapshot(&from)
                    .await?
                    .ok_or_else(|| error("deleted object vanished"))?;
                let e = current.evidence();
                let prior = row
                    .hash
                    .as_deref()
                    .or_else(|| rows.get(&row.entry_id).and_then(|r| r.hash.as_deref()));
                if physical(&meta) == expected
                    && old.dirty == Dirty::Clean
                    && prior == Some(&vaultsyncd_transfer::hash::to_hex(&e.digest))
                {
                    let recorded =
                        stage(worker, op, &serde_json::json!({"before":e,"from":from})).await?;
                    let expected: Evidence =
                        serde_json::from_value(recorded["before"].clone()).map_err(error)?;
                    local
                        .native
                        .quarantine(&op.to_string(), &from, &expected)
                        .map_err(error)?;
                }
            }
        }
        let missing = local
            .vault
            .inspect(&from)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
        let mut tx = local.db.begin_write().await.map_err(error)?;
        entries::put_baseline(&mut tx, local.binding, &after)
            .await
            .map_err(error)?;
        if missing {
            entries::remove_local(&mut tx, local.binding, id)
                .await
                .map_err(error)?;
        }
        tx.commit().await.map_err(error)?;
        return Ok(true);
    }
    if after.content_version != before.content_version {
        return Err(error(
            "move and content must follow their separate committed transaction events",
        ));
    }
    let intent = vaultsyncd_agent_core::ports::state::ActionIntent {
        op,
        binding: local.binding,
        kind: vaultsyncd_agent_core::ports::state::LocalWriteKind::Move,
        entry: Some(id),
        target_path: vaultsyncd_fs_common::path::RelativePath::parse(target).map_err(error)?,
        source_path: Some(from.clone()),
        content_hash: old
            .fingerprint
            .clone()
            .map(|v| v.try_into().map_err(|_| error("bad move fingerprint")))
            .transpose()?,
        generation: old.generation,
    };
    let intent_id = vaultsyncd_agent_core::ports::state::IntentStore::begin_intent(
        &vaultsyncd_agent_core::adapters::ClientStorePorts::new(local.db.clone()),
        &intent,
    )
    .await?;
    let destination = local.vault.inspect(target);
    let already = destination.as_ref().is_ok_and(|m| physical(m) == expected);
    if !already {
        let meta = local.vault.inspect(&from).map_err(error)?;
        if physical(&meta) != expected {
            return Err(error("move source identity changed"));
        }
        stage(
            worker,
            op,
            &serde_json::json!({"from":from,"to":target,"physical":expected}),
        )
        .await?;
        if meta.directory {
            local
                .vault
                .move_directory(&from, target, meta.identity)
                .map_err(error)?;
        } else if meta.cloud.as_ref().is_some_and(|c| c.on_disk < meta.size) {
            worker
                .importer
                .adapter
                .lock()
                .map_err(error)?
                .move_entry(&from, target, &meta)
                .map_err(error)?;
        } else {
            let snapshot = local
                .observe_snapshot(&from)
                .await?
                .ok_or_else(|| error("move source missing"))?;
            local
                .native
                .preserve(&from, target, snapshot.evidence())
                .map_err(error)?;
        }
    }
    if after.content_version == before.content_version {
        let mut tx = local.db.begin_write().await.map_err(error)?;
        entries::put_baseline(&mut tx, local.binding, &after)
            .await
            .map_err(error)?;
        vaultsyncd_client_store::intents::complete_intent(&mut tx, intent_id)
            .await
            .map_err(error)?;
        tx.commit().await.map_err(error)?;
        return Ok(true);
    }
    Ok(false)
}
