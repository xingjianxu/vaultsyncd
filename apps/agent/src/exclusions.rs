//! Binding rules gate durable tasks by their actual paths, and preserve the old three-way baseline.
//! Rule removal first reads a complete authenticated snapshot and observes retained local bytes;
//! ordinary revision/generation-fenced event and upload executors then perform reconciliation.
use crate::{
    local::error,
    work::{Worker, paths},
};
use std::sync::Arc;
use vaultsyncd_agent_core::{
    bindings::{
        control::{BindingControls, ExecutionGate},
        import::{ImportLocal, ImportSnapshot},
    },
    engine::scheduler::DispatchGate,
    ports::state::QueuedOperation,
};
use vaultsyncd_client_store::{
    db::Database,
    entries::{self, LocalEntry},
};
use vaultsyncd_domain::{
    ids::{BindingId, EntryId},
    state::{Dirty, Residency},
    version::Generation,
};
use vaultsyncd_fs_common::{ignore::IgnoreRules, path::RelativePath};
use vaultsyncd_transfer::ports::{PortError, PortFuture};

pub(crate) struct Rules {
    pub version: String,
    rules: IgnoreRules,
    pub active: bool,
}
impl Rules {
    pub async fn load(db: &Arc<Database>, binding: BindingId) -> Result<Self, PortError> {
        BindingControls::new(db.clone())
            .adopt_default_policy(binding)
            .await?;
        let (version,text,paused,detached):(String,String,bool,bool)=sqlx::query_as("SELECT b.rules_version,coalesce(c.rules,''),coalesce(c.paused,0),coalesce(c.detached,0) FROM bindings b LEFT JOIN binding_controls c ON c.binding_id=b.id WHERE b.id=?")
            .bind(binding.as_bytes().as_slice()).fetch_one(db.pool()).await.map_err(error)?;
        Ok(Self {
            version,
            rules: IgnoreRules::parse_with_defaults(&text).map_err(error)?,
            active: !paused && !detached,
        })
    }
    pub fn includes(&self, path: &str, directory: bool) -> Result<bool, PortError> {
        Ok(path.is_empty()
            || !self
                .rules
                .is_excluded(&RelativePath::parse(path).map_err(error)?, directory))
    }
}

