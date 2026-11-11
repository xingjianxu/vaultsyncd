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
    local_metadata(&importer.local).await
}
pub(crate) async fn local_metadata(
    local: &crate::local::Local,
) -> Result<HashMap<String, SnapshotEntry>, PortError> {
    let db = &local.db;
    let binding = local.binding;
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
                modified_at: None,
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
            self.finish(
                commit.op,
                commit.generation,
                commit.upload.content.digest,
                commit.upload.content.size,
                &commit.result,
            )
            .await
        })
    }
}
impl Commits {
    pub(crate) async fn finish(
        &self,
        op: OpId,
        generation: Generation,
        digest: [u8; 32],
        size: u64,
        result: &vaultsyncd_protocol::generated::OperationResult,
    ) -> Result<UploadCommitDisposition, PortError> {
        let local = &self.importer.local;
        let spec = load_spec(local, op).await?;
        let path = spec["path"]
            .as_str()
            .ok_or_else(|| error("upload spec missing path"))?;
        crate::conflicts::record_ack(local, op, result).await?;
        if result.conflict.is_some() {
            return crate::conflicts::settle(&self.importer, op, generation, digest, size, result)
                .await;
        }
        let entry = EntryId::from_hex(
            result
                .entry_id
                .as_deref()
                .ok_or_else(|| error("missing ACK entry"))?,
        )
        .map_err(error)?;
        let version = VersionId::from_hex(
            result
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
                result
                    .revision
                    .as_deref()
                    .ok_or_else(|| error("missing revision"))?,
            )
            .map_err(error)?,
            content_version: Some(version),
            deleted: false,
        };
        // A committed upload can race a newer local/remote delete. Never resurrect that
        // namespace fact just because the ACK arrived late; still retain the version mapping.
        let mut conn = local.db.pool().acquire().await.map_err(error)?;
        let current = entries::baseline_entry(&mut conn, local.binding, entry)
            .await
            .map_err(error)?;
        drop(conn);
        let is_superseded = |row: &BaselineEntry| {
            row.revision > base.revision || (row.deleted && row.revision >= base.revision)
        };
        let superseded = current.as_ref().is_some_and(is_superseded);
        let actual = if superseded {
            None
        } else {
            match local.vault.inspect(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(error(e)),
                Ok(_) => local.observe_snapshot(path).await?,
            }
        };
        let mut tx = local.db.begin_write().await.map_err(error)?;
        // Recheck inside the write transaction so a delete committed by another step wins.
        let current = entries::baseline_entry(&mut tx, local.binding, entry)
            .await
            .map_err(error)?;
        let superseded = current.as_ref().is_some_and(is_superseded);
        if !superseded {
            entries::put_baseline(&mut tx, local.binding, &base)
                .await
                .map_err(error)?;
        }
        let mut disposition = UploadCommitDisposition::StaleGeneration;
        if !superseded && let Some(snapshot) = actual {
            let e = snapshot.evidence();
            let clean = e.generation == generation.get() && e.digest == digest && e.size == size;
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
            .bind(digest.as_slice())
            .bind(size as i64)
            .execute(&mut *tx)
            .await
            .map_err(error)?;
        tx.commit().await.map_err(error)?;
        if disposition == UploadCommitDisposition::Applied {
            let fixed = VersionPin::new(
                local.binding.to_string(),
                entry.to_string(),
                version.to_string(),
                digest,
                size,
            )
            .map_err(error)?;
            self.importer.adopt(path, &fixed)?;
        }
        Ok(disposition)
    }
}
pub(crate) async fn load_spec_optional(
    local: &crate::local::Local,
    op: OpId,
) -> Result<Option<serde_json::Value>, PortError> {
    let text: Option<String> = sqlx::query_scalar(
        "SELECT specification FROM agent_actions WHERE op_id=? AND binding_id=?",
    )
    .bind(op.as_bytes().as_slice())
    .bind(local.binding.as_bytes().as_slice())
    .fetch_optional(local.db.pool())
    .await
    .map_err(error)?;
    text.map(|text| serde_json::from_str(&text).map_err(error))
        .transpose()
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
    async fn acknowledged_upload(&self, op: OpId) -> Result<bool, PortError> {
        let local = &self.importer.local;
        let result: Option<String> =
            sqlx::query_scalar("SELECT result FROM agent_actions WHERE op_id=? AND binding_id=?")
                .bind(op.as_bytes().as_slice())
                .bind(local.binding.as_bytes().as_slice())
                .fetch_one(local.db.pool())
                .await
                .map_err(error)?;
        let Some(result) = result else {
            return Ok(false);
        };
        let result: vaultsyncd_protocol::generated::OperationResult =
            serde_json::from_str(&result).map_err(error)?;
        let spec = load_spec(local, op).await?;
        let path = spec["path"]
            .as_str()
            .ok_or_else(|| error("missing acknowledged path"))?;
        let generation = spec["generation"]
            .as_u64()
            .ok_or_else(|| error("missing acknowledged generation"))?;
        if result.echo_local_generation.as_deref() != Some(&generation.to_string()) {
            return Err(error(
                "stored ACK generation differs from its durable upload",
            ));
        }
        let evidence: String = sqlx::query_scalar(
            "SELECT evidence FROM agent_evidence WHERE binding_id=? AND path=? AND generation=?",
        )
        .bind(local.binding.as_bytes().as_slice())
        .bind(path)
        .bind(generation as i64)
        .fetch_one(local.db.pool())
        .await
        .map_err(error)?;
        let evidence: vaultsyncd_platform_windows::files::controlled::Evidence =
            serde_json::from_str(&evidence).map_err(error)?;
        if evidence.generation != generation {
            return Err(error("stored upload evidence generation differs"));
        }
        Commits {
            importer: self.importer.clone(),
        }
        .finish(
            op,
            Generation::new(generation).map_err(error)?,
            evidence.digest,
            evidence.size,
            &result,
        )
        .await?;
        vaultsyncd_agent_core::ports::state::UploadResumeStore::discard_resume(
            &ClientStorePorts::new(local.db.clone()),
            op,
        )
        .await?;
        Ok(true)
    }
    pub async fn restore_uploads(&self) -> Result<(), PortError> {
        let local = &self.importer.local;
        let mut conn = local.db.pool().acquire().await.map_err(error)?;
        let queued = queue::pending_ops_for_binding(&mut conn, local.binding)
            .await
            .map_err(error)?;
        drop(conn);
        for op in queued {
            if op.kind.starts_with("upload_") {
                let Some(spec) = load_spec_optional(local, op.op).await? else {
                    // First-import uploads are owned by the import journal and carry no
                    // action spec. While the journal is unfinished its own pass decides
                    // (and now drops excluded rows); a completed journal leaves only a
                    // stale residue that must not keep failing every restore round.
                    let importing: bool = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM binding_imports WHERE binding_id=? AND complete=0)",
                    )
                    .bind(local.binding.as_bytes().as_slice())
                    .fetch_one(local.db.pool())
                    .await
                    .map_err(error)?;
                    if !importing {
                        let mut tx = local.db.begin_write().await.map_err(error)?;
                        vaultsyncd_client_store::queue::complete(&mut tx, op.op)
                            .await
                            .map_err(error)?;
                        tx.commit().await.map_err(error)?;
                    }
                    continue;
                };
                if spec["kind"] != "upload" {
                    continue;
                }
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
        let rules = crate::exclusions::Rules::load(&local.db, local.binding).await?;
        let rows = metadata(&self.importer).await?;
        let by_id = paths(&rows)?;
        let by_path: HashMap<_, _> = by_id
            .iter()
            .map(|(id, path)| (path.clone(), id.clone()))
            .collect();
        let report = local.vault.scan();
        if !report.permits_missing_comparison() {
            return Err(error("incomplete local scan"));
        }
        let scan: HashMap<_, _> = report
            .entries
            .into_iter()
            .map(|item| {
                Ok((
                    item.path,
                    item.metadata.ok_or_else(|| error("missing metadata"))?,
                ))
            })
            .collect::<Result<_, PortError>>()?;
        if crate::namespace::discover(self, &scan).await? {
            return Ok(());
        }
        for (path, meta) in scan {
            if !rules.includes(&path, meta.directory)? {
                continue;
            }
            if meta.directory {
                continue;
            }
            if meta.cloud.as_ref().is_some_and(|c| c.on_disk < meta.size) {
                continue;
            }
            let snapshot = local
                .observe_snapshot(&path)
                .await?
                .ok_or_else(|| error("file vanished during scan"))?;
            let e = snapshot.evidence();
            let known = by_path.get(&path).and_then(|id| rows.get(id));
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
            let parts: Vec<_> = path.rsplitn(2, '/').collect();
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
                        v["path"] == path && v["generation"].as_u64() == Some(e.generation)
                    })
            }) {
                continue;
            }
            let op = new_op();
            let value = serde_json::json!({"kind":"upload","path":path,"generation":e.generation,"parent":parent,"name":name,"new":known.is_none()});
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
        let row: SnapshotEntry = serde_json::from_value(spec["remote"].clone()).map_err(error)?;
        let after = baseline(&row)?;
        let mut conn = local.db.pool().acquire().await.map_err(error)?;
        let confirmed = entries::baseline_entry(&mut conn, local.binding, after.entry)
            .await
            .map_err(error)?;
        drop(conn);
        // Replayed self-ACKs and historical events are facts already confirmed locally.
        // Reinstalling them would resurrect a file that the user has just moved or removed.
        if confirmed.as_ref().is_some_and(|b| {
            b.revision > after.revision
                || (b.revision == after.revision
                    && b == &BaselineEntry {
                        deleted: spec["deleted"] == true,
                        ..after.clone()
                    })
        }) {
            return Ok(());
        }
        if crate::namespace::incoming(self, op, &spec, &row).await? {
            return Ok(());
        }
        let target = RelativePath::parse(
            spec["path"]
                .as_str()
                .ok_or_else(|| error("missing event path"))?,
        )
        .map_err(error)?;
        if row.kind == WireKind::Directory {
            let intent = ActionIntent {
                op,
                binding: local.binding,
                kind: vaultsyncd_agent_core::ports::state::LocalWriteKind::Import,
                entry: Some(after.entry),
                target_path: target.clone(),
                source_path: None,
                content_hash: None,
                generation: Generation::ZERO,
            };
            let intent_id = vaultsyncd_agent_core::ports::state::IntentStore::begin_intent(
                &ClientStorePorts::new(local.db.clone()),
                &intent,
            )
            .await?;
            let meta = local
                .vault
                .ensure_directory(&target.to_string())
                .map_err(error)?;
            let mut physical = meta.identity.volume.to_le_bytes().to_vec();
            physical.extend(meta.identity.file);
            let mut tx = local.db.begin_write().await.map_err(error)?;
            entries::put_baseline(&mut tx, local.binding, &after)
                .await
                .map_err(error)?;
            entries::put_local(
                &mut tx,
                local.binding,
                &LocalEntry {
                    entry: after.entry,
                    generation: Generation::ZERO,
                    dirty: Dirty::Clean,
                    residency: Residency::Materialized,
                    fingerprint: None,
                    file_identity: Some(physical),
                },
            )
            .await
            .map_err(error)?;
            vaultsyncd_client_store::intents::complete_intent(&mut tx, intent_id)
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
        let meta = local.vault.inspect(&target.to_string());
        let mode: String = sqlx::query_scalar("SELECT mode FROM bindings WHERE id=?")
            .bind(local.binding.as_bytes().as_slice())
            .fetch_one(local.db.pool())
            .await
            .map_err(error)?;
        if mode == "on_demand"
            && meta
                .as_ref()
                .ok()
                .and_then(|m| m.cloud.as_ref())
                .is_some_and(|c| c.on_disk == 0)
        {
            let mut conn = local.db.pool().acquire().await.map_err(error)?;
            let previous = entries::local_entry(&mut conn, local.binding, after.entry)
                .await
                .map_err(error)?
                .ok_or_else(|| error("placeholder local state missing"))?;
            drop(conn);
            let previous_version = before
                .as_ref()
                .and_then(|b| b.content_version)
                .ok_or_else(|| error("placeholder baseline version missing"))?;
            let previous_size: i64 = sqlx::query_scalar(
                "SELECT size FROM agent_versions WHERE binding_id=? AND version_id=?",
            )
            .bind(local.binding.as_bytes().as_slice())
            .bind(previous_version.as_bytes().as_slice())
            .fetch_one(local.db.pool())
            .await
            .map_err(error)?;
            if previous.dirty != Dirty::Clean
                || meta.as_ref().map_err(error)?.size != previous_size as u64
                || meta
                    .as_ref()
                    .map_err(error)?
                    .cloud
                    .as_ref()
                    .is_none_or(|c| c.modified != 0)
            {
                return Err(PortError::rejected(
                    "LOCAL_CONTENT_PENDING",
                    "placeholder content changed since baseline",
                ));
            }
            let changed = {
                let adapter = self.importer.adapter.lock().map_err(error)?;
                let catalog = adapter.catalog();
                let vault = catalog.read().map_err(error)?.vault;
                let id = vaultsyncd_platform_windows::cfapi::catalog::Identity {
                    binding: local.binding,
                    vault,
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
                    generation: previous.generation,
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
        if before.as_ref().is_some_and(|base| base.deleted) {
            if actual != vaultsyncd_agent_core::recovery::ContentEvidence::Absent {
                return Err(error(
                    "restored remote entry cannot overwrite a locally reused path",
                ));
            }
        } else if let Some(base) = &before {
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
                return Err(PortError::rejected(
                    "LOCAL_CONTENT_PENDING",
                    "unconfirmed local content retained while its upload is reconciled",
                ));
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
        if local
            .native
            .publication_present(&op.to_string())
            .map_err(error)?
        {
            local
                .native
                .acknowledge_publication(&op.to_string())
                .map_err(error)?;
        }
        Ok(())
    }
}
fn failure(e: PortError) -> ActionFailure {
    use vaultsyncd_agent_core::executor::FailureScheduling;
    use vaultsyncd_domain::error::ErrorCode;
    let (code, scheduling) = match &e {
        PortError::Unavailable { retry_after, .. } => (
            ErrorCode::RateLimited,
            FailureScheduling::Unavailable {
                retry_after: *retry_after,
            },
        ),
        PortError::Cancelled => (ErrorCode::RateLimited, FailureScheduling::Cancelled),
        PortError::ReauthRequired { .. } => (ErrorCode::AuthExpired, FailureScheduling::ByCode),
        PortError::Rejected { code, .. } => (
            ErrorCode::from_code(code).unwrap_or(ErrorCode::NeedsRecovery),
            FailureScheduling::ByCode,
        ),
        _ => (ErrorCode::NeedsRecovery, FailureScheduling::ByCode),
    };
    ActionFailure::new(code, e.to_string()).with_scheduling(scheduling)
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
            if load_spec(&self.importer.local, op.op)
                .await
                .map_err(failure)?["kind"]
                == "namespace"
            {
                crate::namespace::outgoing(self, ports, op)
                    .await
                    .map_err(failure)
            } else if op.kind.starts_with("upload_") {
                if self.acknowledged_upload(op.op).await.map_err(failure)? {
                    return Ok(ActionOutcome::Completed);
                }
                self.uploads.execute(ports, op, intent).await
            } else {
                match self.apply(op.op, &Cancellation::new()).await {
                    Ok(()) => Ok(ActionOutcome::Completed),
                    Err(e)
                        if matches!(e.code(), Some("SUBTREE_BUSY" | "LOCAL_CONTENT_PENDING")) =>
                    {
                        Ok(ActionOutcome::Pending {
                            poll_after: Some(std::time::Duration::from_secs(1)),
                        })
                    }
                    Err(e) => Err(failure(e)),
                }
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
            if load_spec(&self.importer.local, op.op)
                .await
                .map_err(failure)
                .map_err(TaskError::from)?["kind"]
                == "namespace"
            {
                crate::namespace::outgoing(self, ports, op)
                    .await
                    .map_err(failure)
                    .map_err(TaskError::from)
            } else if op.kind.starts_with("upload_") {
                if self
                    .acknowledged_upload(op.op)
                    .await
                    .map_err(failure)
                    .map_err(TaskError::from)?
                {
                    return Ok(ActionOutcome::Completed);
                }
                self.uploads
                    .execute_scheduled(ports, op, intent, cancel)
                    .await
            } else {
                match self.apply(op.op, cancel).await {
                    Ok(()) => Ok(ActionOutcome::Completed),
                    Err(e)
                        if matches!(e.code(), Some("SUBTREE_BUSY" | "LOCAL_CONTENT_PENDING")) =>
                    {
                        Ok(ActionOutcome::Pending {
                            poll_after: Some(std::time::Duration::from_millis(500)),
                        })
                    }
                    Err(e) => Err(TaskError::from(failure(e))),
                }
            }
        })
    }
}
