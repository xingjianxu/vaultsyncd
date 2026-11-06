//! Decode actual server transaction bodies and hand their durable tasks to the shared scheduler.
use crate::{
    importer::action,
    local::error,
    work::{Worker, paths},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use vaultsyncd_agent_core::{
    engine::{
        events::{ApplyResult, DecodeError, EventApplier, EventDecoder, EventEnvelope},
        scheduler::Scheduler,
    },
    ports::{PortError, PortFuture},
};
use vaultsyncd_client_store::{
    entries::RemoteEntry,
    events::IncomingBatch,
    queue::{self, NewPendingOp},
};
use vaultsyncd_domain::{
    error::ErrorCode,
    ids::{BindingId, EntryId, OpId, VersionId},
    version::Revision,
};
use vaultsyncd_protocol::generated::{EntryKind, SnapshotEntry};
pub(crate) struct Decoder {
    pub rows: Mutex<HashMap<String, SnapshotEntry>>,
    pub specifications: Mutex<HashMap<OpId, serde_json::Value>>,
}
impl EventDecoder for Decoder {
    fn decode(
        &self,
        binding: BindingId,
        envelope: &EventEnvelope,
    ) -> Result<IncomingBatch, DecodeError> {
        let invalid = |e: &dyn std::fmt::Display| DecodeError::new("events_json", e.to_string());
        let values: Vec<serde_json::Value> =
            serde_json::from_str(&envelope.events_json).map_err(|e| invalid(&e))?;
        if values.is_empty() {
            return Err(invalid(&"empty transaction"));
        }
        let mut rows = self.rows.lock().map_err(|e| invalid(&e))?;
        let mut specs = self.specifications.lock().map_err(|e| invalid(&e))?;
        let mut changes = Vec::new();
        let mut tasks = Vec::new();
        let mut previous = HashMap::new();
        for (index, value) in values.into_iter().enumerate() {
            let kind = value["kind"]
                .as_str()
                .ok_or_else(|| invalid(&"missing event kind"))?;
            if !matches!(
                kind,
                "external" | "put" | "create" | "move" | "delete" | "restore" | "conflict"
            ) {
                return Err(invalid(&format!("unsupported event {kind}")));
            }
            let id = value["entry_id"]
                .as_str()
                .ok_or_else(|| invalid(&"missing entry"))?;
            let entry = EntryId::from_hex(id).map_err(|e| invalid(&e))?;
            let prior = rows.get(id).cloned();
            let revision = value["revision"]
                .as_str()
                .ok_or_else(|| invalid(&"missing revision"))?;
            let namespace = paths(&rows).map_err(|e| invalid(&e))?;
            let path = value["path"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    let parent = value["parent_id"].as_str()?;
                    let name = value["name"].as_str()?;
                    let p = namespace.get(parent)?;
                    Some(if p.is_empty() {
                        name.into()
                    } else {
                        format!("{p}/{name}")
                    })
                })
                .or_else(|| namespace.get(id).cloned())
                .ok_or_else(|| invalid(&"unknown event path"))?;
            let parts: Vec<_> = path.rsplitn(2, '/').collect();
            let name = parts[0].to_owned();
            let parent_path = parts.get(1).copied().unwrap_or("");
            let parent = value["parent_id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    namespace
                        .iter()
                        .find(|(_, p)| p.as_str() == parent_path)
                        .map(|(id, _)| id.clone())
                })
                .or_else(|| prior.as_ref().and_then(|p| p.parent_id.clone()))
                .ok_or_else(|| invalid(&"unknown event parent"))?;
            let file = value["version_id"].is_string()
                || prior.as_ref().is_some_and(|r| r.kind == EntryKind::File);
            let row = SnapshotEntry {
                entry_id: id.into(),
                parent_id: Some(parent),
                name,
                kind: if file {
                    EntryKind::File
                } else {
                    EntryKind::Directory
                },
                revision: revision.into(),
                version_id: value["version_id"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| prior.as_ref().and_then(|r| r.version_id.clone())),
                hash: value["hash"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| prior.as_ref().and_then(|r| r.hash.clone())),
                size: value["size"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| prior.as_ref().and_then(|r| r.size.clone())),
            };
            let mut seed = envelope.tx.as_bytes().to_vec();
            seed.extend(entry.as_bytes());
            seed.extend((index as u64).to_le_bytes());
            let digest = vaultsyncd_transfer::hash::digest_of(&seed);
            let op = OpId::from_bytes(digest[..16].try_into().map_err(|e| invalid(&e))?);
            specs.insert(op,serde_json::json!({"kind":"apply","path":path,"remote":row,"deleted":kind=="delete"}));
            changes.push(RemoteEntry {
                entry,
                parent: row
                    .parent_id
                    .as_deref()
                    .map(EntryId::from_hex)
                    .transpose()
                    .map_err(|e| invalid(&e))?,
                name: row.name.clone(),
                kind: if file {
                    vaultsyncd_client_store::entries::EntryKind::File
                } else {
                    vaultsyncd_client_store::entries::EntryKind::Directory
                },
                revision: Revision::from_decimal(revision).map_err(|e| invalid(&e))?,
                content_version: row
                    .version_id
                    .as_deref()
                    .map(VersionId::from_hex)
                    .transpose()
                    .map_err(|e| invalid(&e))?,
                deleted: kind == "delete",
            });
            tasks.push(NewPendingOp {
                op,
                binding,
                kind: "apply_remote".into(),
                entry: Some(entry),
                baseline_revision: None,
                baseline_version: None,
                depends_on: previous.insert(entry, op),
                pinned_version: row
                    .version_id
                    .as_deref()
                    .map(VersionId::from_hex)
                    .transpose()
                    .map_err(|e| invalid(&e))?,
            });
            rows.insert(id.into(), row);
        }
        Ok(IncomingBatch {
            binding,
            seq: envelope.seq,
            tx: envelope.tx,
            events_json: envelope.events_json.clone(),
            changes,
            tasks,
        })
    }
}
pub(crate) struct Bridge {
    pub worker: Worker,
    pub scheduler: Arc<Scheduler<crate::work::Dispatch>>,
    pub decoder: Arc<Decoder>,
}
impl EventApplier for Bridge {
    fn apply(
        &self,
        binding: BindingId,
        _: &[RemoteEntry],
        tasks: &[NewPendingOp],
    ) -> PortFuture<'_, Result<ApplyResult, PortError>> {
        let tasks = tasks.to_vec();
        Box::pin(async move {
            if binding != self.worker.importer.local.binding {
                return Err(error("event binding mismatch"));
            }
            for task in &tasks {
                let value = self
                    .decoder
                    .specifications
                    .lock()
                    .map_err(error)?
                    .get(&task.op)
                    .cloned()
                    .ok_or_else(|| error("missing decoded action"))?;
                action(&self.worker.importer.local, task.op, &value).await?;
            }
            self.scheduler.run_round(binding).await?;
            let mut conn = self
                .worker
                .importer
                .local
                .db
                .pool()
                .acquire()
                .await
                .map_err(error)?;
            for task in &tasks {
                if queue::pending_op(&mut conn, task.op)
                    .await
                    .map_err(error)?
                    .is_some()
                {
                    return Ok(ApplyResult::Blocked(ErrorCode::NeedsRecovery));
                }
            }
            Ok(ApplyResult::Applied)
        })
    }
}
