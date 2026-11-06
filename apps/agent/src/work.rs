//! Durable upload specifications and core-controlled remote application.
use crate::{
    importer::{Importer, pin},
    local::{DiskTemp, error},
};
use rand_core::{OsRng, RngCore};
use std::{collections::HashMap, sync::Arc};
use vaultsyncd_agent_core::{
    adapters::ClientStorePorts,
    engine::scheduler::{Cancellation, ScheduledHandler, TaskError},
    executor::{
        ActionFailure, ActionHandler, ActionOutcome,
        apply::{ApplySpec, ControlledApply},
        upload::{
            UploadActionHandler, UploadCommit, UploadCommitDisposition, UploadCommitStore,
            UploadSpec,
        },
    },
    ports::{
        PortError, PortFuture, Ports,
        state::{ActionIntent, QueuedOperation},
    },
};
use vaultsyncd_client_store::{
    bindings::SyncMode,
    entries::{self, BaselineEntry, EntryKind, LocalEntry},
    queue::{self, NewPendingOp},
};
use vaultsyncd_domain::{
    ids::{EntryId, OpId, VersionId},
    state::{Dirty, Residency},
    version::{Generation, Revision},
};
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_protocol::generated::{EntryKind as WireKind, SnapshotEntry};
use vaultsyncd_transfer::{download::DownloadOptions, ports::VersionPin};

