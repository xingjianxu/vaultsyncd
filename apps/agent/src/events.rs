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
        let mut rows = self.rows.lock().map_err(|e| invalid(&e))?.clone();
        let mut specs = HashMap::new();
        let mut changes = Vec::new();
        let mut tasks = Vec::new();
        let mut previous = HashMap::new();
        for (index, value) in values.into_iter().enumerate() {
            let kind = value["kind"]
                .as_str()
                .ok_or_else(|| invalid(&"missing event kind"))?;
            if !matches!(
                kind,
                "external"
                    | "put"
                    | "update"
                    | "create"
                    | "move"
                    | "delete"
                    | "restore"
                    | "conflict"
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
                modified_at: None,
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
        if tasks
            .iter()
            .all(|task| specs.get(&task.op).is_some_and(|v| v["deleted"] == true))
        {
            tasks.sort_by_key(|task| {
                std::cmp::Reverse(
                    specs[&task.op]["path"]
                        .as_str()
                        .unwrap_or("")
                        .matches('/')
                        .count(),
                )
            });
            let mut dependency = None;
            for task in &mut tasks {
                task.depends_on = dependency;
                dependency = Some(task.op);
            }
        }
        self.specifications
            .lock()
            .map_err(|e| invalid(&e))?
            .extend(specs);
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
            let mut durable_rows = Vec::new();
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
                durable_rows.push(
                    serde_json::from_value::<SnapshotEntry>(value["remote"].clone())
                        .map_err(error)?,
                );
            }
            {
                let mut rows = self.decoder.rows.lock().map_err(error)?;
                for row in durable_rows {
                    rows.insert(row.entry_id.clone(), row);
                }
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

/// Optional wake stream. Drop aborts the connection; errors only reconnect, never update state.
pub(crate) struct Hints(tokio::task::JoinHandle<()>);
impl Hints {
    pub fn start(remote: Arc<crate::remote::Remote>, wake: tokio::sync::mpsc::Sender<()>) -> Self {
        Self(tokio::spawn(async move {
            loop {
                let _ = remote
                    .client
                    .receive_hints(&remote.vault.to_string(), &wake)
                    .await;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }))
    }
}
impl Drop for Hints {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_tail_and_unpersisted_good_batch_never_publish_namespace() {
        let vault = "11111111111111111111111111111111";
        let child = "22222222222222222222222222222222";
        let row = SnapshotEntry {
            modified_at: None,
            entry_id: vault.into(),
            parent_id: None,
            name: String::new(),
            kind: EntryKind::Directory,
            revision: "0".into(),
            version_id: None,
            hash: None,
            size: None,
        };
        let initial = HashMap::from([(vault.into(), row)]);
        let decoder = Decoder {
            rows: Mutex::new(initial.clone()),
            specifications: Mutex::new(HashMap::new()),
        };
        let mut envelope=EventEnvelope{seq:vaultsyncd_domain::version::VaultSeq::new(1).unwrap(),tx:vaultsyncd_domain::ids::TxId::from_bytes([3;16]),events_json:serde_json::json!([{"kind":"create","entry_id":child,"parent_id":vault,"name":"newdir","revision":"0"},{"kind":"create","parent_id":vault,"name":"bad","revision":"0"}]).to_string()};
        assert!(
            decoder
                .decode(BindingId::from_bytes([4; 16]), &envelope)
                .is_err()
        );
        assert_eq!(*decoder.rows.lock().unwrap(), initial);
        assert!(decoder.specifications.lock().unwrap().is_empty());
        envelope.events_json=serde_json::json!([{"kind":"create","entry_id":child,"parent_id":vault,"name":"newdir","revision":"0"}]).to_string();
        assert!(
            decoder
                .decode(BindingId::from_bytes([4; 16]), &envelope)
                .is_ok()
        );
        assert_eq!(*decoder.rows.lock().unwrap(), initial);
    }
}