/// Whether a durable action specification still participates under the effective rules.
/// Directory matching is conservative for namespace/new tasks whose final kind is unknown.
fn spec_included(rules: &Rules, spec: &serde_json::Value) -> Result<bool, PortError> {
    let directory = spec["remote"]["kind"] != "file" && spec["kind"] != "upload";
    for field in ["path", "from"] {
        if let Some(path) = spec[field].as_str() {
            if !rules.includes(path, directory)? {
                return Ok(false);
            }
        } else if field == "path" {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Queue and intent rows that still owe controlled work. Rows parked by an exclusion keep
/// their bytes, baseline and queue position, but they cannot be dispatched until re-inclusion
/// is reconciled; they must not hold back unrelated full-mode progress on the same binding.
/// A row without a durable specification is first-import residue; the journal owns it.
pub(crate) async fn unsettled(db: &Arc<Database>, binding: BindingId) -> Result<i64, PortError> {
    let rules = Rules::load(db, binding).await?;
    let mut count = 0i64;
    let ops: Vec<Vec<u8>> = sqlx::query_scalar("SELECT op_id FROM pending_ops WHERE binding_id=?")
        .bind(binding.as_bytes().as_slice())
        .fetch_all(db.pool())
        .await
        .map_err(error)?;
    for raw in ops {
        let op = vaultsyncd_domain::ids::OpId::from_bytes(
            raw.try_into()
                .map_err(|_| error("invalid queued identity"))?,
        );
        let text: Option<String> = sqlx::query_scalar(
            "SELECT specification FROM agent_actions WHERE op_id=? AND binding_id=?",
        )
        .bind(op.as_bytes().as_slice())
        .bind(binding.as_bytes().as_slice())
        .fetch_optional(db.pool())
        .await
        .map_err(error)?;
        match text {
            Some(text) => {
                let spec: serde_json::Value = serde_json::from_str(&text).map_err(error)?;
                if spec_included(&rules, &spec)? {
                    count += 1;
                }
            }
            None => count += 1,
        }
    }
    let intents: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT target_path,source_path FROM local_intents WHERE binding_id=?")
            .bind(binding.as_bytes().as_slice())
            .fetch_all(db.pool())
            .await
            .map_err(error)?;
    for (target, source) in intents {
        let mut included = rules.includes(&target, false)?;
        if let Some(source) = &source {
            included = included && rules.includes(source, false)?;
        }
        if included {
            count += 1;
        }
    }
    Ok(count)
}

pub(crate) struct Gate(pub Arc<Database>);
impl DispatchGate for Gate {
    fn allowed<'a>(&'a self, op: &'a QueuedOperation) -> PortFuture<'a, Result<bool, PortError>> {
        Box::pin(async move {
            let rules = Rules::load(&self.0, op.binding).await?;
            if !rules.active {
                return Ok(false);
            }
            if op.kind == "user_read" {
                let gate = BindingControls::new(self.0.clone())
                    .execution_gate(op.binding, op.entry)
                    .await?;
                // Reads of the already pinned old version may complete retained partial bytes.
                // This grants no upload, replacement, release or baseline ACK.
                if gate == ExecutionGate::ReconcileRequired {
                    return sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM local_entries l JOIN baseline_entries b USING(binding_id,entry_id) WHERE l.binding_id=? AND l.entry_id=? AND l.dirty='clean' AND b.deleted=0 AND b.content_version_id IS NOT NULL)")
                        .bind(op.binding.as_bytes().as_slice()).bind(op.entry.map(|id| id.as_bytes().to_vec()))
                        .fetch_one(self.0.pool()).await.map_err(error);
                }
                return Ok(gate == ExecutionGate::Ready);
            }
            let text: Option<String> = sqlx::query_scalar(
                "SELECT specification FROM agent_actions WHERE op_id=? AND binding_id=?",
            )
            .bind(op.op.as_bytes().as_slice())
            .bind(op.binding.as_bytes().as_slice())
            .fetch_optional(self.0.pool())
            .await
            .map_err(error)?;
            let Some(text) = text else {
                return Ok(false);
            };
            let spec: serde_json::Value = serde_json::from_str(&text).map_err(error)?;
            if !spec_included(&rules, &spec)? {
                return Ok(false);
            }
            if let Some(entry) = op.entry {
                let gate = BindingControls::new(self.0.clone())
                    .execution_gate(op.binding, Some(entry))
                    .await?;
                if gate == ExecutionGate::Ready {
                    return Ok(true);
                }
                if gate != ExecutionGate::ReconcileRequired {
                    return Ok(false);
                }
                let known: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM baseline_entries WHERE binding_id=? AND entry_id=?) OR EXISTS(SELECT 1 FROM binding_participation WHERE binding_id=? AND entry_id=?)")
                    .bind(op.binding.as_bytes().as_slice()).bind(entry.as_bytes().as_slice()).bind(op.binding.as_bytes().as_slice()).bind(entry.as_bytes().as_slice()).fetch_one(self.0.pool()).await.map_err(error)?;
                // New remote entries have no old namespace row; their validated event path is authority.
                return Ok(!known);
            }
            Ok(true)
        })
    }
}

/// Called after the event receiver has refreshed durable remote facts, before new local discovery.
pub(crate) async fn reconcile(worker: &Worker) -> Result<(), PortError> {
    reconcile_selected(worker, None, None).await
}

/// Existing DownloadEntry is the explicit recovery entry point; exclusion alone never restores a deleted file.
pub(crate) async fn recover(
    worker: &Worker,
    entry: EntryId,
    operation: vaultsyncd_domain::ids::OpId,
) -> Result<(), PortError> {
    reconcile_selected(worker, Some(entry), Some(operation)).await?;
    if BindingControls::new(worker.importer.local.db.clone())
        .execution_gate(worker.importer.local.binding, Some(entry))
        .await?
        != ExecutionGate::Ready
    {
        return Err(PortError::rejected(
            "RECONCILE_REQUIRED",
            "恢复未完成：请检查远端连接、规则及本地未确认内容；现场与围栏保留",
        ));
    }
    Ok(())
}

async fn reconcile_selected(
    worker: &Worker,
    explicit: Option<EntryId>,
    operation: Option<vaultsyncd_domain::ids::OpId>,
) -> Result<(), PortError> {
    let local = &worker.importer.local;
    // Refresh first: a remote rename can turn a persisted exclusion into a reconciliation.
    let rules = Rules::load(&local.db, local.binding).await?;
    let pending:Vec<Vec<u8>>=sqlx::query_scalar("SELECT entry_id FROM binding_participation WHERE binding_id=? AND state='reconcile_required'")
        .bind(local.binding.as_bytes().as_slice()).fetch_all(local.db.pool()).await.map_err(error)?;
    if pending.is_empty() {
        return Ok(());
    }
    if !rules.active {
        return Ok(());
    }
    // An unavailable/incomplete remote cannot authorize old local files. Do not reuse first import.
    let remote = &worker.importer.remote;
    let seed = remote.begin(remote.vault).await?;
    let mut cursor = None;
    let mut count = 0u64;
    let mut current = std::collections::HashMap::new();
    loop {
        let page = remote
            .page(remote.vault, &seed.metadata.snapshot_id, cursor.as_deref())
            .await?;
        if page.snapshot_id != seed.metadata.snapshot_id {
            return Err(error("rule reconciliation snapshot changed"));
        }
        if page.high_water_seq != seed.metadata.high_water_seq {
            return Err(error("rule reconciliation high water changed"));
        }
        count += page.entries.len() as u64;
        for row in page.entries {
            if current.insert(row.entry_id.clone(), row).is_some() {
                return Err(error("duplicate rule reconciliation entry"));
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    if seed.metadata.entry_count.parse::<u64>().map_err(error)? != count {
        return Err(error("incomplete rule reconciliation snapshot"));
    }
    let epoch: String =
        sqlx::query_scalar("SELECT cast(server_epoch AS TEXT) FROM bindings WHERE id=?")
            .bind(local.binding.as_bytes().as_slice())
            .fetch_one(local.db.pool())
            .await
            .map_err(error)?;
    if epoch != seed.metadata.server_epoch {
        return Err(error("rule reconciliation epoch changed"));
    }
    let received: i64 = sqlx::query_scalar("SELECT received_seq FROM bindings WHERE id=?")
        .bind(local.binding.as_bytes().as_slice())
        .fetch_one(local.db.pool())
        .await
        .map_err(error)?;
    // A write racing the preceding pull must be received before any excluded operation is reopened.
    if (received as u64) != seed.metadata.high_water_seq.parse::<u64>().map_err(error)? {
        return Ok(());
    }
    // The authenticated remote namespace includes excluded entries that never had a local
    // baseline. Baseline-only metadata cannot resolve those paths or authorize restoration.
    let names = paths(&current)?;
    // Parents first: a restored directory must exist before its children are restored.
    let mut pending = pending;
    pending.sort_by_key(|raw| {
        names
            .get(&EntryId::from_bytes(raw[..].try_into().unwrap_or_default()).to_string())
            .map(|path| path.matches('/').count())
            .unwrap_or(usize::MAX)
    });
    for raw in pending {
        let entry = EntryId::from_bytes(
            raw.try_into()
                .map_err(|_| error("invalid participation identity"))?,
        );
        if explicit.is_some_and(|selected| selected != entry) {
            continue;
        }
        let mut connection = local.db.pool().acquire().await.map_err(error)?;
        let observed_remote = entries::remote_entry(&mut connection, local.binding, entry)
            .await
            .map_err(error)?;
        drop(connection);
        // Compare the fixed authenticated view with the durable event view, keeping baseline intact.
        if let Some(fixed) = current.get(&entry.to_string()) {
            let revision = vaultsyncd_domain::version::Revision::from_decimal(&fixed.revision)
                .map_err(error)?;
            if observed_remote
                .as_ref()
                .is_none_or(|r| r.revision != revision)
            {
                // A snapshot newer OR older than the received view cannot authorize an ACK.
                continue;
            }
            let expected = crate::work::baseline(fixed)?;
            if observed_remote.as_ref().is_some_and(|r| {
                r.deleted
                    || r.parent != expected.parent
                    || r.name != expected.name
                    || r.kind != expected.kind
                    || r.content_version != expected.content_version
            }) {
                return Err(error("rule reconciliation remote identity mismatch"));
            }
        } else {
            // A remote tombstone is not permission to discard retained or missing local
            // unconfirmed content. Leave deletion/conflict recovery explicitly blocked.
            continue;
        }
        let row = &current[&entry.to_string()];
        if explicit.is_some() && row.kind != vaultsyncd_protocol::generated::EntryKind::File {
            return Err(PortError::rejected(
                "INVALID_ARGUMENT",
                "显式下载恢复只接受文件",
            ));
        }
        let path = &names[&entry.to_string()];
        if !rules.includes(
            path,
            row.kind == vaultsyncd_protocol::generated::EntryKind::Directory,
        )? {
            continue;
        }
        let mut conn = local.db.pool().acquire().await.map_err(error)?;
        let before = entries::baseline_entry(&mut conn, local.binding, entry)
            .await
            .map_err(error)?;
        let previous = entries::local_entry(&mut conn, local.binding, entry)
            .await
            .map_err(error)?;
        drop(conn);
        if before.as_ref().is_some_and(|base| {
            base.parent.map(|id| id.to_string()) != row.parent_id || base.name != row.name
        }) {
            // Never abandon the old path of a renamed entry; its retained bytes need
            // namespace/conflict recovery before this new location can participate.
            continue;
        }
        let mut meta = match local.vault.inspect(path) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(error(e)),
        };
        if let Some(op) = operation {
            // Retained bytes still belong to the common baseline; explicit missing-file
            // restoration installs the authenticated snapshot head instead.
            let version = if meta.is_none() {
                row.version_id.clone()
            } else {
                before
                    .as_ref()
                    .and_then(|b| b.content_version)
                    .map(|v| v.to_string())
            };
            crate::importer::action(local, op, &serde_json::json!({"kind":"manual_download","entry":entry.to_string(),"path":path,"version":version})).await?;
        }
        if let Some(observed) = meta
            .as_ref()
            .filter(|m| m.cloud.as_ref().is_some_and(|c| c.on_disk < m.size))
        {
            let cloud = observed.cloud.as_ref().expect("partial");
            let identity =
                vaultsyncd_platform_windows::cfapi::catalog::Identity::decode(&cloud.identity)
                    .map_err(error)?;
            let old_version = before.as_ref().and_then(|b| b.content_version);
            let vault = worker.importer.remote.vault;
            let facts: Option<(Vec<u8>, i64)> = sqlx::query_as("SELECT digest,size FROM agent_versions WHERE binding_id=? AND entry_id=? AND version_id=?")
                .bind(local.binding.as_bytes().as_slice()).bind(entry.as_bytes().as_slice()).bind(identity.version.as_bytes().as_slice())
                .fetch_optional(local.db.pool()).await.map_err(error)?;
            let mut physical = observed.identity.volume.to_le_bytes().to_vec();
            physical.extend(observed.identity.file);
            if identity.binding != local.binding
                || identity.vault != vault
                || identity.entry != entry
                || Some(identity.version) != old_version
                || cloud.modified != 0
                || cloud.in_sync_state != 1
                || previous.as_ref().is_none_or(|p| {
                    p.dirty != Dirty::Clean || p.file_identity.as_ref() != Some(&physical)
                })
                || facts
                    .as_ref()
                    .is_none_or(|(_, size)| *size as u64 != observed.size)
            {
                if explicit.is_some() {
                    return Err(PortError::rejected(
                        "LOCAL_CONTENT_PENDING",
                        "部分占位身份、固定版本或未确认范围不符；保留内容，不能恢复覆盖",
                    ));
                }
                continue;
            }
            if row.version_id.as_deref() != Some(&identity.version.to_string()) {
                // Preserve downloaded old ranges by completing their original immutable version.
                // Never dehydrate or retag a partial file to the new server head.
                let (digest, size) = facts.expect("checked");
                let fixed = vaultsyncd_transfer::ports::VersionPin::new(
                    local.binding.to_string(),
                    entry.to_string(),
                    identity.version.to_string(),
                    digest
                        .try_into()
                        .map_err(|_| error("invalid fixed digest"))?,
                    size as u64,
                )
                .map_err(error)?;
                worker
                    .importer
                    .callbacks
                    .register_version(
                        crate::bootstrap::PinnedVersion::new(identity, fixed).map_err(error)?,
                    )
                    .map_err(error)?;
                crate::management_files::read_pinned(local, path, identity).await?;
                meta = Some(local.vault.inspect(path).map_err(error)?);
            }
        }
        let restoring = meta.is_none();
        let after_local;
        if let Some(meta) = meta.clone() {
            if meta.directory != (row.kind == vaultsyncd_protocol::generated::EntryKind::Directory)
                || before.is_none()
            {
                // Unknown/occupied paths are never proof of absence or an acknowledged
                // baseline. Keep the participation fence intact.
                continue;
            }
            if meta.cloud.as_ref().is_some_and(|c| c.on_disk < meta.size) {
                // Placeholder or interrupted hydration. Re-inclusion must not deadlock the
                // binding: a clean tracked row whose baseline is still the fixed remote
                // version resumes participation unchanged, and a later read completes that
                // same version. A dirty row or a remote change while excluded stays fenced.
                let same_version = before
                    .as_ref()
                    .and_then(|b| b.content_version)
                    .map(|v| v.to_string())
                    == row.version_id.clone();
                if !same_version || previous.as_ref().is_none_or(|p| p.dirty != Dirty::Clean) {
                    continue;
                }
                let pin = crate::importer::pin(local, row).await?;
                let id = vaultsyncd_platform_windows::cfapi::catalog::Identity {
                    binding: local.binding,
                    vault: worker
                        .importer
                        .adapter
                        .lock()
                        .map_err(error)?
                        .catalog()
                        .read()
                        .map_err(error)?
                        .vault,
                    entry,
                    version: vaultsyncd_domain::ids::VersionId::from_hex(&pin.version)
                        .map_err(error)?,
                };
                worker
                    .importer
                    .callbacks
                    .register_version(crate::bootstrap::PinnedVersion::new(id, pin).map_err(error)?)
                    .map_err(error)?;
                after_local = previous.clone();
            } else if !meta.directory {
                let snapshot = local
                    .observe_snapshot(path)
                    .await?
                    .ok_or_else(|| error("reconciliation file vanished"))?;
                let evidence = snapshot.evidence();
                let mut tx = local.db.begin_write().await.map_err(error)?;
                let old = entries::local_entry(&mut tx, local.binding, entry)
                    .await
                    .map_err(error)?;
                if old != previous {
                    continue;
                }
                // Compare against the common baseline, not the server head: equality with
                // a newer remote head does not prove an unconfirmed local edit committed.
                let unchanged: bool = if let Some(version) =
                    before.as_ref().and_then(|b| b.content_version)
                {
                    let facts: Option<(Vec<u8>, i64)> = sqlx::query_as("SELECT digest,size FROM agent_versions WHERE binding_id=? AND version_id=?")
                        .bind(local.binding.as_bytes().as_slice()).bind(version.as_bytes().as_slice())
                        .fetch_optional(&mut *tx).await.map_err(error)?;
                    facts.is_some_and(|(digest, size)| {
                        digest == evidence.digest && size as u64 == evidence.size
                    })
                } else {
                    false
                };
                after_local = Some(LocalEntry {
                    entry,
                    generation: Generation::new(evidence.generation).map_err(error)?,
                    dirty: match old.as_ref().map(|o| o.dirty) {
                        Some(dirty) if dirty != Dirty::Clean => dirty,
                        _ if unchanged => Dirty::Clean,
                        _ => Dirty::Dirty,
                    },
                    residency: Residency::Materialized,
                    fingerprint: Some(evidence.digest.to_vec()),
                    file_identity: Some(evidence.identity()),
                });
                entries::put_local(&mut tx, local.binding, after_local.as_ref().unwrap())
                    .await
                    .map_err(error)?;
                tx.commit().await.map_err(error)?;
            } else {
                after_local = previous;
            }
        } else {
            // Missing while excluded means restore the fixed remote entry, NOT local Delete.
            // Unknown/dirty/partial history stays blocked and is never silently overwritten.
            if previous
                .as_ref()
                .is_some_and(|p| p.dirty != Dirty::Clean || p.residency == Residency::Partial)
            {
                continue;
            }
            let mode: String = sqlx::query_scalar("SELECT mode FROM bindings WHERE id=?")
                .bind(local.binding.as_bytes().as_slice())
                .fetch_one(local.db.pool())
                .await
                .map_err(error)?;
            if before.is_some() && explicit != Some(entry) {
                // A tracked file that disappeared while excluded is a user-side deletion, not
                // proof that the server copy should be re-downloaded or deleted. Keep the
                // fence: explicit recovery decides, and nothing is silently restored or lost.
                continue;
            }
            if row.kind == vaultsyncd_protocol::generated::EntryKind::Directory {
                // Restore the directory itself first; children are ordered parent-first.
                worker
                    .importer
                    .install(&vaultsyncd_agent_core::bindings::import::ImportInstall {
                        op: crate::work::new_op(),
                        binding: local.binding,
                        path: RelativePath::parse(path).map_err(error)?,
                        remote: row.clone(),
                        mode: vaultsyncd_client_store::bindings::SyncMode::Full,
                    })
                    .await?;
                let mut conn = local.db.pool().acquire().await.map_err(error)?;
                after_local = entries::local_entry(&mut conn, local.binding, entry)
                    .await
                    .map_err(error)?;
                drop(conn);
            } else if mode == "on_demand" && explicit != Some(entry) {
                // An on-demand binding keeps a restored entry as a placeholder; exclusion
                // must not turn into an unexpected full download.
                worker
                    .importer
                    .install(&vaultsyncd_agent_core::bindings::import::ImportInstall {
                        op: crate::work::new_op(),
                        binding: local.binding,
                        path: RelativePath::parse(path).map_err(error)?,
                        remote: row.clone(),
                        mode: vaultsyncd_client_store::bindings::SyncMode::OnDemand,
                    })
                    .await?;
                let mut tx = local.db.begin_write().await.map_err(error)?;
                entries::put_baseline(&mut tx, local.binding, &crate::work::baseline(row)?)
                    .await
                    .map_err(error)?;
                tx.commit().await.map_err(error)?;
                let mut conn = local.db.pool().acquire().await.map_err(error)?;
                after_local = entries::local_entry(&mut conn, local.binding, entry)
                    .await
                    .map_err(error)?;
                drop(conn);
            } else {
                let spec = vaultsyncd_agent_core::executor::apply::ApplySpec {
                    op: operation.unwrap_or_else(crate::work::new_op),
                    binding: local.binding,
                    target: RelativePath::parse(path).map_err(error)?,
                    baseline_before: before.clone(),
                    baseline_after: crate::work::baseline(row)?,
                    pin: crate::importer::pin(local, row).await?,
                    expected: vaultsyncd_agent_core::recovery::ContentEvidence::Absent,
                };
                vaultsyncd_agent_core::executor::apply::ControlledApply::new(
                    local.db.clone(),
                    local.clone(),
                )
                .apply(
                    &spec,
                    &*worker.importer.content,
                    &crate::local::DiskTemp::new(&local.cache, spec.op),
                    &vaultsyncd_agent_core::adapters::ClientStorePorts::new(local.db.clone()),
                    &vaultsyncd_transfer::download::DownloadOptions::new(1024 * 1024),
                )
                .await?;
                worker.importer.adopt(path, &spec.pin)?;
                if local
                    .native
                    .publication_present(&spec.op.to_string())
                    .map_err(error)?
                {
                    local
                        .native
                        .acknowledge_publication(&spec.op.to_string())
                        .map_err(error)?;
                }
                let mut conn = local.db.pool().acquire().await.map_err(error)?;
                after_local = entries::local_entry(&mut conn, local.binding, entry)
                    .await
                    .map_err(error)?;
                drop(conn);
            }
        }
        if let Some(initial) = meta
            .as_ref()
            .filter(|m| m.cloud.as_ref().is_some_and(|c| c.on_disk < m.size))
        {
            let fresh = local.vault.inspect(path).map_err(error)?;
            if fresh.identity != initial.identity
                || fresh.size != initial.size
                || fresh.cloud != initial.cloud
            {
                continue;
            }
        }
        // Final CAS includes namespace and local evidence, not only the rule text/version.
        // It is a short DB transaction; no platform or network operation holds its write lock.
        let mut tx = local.db.begin_write().await.map_err(error)?;
        let fence: (String, String, i64, bool, bool) = sqlx::query_as("SELECT b.rules_version,cast(b.server_epoch AS TEXT),b.received_seq,coalesce(c.paused,0),coalesce(c.detached,0) FROM bindings b LEFT JOIN binding_controls c ON c.binding_id=b.id WHERE b.id=?")
            .bind(local.binding.as_bytes().as_slice()).fetch_one(&mut *tx).await.map_err(error)?;
        if fence.0 != rules.version
            || fence.1 != epoch
            || fence.2 != received
            || fence.3
            || fence.4
            || entries::remote_entry(&mut tx, local.binding, entry)
                .await
                .map_err(error)?
                != observed_remote
            || entries::local_entry(&mut tx, local.binding, entry)
                .await
                .map_err(error)?
                != after_local
        {
            continue;
        }
        // Restoration intentionally advances the baseline; existing retained bytes do not.
        let expected_base = if restoring {
            Some(crate::work::baseline(row)?)
        } else {
            before
        };
        if entries::baseline_entry(&mut tx, local.binding, entry)
            .await
            .map_err(error)?
            != expected_base
        {
            continue;
        }
        sqlx::query("DELETE FROM binding_participation WHERE binding_id=? AND entry_id=? AND state='reconcile_required'")
            .bind(local.binding.as_bytes().as_slice()).bind(entry.as_bytes().as_slice())
            .execute(&mut *tx).await.map_err(error)?;
        tx.commit().await.map_err(error)?;
    }
    Ok(())
}