pub(crate) fn new_op() -> OpId {
    let mut b = [0; 16];
    OsRng.fill_bytes(&mut b);
    OpId::from_bytes(b)
}
pub(crate) fn baseline(row: &SnapshotEntry) -> Result<BaselineEntry, PortError> {
    Ok(BaselineEntry {
        entry: EntryId::from_hex(&row.entry_id).map_err(error)?,
        parent: row
            .parent_id
            .as_deref()
            .map(EntryId::from_hex)
            .transpose()
            .map_err(error)?,
        name: row.name.clone(),
        kind: if row.kind == WireKind::File {
            EntryKind::File
        } else {
            EntryKind::Directory
        },
        revision: Revision::from_decimal(&row.revision).map_err(error)?,
        content_version: row
            .version_id
            .as_deref()
            .map(VersionId::from_hex)
            .transpose()
            .map_err(error)?,
        deleted: false,
    })
}
pub(crate) async fn metadata(
    importer: &Importer,
) -> Result<HashMap<String, SnapshotEntry>, PortError> {
    let db = &importer.local.db;
    let binding = importer.local.binding;
    let ids: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT entry_id FROM baseline_entries WHERE binding_id=? AND deleted=0",
    )
    .bind(binding.as_bytes().as_slice())
    .fetch_all(db.pool())
    .await
    .map_err(error)?;
    let mut result = HashMap::new();
    let mut conn = db.pool().acquire().await.map_err(error)?;
    for raw in ids {
        let id = EntryId::from_bytes(raw.try_into().map_err(|_| error("bad entry id"))?);
        let base = entries::baseline_entry(&mut conn, binding, id)
            .await
            .map_err(error)?
            .ok_or_else(|| error("baseline missing"))?;
        let version = if let Some(version) = base.content_version {
            let row: Option<(Vec<u8>, i64)> = sqlx::query_as(
                "SELECT digest,size FROM agent_versions WHERE binding_id=? AND version_id=?",
            )
            .bind(binding.as_bytes().as_slice())
            .bind(version.as_bytes().as_slice())
            .fetch_optional(&mut *conn)
            .await
            .map_err(error)?;
            row
        } else {
            None
        };
        result.insert(
            id.to_string(),
            SnapshotEntry {
                entry_id: id.to_string(),
                parent_id: base.parent.map(|p| p.to_string()),
                name: base.name,
                kind: if base.kind == EntryKind::File {
                    WireKind::File
                } else {
                    WireKind::Directory
                },
                revision: base.revision.to_string(),
                version_id: base.content_version.map(|v| v.to_string()),
                hash: version
                    .as_ref()
                    .map(|(h, _)| {
                        h.clone()
                            .try_into()
                            .map(|h| vaultsyncd_transfer::hash::to_hex(&h))
                            .map_err(|_| error("invalid immutable digest"))
                    })
                    .transpose()?,
                size: version.map(|(_, s)| s.to_string()),
            },
        );
    }
    Ok(result)
}
pub(crate) fn paths(
    rows: &HashMap<String, SnapshotEntry>,
) -> Result<HashMap<String, String>, PortError> {
    fn resolve(
        id: &str,
        rows: &HashMap<String, SnapshotEntry>,
        depth: usize,
    ) -> Result<String, PortError> {
        if depth > rows.len() {
            return Err(error("namespace cycle"));
        }
        let row = rows.get(id).ok_or_else(|| error("missing parent"))?;
        match &row.parent_id {
            None => Ok(String::new()),
            Some(parent) => {
                let p = resolve(parent, rows, depth + 1)?;
                Ok(if p.is_empty() {
                    row.name.clone()
                } else {
                    format!("{p}/{}", row.name)
                })
            }
        }
    }
    rows.keys()
        .map(|id| Ok((id.clone(), resolve(id, rows, 0)?)))
        .collect()
}
pub(crate) struct Commits {
    pub importer: Arc<Importer>,
}
impl UploadCommitStore for Commits {
    fn commit_upload<'a>(
        &'a self,
        commit: &'a UploadCommit,
    ) -> PortFuture<'a, Result<UploadCommitDisposition, PortError>> {
        Box::pin(async move {
            let local = &self.importer.local;
            let spec = load_spec(local, commit.op).await?;
            let path = spec["path"]
                .as_str()
                .ok_or_else(|| error("upload spec missing path"))?;
            let actual = local.observe_snapshot(path).await?;
            let mut tx = local.db.begin_write().await.map_err(error)?;
            sqlx::query(
                "UPDATE agent_actions SET result=? WHERE op_id=? AND (result IS NULL OR result=?)",
            )
            .bind(serde_json::to_string(&commit.result).map_err(error)?)
            .bind(commit.op.as_bytes().as_slice())
            .bind(serde_json::to_string(&commit.result).map_err(error)?)
            .execute(&mut *tx)
            .await
            .map_err(error)?;
            if commit.result.conflict.is_some() {
                tx.commit().await.map_err(error)?;
                return Err(PortError::rejected(
                    "NEEDS_RECOVERY",
                    "committed conflict retained; namespace reconciliation required",
                ));
            }
            let entry = EntryId::from_hex(
                commit
                    .result
                    .entry_id
                    .as_deref()
                    .ok_or_else(|| error("missing ACK entry"))?,
            )
            .map_err(error)?;
            let version = VersionId::from_hex(
                commit
                    .result
                    .version_id
                    .as_deref()
                    .ok_or_else(|| error("missing ACK version"))?,
            )
            .map_err(error)?;
            let parent = EntryId::from_hex(
                spec["parent"]
                    .as_str()
                    .ok_or_else(|| error("missing parent"))?,
            )
            .map_err(error)?;
            let name = spec["name"]
                .as_str()
                .ok_or_else(|| error("missing name"))?
                .to_owned();
            let base = BaselineEntry {
                entry,
                parent: Some(parent),
                name,
                kind: EntryKind::File,
                revision: Revision::from_decimal(
                    commit
                        .result
                        .revision
                        .as_deref()
                        .ok_or_else(|| error("missing revision"))?,
                )
                .map_err(error)?,
                content_version: Some(version),
                deleted: false,
            };
            entries::put_baseline(&mut tx, local.binding, &base)
                .await
                .map_err(error)?;
            let mut disposition = UploadCommitDisposition::StaleGeneration;
            if let Some(snapshot) = actual {
                let e = snapshot.evidence();
                let clean = e.generation == commit.generation.get()
                    && e.digest == commit.upload.content.digest
                    && e.size == commit.upload.content.size;
                entries::put_local(
                    &mut tx,
                    local.binding,
                    &LocalEntry {
                        entry,
                        generation: Generation::new(e.generation).map_err(error)?,
                        dirty: if clean { Dirty::Clean } else { Dirty::Dirty },
                        residency: Residency::Materialized,
                        fingerprint: Some(e.digest.to_vec()),
                        file_identity: Some(e.identity()),
                    },
                )
                .await
                .map_err(error)?;
                if clean {
                    disposition = UploadCommitDisposition::Applied;
                }
            }
            sqlx::query("INSERT OR IGNORE INTO agent_versions VALUES(?,?,?,?,?)")
                .bind(local.binding.as_bytes().as_slice())
                .bind(entry.as_bytes().as_slice())
                .bind(version.as_bytes().as_slice())
                .bind(commit.upload.content.digest.as_slice())
                .bind(commit.upload.content.size as i64)
                .execute(&mut *tx)
                .await
                .map_err(error)?;
            tx.commit().await.map_err(error)?;
            if disposition == UploadCommitDisposition::Applied {
                let fixed = VersionPin::new(
                    local.binding.to_string(),
                    entry.to_string(),
                    version.to_string(),
                    commit.upload.content.digest,
                    commit.upload.content.size,
                )
                .map_err(error)?;
                self.importer.adopt(path, &fixed)?;
            }
            Ok(disposition)
        })
    }
}
pub(crate) async fn load_spec(
    local: &crate::local::Local,
    op: OpId,
) -> Result<serde_json::Value, PortError> {
    let text: String = sqlx::query_scalar(
        "SELECT specification FROM agent_actions WHERE op_id=? AND binding_id=?",
    )
    .bind(op.as_bytes().as_slice())
    .bind(local.binding.as_bytes().as_slice())
    .fetch_one(local.db.pool())
    .await
    .map_err(error)?;
    serde_json::from_str(&text).map_err(error)
}
/// Shared handler for S062 background work and native user-read admission.
#[derive(Clone)]
pub(crate) struct Worker {
    pub importer: Arc<Importer>,
    pub uploads: Arc<UploadActionHandler>,
    pub mode: SyncMode,
}
#[derive(Clone)]
pub(crate) struct Dispatch(pub Arc<std::sync::OnceLock<Worker>>);
impl ActionHandler for Dispatch {
    fn plan_intent<'a>(
        &'a self,
        op: &'a QueuedOperation,
    ) -> PortFuture<'a, Result<Option<ActionIntent>, ActionFailure>> {
        Box::pin(async move {
            self.0
                .get()
                .ok_or_else(|| failure(error("startup not ready")))?
                .plan_intent(op)
                .await
        })
    }
    fn execute<'a>(
        &'a self,
        p: &'a Ports,
        o: &'a QueuedOperation,
        i: Option<&'a ActionIntent>,
    ) -> PortFuture<'a, Result<ActionOutcome, ActionFailure>> {
        Box::pin(async move {
            self.0
                .get()
                .ok_or_else(|| failure(error("startup not ready")))?
                .execute(p, o, i)
                .await
        })
    }
}
impl ScheduledHandler for Dispatch {
    fn execute_scheduled<'a>(
        &'a self,
        p: &'a Ports,
        o: &'a QueuedOperation,
        i: Option<&'a ActionIntent>,
        c: &'a Cancellation,
    ) -> PortFuture<'a, Result<ActionOutcome, TaskError>> {
        Box::pin(async move {
            self.0
                .get()
                .ok_or_else(|| TaskError::from(failure(error("startup not ready"))))?
                .execute_scheduled(p, o, i, c)
                .await
        })
    }
}
impl Worker {
    pub async fn restore_uploads(&self) -> Result<(), PortError> {
        let local = &self.importer.local;
        let mut conn = local.db.pool().acquire().await.map_err(error)?;
        let queued = queue::pending_ops_for_binding(&mut conn, local.binding)
            .await
            .map_err(error)?;
        drop(conn);
        for op in queued {
            if op.kind.starts_with("upload_") {
                let spec = load_spec(local, op.op).await?;
                self.uploads.register(
                    op.op,
                    UploadSpec::new(
                        RelativePath::parse(
                            spec["path"].as_str().ok_or_else(|| error("missing path"))?,
                        )
                        .map_err(error)?,
                        Generation::new(
                            spec["generation"]
                                .as_u64()
                                .ok_or_else(|| error("missing generation"))?,
                        )
                        .map_err(error)?,
                    )
                    .with_target(
                        EntryId::from_hex(
                            spec["parent"]
                                .as_str()
                                .ok_or_else(|| error("missing parent"))?,
                        )
                        .map_err(error)?,
                        spec["name"].as_str().ok_or_else(|| error("missing name"))?,
                    )
                    .with_expected_absent(
                        spec["new"]
                            .as_bool()
                            .ok_or_else(|| error("missing expected-absent flag"))?,
                    ),
                );
            }
        }
        Ok(())
    }
    pub async fn discover(&self) -> Result<(), PortError> {
        let local = &self.importer.local;
        let rows = metadata(&self.importer).await?;
        let by_id = paths(&rows)?;
        let by_path: HashMap<_, _> = by_id
            .iter()
            .map(|(id, path)| (path.clone(), id.clone()))
            .collect();
        let report = local.root.scan();
        if !report.permits_missing_comparison() {
            return Err(error("incomplete local scan"));
        }
        for item in report.entries {
            let meta = item.metadata.ok_or_else(|| error("missing metadata"))?;
            if meta.directory {
                continue;
            }
            if meta.cloud.as_ref().is_some_and(|c| c.on_disk < meta.size) {
                continue;
            }
            let snapshot = local
                .observe_snapshot(&item.path)
                .await?
                .ok_or_else(|| error("file vanished during scan"))?;
            let e = snapshot.evidence();
            let known = by_path.get(&item.path).and_then(|id| rows.get(id));
            let id = known
                .map(|r| EntryId::from_hex(&r.entry_id).map_err(error))
                .transpose()?;
            if let Some(row) = known {
                if row.hash.as_deref() == Some(&vaultsyncd_transfer::hash::to_hex(&e.digest))
                    && row.size.as_deref() == Some(&e.size.to_string())
                {
                    let mut tx = local.db.begin_write().await.map_err(error)?;
                    entries::put_local(
                        &mut tx,
                        local.binding,
                        &LocalEntry {
                            entry: id.unwrap(),
                            generation: Generation::new(e.generation).map_err(error)?,
                            dirty: Dirty::Clean,
                            residency: Residency::Materialized,
                            fingerprint: Some(e.digest.to_vec()),
                            file_identity: Some(e.identity()),
                        },
                    )
                    .await
                    .map_err(error)?;
                    tx.commit().await.map_err(error)?;
                    continue;
                }
            }
            let parts: Vec<_> = item.path.rsplitn(2, '/').collect();
            let name = parts[0];
            let parent_path = parts.get(1).copied().unwrap_or("");
            let parent = by_path
                .get(parent_path)
                .ok_or_else(|| error("new parent requires namespace reconciliation"))?;
            let existing:Vec<String>=sqlx::query_scalar("SELECT a.specification FROM agent_actions a JOIN pending_ops p ON p.op_id=a.op_id WHERE p.binding_id=? AND p.kind IN ('upload_content','upload_new_entry')").bind(local.binding.as_bytes().as_slice()).fetch_all(local.db.pool()).await.map_err(error)?;
            if existing.iter().any(|s| {
                serde_json::from_str::<serde_json::Value>(s)
                    .ok()
                    .is_some_and(|v| {
                        v["path"] == item.path && v["generation"].as_u64() == Some(e.generation)
                    })
            }) {
                continue;
            }
            let op = new_op();
            let value = serde_json::json!({"kind":"upload","path":item.path,"generation":e.generation,"parent":parent,"name":name,"new":known.is_none()});
            let text = serde_json::to_string(&value).map_err(error)?;
            let mut tx = local.db.begin_write().await.map_err(error)?;
            if let Some(id) = id {
                entries::put_local(
                    &mut tx,
                    local.binding,
                    &LocalEntry {
                        entry: id,
                        generation: Generation::new(e.generation).map_err(error)?,
                        dirty: Dirty::Dirty,
                        residency: Residency::Materialized,
                        fingerprint: Some(e.digest.to_vec()),
                        file_identity: Some(e.identity()),
                    },
                )
                .await
                .map_err(error)?;
            }
            queue::enqueue(
                &mut tx,
                &NewPendingOp {
                    op,
                    binding: local.binding,
                    kind: if known.is_some() {
                        "upload_content"
                    } else {
                        "upload_new_entry"
                    }
                    .into(),
                    entry: id,
                    baseline_revision: known
                        .map(|r| Revision::from_decimal(&r.revision).map_err(error))
                        .transpose()?,
                    baseline_version: known
                        .and_then(|r| r.version_id.as_deref())
                        .map(VersionId::from_hex)
                        .transpose()
                        .map_err(error)?,
                    depends_on: None,
                    pinned_version: None,
                },
            )
            .await
            .map_err(error)?;
            sqlx::query("INSERT INTO agent_actions(op_id,binding_id,specification) VALUES(?,?,?)")
                .bind(op.as_bytes().as_slice())
                .bind(local.binding.as_bytes().as_slice())
                .bind(text)
                .execute(&mut *tx)
                .await
                .map_err(error)?;
            tx.commit().await.map_err(error)?;
        }
        self.restore_uploads().await
    }
    async fn apply(&self, op: OpId, cancel: &Cancellation) -> Result<(), PortError> {
        let local = &self.importer.local;
        let spec = load_spec(local, op).await?;
        if spec["deleted"] == true {
            return Err(PortError::rejected(
                "UNSUPPORTED_ENTRY",
                "delete retained for tombstone coordination",
            ));
        }
        let row: SnapshotEntry = serde_json::from_value(spec["remote"].clone()).map_err(error)?;
        let target = RelativePath::parse(
            spec["path"]
                .as_str()
                .ok_or_else(|| error("missing event path"))?,
        )
        .map_err(error)?;
        let after = baseline(&row)?;
        if row.kind == WireKind::Directory {
            local
                .root
                .ensure_directory(&target.to_string())
                .map_err(error)?;
            let mut tx = local.db.begin_write().await.map_err(error)?;
            entries::put_baseline(&mut tx, local.binding, &after)
                .await
                .map_err(error)?;
            tx.commit().await.map_err(error)?;
            return Ok(());
        }
        let fixed = pin(local, &row).await?;
        let mut conn = local.db.pool().acquire().await.map_err(error)?;
        let before = entries::baseline_entry(&mut conn, local.binding, after.entry)
            .await
            .map_err(error)?;
        drop(conn);
        let meta = local.root.inspect(&target.to_string());
        if self.mode == SyncMode::OnDemand
            && meta
                .as_ref()
                .ok()
                .and_then(|m| m.cloud.as_ref())
                .is_some_and(|c| c.on_disk == 0)
        {
            let changed = {
                let adapter = self.importer.adapter.lock().map_err(error)?;
                let catalog = adapter.catalog();
                let root = catalog.read().map_err(error)?.root;
                let id = vaultsyncd_platform_windows::cfapi::catalog::Identity {
                    binding: local.binding,
                    root,
                    entry: after.entry,
                    version: after.content_version.unwrap(),
                };
                self.importer
                    .callbacks
                    .register_version(
                        crate::bootstrap::PinnedVersion::new(id, fixed.clone()).map_err(error)?,
                    )
                    .map_err(error)?;
                let changed = adapter
                    .update(
                        &target.to_string(),
                        meta.as_ref().map_err(error)?,
                        id,
                        fixed.size,
                    )
                    .map_err(error)?;
                catalog.write().map_err(error)?.insert(id).map_err(error)?;
                changed
            };
            let mut tx = local.db.begin_write().await.map_err(error)?;
            entries::put_baseline(&mut tx, local.binding, &after)
                .await
                .map_err(error)?;
            let mut physical = changed.identity.volume.to_le_bytes().to_vec();
            physical.extend(changed.identity.file);
            entries::put_local(
                &mut tx,
                local.binding,
                &LocalEntry {
                    entry: after.entry,
                    generation: Generation::ZERO,
                    dirty: Dirty::Clean,
                    residency: Residency::MetadataOnly,
                    fingerprint: Some(fixed.digest.to_vec()),
                    file_identity: Some(physical),
                },
            )
            .await
            .map_err(error)?;
            tx.commit().await.map_err(error)?;
            return Ok(());
        }
        let actual = vaultsyncd_agent_core::recovery::inspect(&*local.clone(), &target).await?;
        if let Some(base) = &before {
            let old: Option<(Vec<u8>, i64)> = sqlx::query_as(
                "SELECT digest,size FROM agent_versions WHERE binding_id=? AND version_id=?",
            )
            .bind(local.binding.as_bytes().as_slice())
            .bind(
                base.content_version
                    .ok_or_else(|| error("missing baseline version"))?
                    .as_bytes()
                    .as_slice(),
            )
            .fetch_optional(local.db.pool())
            .await
            .map_err(error)?;
            let (hash, size) = old.ok_or_else(|| error("unknown baseline content"))?;
            let hash: [u8; 32] = hash.try_into().map_err(|_| error("bad digest"))?;
            if !actual.has_content(&hash, size as u64)
                && !actual.has_content(&fixed.digest, fixed.size)
            {
                return Err(error("unconfirmed local content retained"));
            }
        } else if actual != vaultsyncd_agent_core::recovery::ContentEvidence::Absent
            && !actual.has_content(&fixed.digest, fixed.size)
        {
            return Err(error("new remote path occupied"));
        }
        let apply = ApplySpec {
            op,
            binding: local.binding,
            target: target.clone(),
            baseline_before: before,
            baseline_after: after,
            pin: fixed.clone(),
            expected: actual,
        };
        let temp = DiskTemp::new(&local.cache, op);
        ControlledApply::new(local.db.clone(), local.clone())
            .apply(
                &apply,
                &*self.importer.content,
                &temp,
                &ClientStorePorts::new(local.db.clone()),
                &DownloadOptions::new(1024 * 1024).with_cancel(cancel.clone()),
            )
            .await?;
        self.importer.adopt(&target.to_string(), &fixed)?;
        local
            .native
            .acknowledge_publication(&op.to_string())
            .map_err(error)?;
        Ok(())
    }
}
fn failure(e: PortError) -> ActionFailure {
    ActionFailure::new(
        vaultsyncd_domain::error::ErrorCode::NeedsRecovery,
        e.to_string(),
    )
}
impl ActionHandler for Worker {
    fn plan_intent<'a>(
        &'a self,
        _: &'a QueuedOperation,
    ) -> PortFuture<'a, Result<Option<ActionIntent>, ActionFailure>> {
        Box::pin(async { Ok(None) })
    }
    fn execute<'a>(
        &'a self,
        ports: &'a Ports,
        op: &'a QueuedOperation,
        intent: Option<&'a ActionIntent>,
    ) -> PortFuture<'a, Result<ActionOutcome, ActionFailure>> {
        Box::pin(async move {
            if op.kind.starts_with("upload_") {
                self.uploads.execute(ports, op, intent).await
            } else {
                self.apply(op.op, &Cancellation::new())
                    .await
                    .map_err(failure)?;
                Ok(ActionOutcome::Completed)
            }
        })
    }
}
impl ScheduledHandler for Worker {
    fn execute_scheduled<'a>(
        &'a self,
        ports: &'a Ports,
        op: &'a QueuedOperation,
        intent: Option<&'a ActionIntent>,
        cancel: &'a Cancellation,
    ) -> PortFuture<'a, Result<ActionOutcome, TaskError>> {
        Box::pin(async move {
            if op.kind.starts_with("upload_") {
                self.uploads
                    .execute_scheduled(ports, op, intent, cancel)
                    .await
            } else {
                self.apply(op.op, cancel)
                    .await
                    .map_err(failure)
                    .map_err(TaskError::from)?;
                Ok(ActionOutcome::Completed)
            }
        })
    }
}
