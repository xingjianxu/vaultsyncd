//! Binding rules gate durable tasks by their actual paths, and preserve the old three-way baseline.
//! Rule removal first reads a complete authenticated snapshot and observes retained local bytes;
//! ordinary revision/generation-fenced event and upload executors then perform reconciliation.
use crate::{
    local::error,
    work::{Worker, metadata, paths},
};
use std::sync::Arc;
use vaultsyncd_agent_core::{
    bindings::{
        control::{BindingControls, ExecutionGate},
        import::ImportSnapshot,
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
    pub async fn load(db: &Database, binding: BindingId) -> Result<Self, PortError> {
        let (version,text,paused,detached):(String,String,bool,bool)=sqlx::query_as("SELECT b.rules_version,coalesce(c.rules,''),coalesce(c.paused,0),coalesce(c.detached,0) FROM bindings b LEFT JOIN binding_controls c ON c.binding_id=b.id WHERE b.id=?")
            .bind(binding.as_bytes().as_slice()).fetch_one(db.pool()).await.map_err(error)?;
        Ok(Self {
            version,
            rules: IgnoreRules::parse(&text).map_err(error)?,
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

pub(crate) struct Gate(pub Arc<Database>);
impl DispatchGate for Gate {
    fn allowed<'a>(&'a self, op: &'a QueuedOperation) -> PortFuture<'a, Result<bool, PortError>> {
        Box::pin(async move {
            let rules = Rules::load(&self.0, op.binding).await?;
            if !rules.active {
                return Ok(false);
            }
            if op.kind == "user_read" {
                return Ok(BindingControls::new(self.0.clone())
                    .execution_gate(op.binding, op.entry)
                    .await?
                    == ExecutionGate::Ready);
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
            // Directory matching is conservative for namespace/new tasks whose final kind is unknown.
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
    let local = &worker.importer.local;
    let pending:Vec<Vec<u8>>=sqlx::query_scalar("SELECT entry_id FROM binding_participation WHERE binding_id=? AND state='reconcile_required'")
        .bind(local.binding.as_bytes().as_slice()).fetch_all(local.db.pool()).await.map_err(error)?;
    if pending.is_empty() {
        return Ok(());
    }
    let rules = Rules::load(&local.db, local.binding).await?;
    if !rules.active {
        return Ok(());
    }
    // An unavailable/incomplete remote cannot authorize old local files. Do not reuse first import.
    let remote = &worker.importer.remote;
    let seed = remote.begin(remote.root).await?;
    let mut cursor = None;
    let mut count = 0u64;
    let mut current = std::collections::HashMap::new();
    loop {
        let page = remote
            .page(remote.root, &seed.metadata.snapshot_id, cursor.as_deref())
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
    if (received as u64) < seed.metadata.high_water_seq.parse::<u64>().map_err(error)? {
        return Ok(());
    }
    let rows = metadata(&worker.importer).await?;
    let names = paths(&rows)?;
    for raw in pending {
        let entry = EntryId::from_bytes(
            raw.try_into()
                .map_err(|_| error("invalid participation identity"))?,
        );
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
                .is_none_or(|r| r.revision < revision)
            {
                continue;
            }
            if let Some(observed) = &observed_remote {
                if observed.revision == revision
                    && (observed.deleted
                        || observed.content_version.map(|v| v.to_string()) != fixed.version_id
                        || observed.name != fixed.name)
                {
                    return Err(error("rule reconciliation remote identity mismatch"));
                }
            }
        } else if observed_remote.as_ref().is_none_or(|r| !r.deleted) {
            // Missing snapshot rows are deletions only with an authenticated received tombstone.
            continue;
        }
        let Some(row) = rows.get(&entry.to_string()) else {
            continue;
        };
        let path = &names[&entry.to_string()];
        if !rules.includes(
            path,
            row.kind == vaultsyncd_protocol::generated::EntryKind::Directory,
        )? {
            continue;
        }
        let meta = match local.root.inspect(path) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(error(e)),
        };
        if let Some(meta) = meta {
            if !meta.directory && !meta.cloud.as_ref().is_some_and(|c| c.on_disk < meta.size) {
                let snapshot = local
                    .observe_snapshot(path)
                    .await?
                    .ok_or_else(|| error("reconciliation file vanished"))?;
                let evidence = snapshot.evidence();
                let mut tx = local.db.begin_write().await.map_err(error)?;
                let old = entries::local_entry(&mut tx, local.binding, entry)
                    .await
                    .map_err(error)?;
                let unchanged = row.hash.as_deref()
                    == Some(vaultsyncd_transfer::hash::to_hex(&evidence.digest).as_str())
                    && row.size.as_deref() == Some(evidence.size.to_string().as_str());
                entries::put_local(
                    &mut tx,
                    local.binding,
                    &LocalEntry {
                        entry,
                        generation: Generation::new(evidence.generation).map_err(error)?,
                        dirty: if unchanged && old.as_ref().is_none_or(|o| o.dirty == Dirty::Clean)
                        {
                            Dirty::Clean
                        } else {
                            Dirty::Dirty
                        },
                        residency: Residency::Materialized,
                        fingerprint: Some(evidence.digest.to_vec()),
                        file_identity: Some(evidence.identity()),
                    },
                )
                .await
                .map_err(error)?;
                tx.commit().await.map_err(error)?;
            }
        }
        BindingControls::new(local.db.clone())
            .acknowledge_reconciled(local.binding, entry, &rules.version)
            .await?;
    }
    Ok(())
}
