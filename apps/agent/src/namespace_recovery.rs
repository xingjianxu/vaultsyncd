//! Finish only application-owned Move/Import intents before generic core recovery is admitted.
use super::*;
use std::sync::Arc;
use vaultsyncd_client_store::intents::{self, IntentState, LocalIntentKind};
use vaultsyncd_platform_windows::cfapi::{adapter::Adapter, catalog::Identity};
pub(crate) async fn recover(
    local: &Arc<crate::local::Local>,
    adapter: &Adapter,
) -> Result<(), PortError> {
    let mut conn = local.db.pool().acquire().await.map_err(error)?;
    let pending = intents::intents_for_binding(&mut conn, local.binding)
        .await
        .map_err(error)?;
    drop(conn);
    for intent in pending {
        if !matches!(intent.kind, LocalIntentKind::Move | LocalIntentKind::Import) {
            continue;
        }
        if intent.state != IntentState::Pending {
            return Err(error("failed namespace intent requires recovery"));
        }
        let id = intent
            .entry
            .ok_or_else(|| error("namespace intent missing entry"))?;
        let raw: Vec<(Vec<u8>, String)> =
            sqlx::query_as("SELECT op_id,specification FROM agent_actions WHERE binding_id=?")
                .bind(local.binding.as_bytes().as_slice())
                .fetch_all(local.db.pool())
                .await
                .map_err(error)?;
        let mut candidates = Vec::new();
        for (op, text) in raw {
            let value: serde_json::Value = serde_json::from_str(&text).map_err(error)?;
            if matches!(value["kind"].as_str(), Some("apply" | "import_directory"))
                && value["deleted"] != true
                && value["path"] == intent.target_path
                && value["remote"]["entry_id"] == id.to_string()
            {
                candidates.push((
                    OpId::from_bytes(
                        op.try_into()
                            .map_err(|_| error("bad namespace operation ID"))?,
                    ),
                    serde_json::from_value::<SnapshotEntry>(value["remote"].clone())
                        .map_err(error)?,
                ));
            }
        }
        candidates.sort_by_key(|(_, row)| row.revision.parse::<u64>().unwrap_or(u64::MAX));
        candidates.dedup_by(|a, b| a == b);
        if candidates.len() != 1 {
            return Err(error("namespace intent has no unique durable action"));
        }
        let (op, row) = candidates.remove(0);
        let after = baseline(&row)?;
        let local_after = if intent.kind == LocalIntentKind::Import {
            if after.kind != EntryKind::Directory
                || intent.source_path.is_some()
                || intent.content_hash.is_some()
            {
                return Err(error("invalid directory import intent"));
            }
            let meta = local
                .vault
                .ensure_directory(&intent.target_path)
                .map_err(error)?;
            Some(LocalEntry {
                entry: id,
                generation: Generation::ZERO,
                dirty: Dirty::Clean,
                residency: Residency::Materialized,
                fingerprint: None,
                file_identity: Some(physical(&meta)),
            })
        } else {
            let from = intent
                .source_path
                .as_deref()
                .ok_or_else(|| error("move intent source missing"))?;
            let mut conn = local.db.pool().acquire().await.map_err(error)?;
            let before = entries::baseline_entry(&mut conn, local.binding, id)
                .await
                .map_err(error)?
                .ok_or_else(|| error("move recovery baseline missing"))?;
            let state = entries::local_entry(&mut conn, local.binding, id)
                .await
                .map_err(error)?
                .ok_or_else(|| error("move recovery physical state missing"))?;
            drop(conn);
            if before.revision >= after.revision
                || before.content_version != after.content_version
                || Some(state.generation) != intent.generation
                || state.fingerprint != intent.content_hash
            {
                return Err(error("move recovery durable fence changed"));
            }
            let expected = state
                .file_identity
                .clone()
                .ok_or_else(|| error("move recovery physical identity missing"))?;
            let destination = local.vault.inspect(&intent.target_path);
            if !destination
                .as_ref()
                .is_ok_and(|meta| physical(meta) == expected)
            {
                if !destination
                    .as_ref()
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                {
                    return Err(error("move recovery destination occupied or unknown"));
                }
                let source = local.vault.inspect(from).map_err(error)?;
                if physical(&source) != expected {
                    return Err(error("move recovery source changed"));
                }
                if source.directory {
                    local
                        .vault
                        .move_directory(from, &intent.target_path, source.identity)
                        .map_err(error)?;
                } else if let Some(cloud) =
                    source.cloud.as_ref().filter(|c| c.on_disk < source.size)
                {
                    let identity = Identity::decode(&cloud.identity).map_err(error)?;
                    if identity.binding != local.binding
                        || identity.entry != id
                        || Some(identity.version) != before.content_version
                    {
                        return Err(error("move recovery placeholder identity differs"));
                    }
                    adapter
                        .catalog()
                        .write()
                        .map_err(error)?
                        .insert(identity)
                        .map_err(error)?;
                    adapter
                        .move_entry(from, &intent.target_path, &source)
                        .map_err(error)?;
                } else {
                    let snapshot = local
                        .observe_snapshot(from)
                        .await?
                        .ok_or_else(|| error("move recovery source vanished"))?;
                    let e = snapshot.evidence();
                    if Some(e.generation) != intent.generation.map(|g| g.get())
                        || intent.content_hash.as_deref() != Some(e.digest.as_slice())
                    {
                        return Err(error("move recovery source generation/content changed"));
                    }
                    local
                        .native
                        .preserve(from, &intent.target_path, e)
                        .map_err(error)?;
                }
            }
            None
        };
        let mut tx = local.db.begin_write().await.map_err(error)?;
        entries::put_baseline(&mut tx, local.binding, &after)
            .await
            .map_err(error)?;
        if let Some(row) = local_after {
            entries::put_local(&mut tx, local.binding, &row)
                .await
                .map_err(error)?;
        }
        if !intents::complete_intent(&mut tx, intent.id)
            .await
            .map_err(error)?
        {
            return Err(error("namespace recovery intent disappeared"));
        }
        // The recovered native result and baseline prove completion of this exact durable action.
        // Release dependencies in the same transaction, preserving every dependent operation.
        sqlx::query("UPDATE pending_ops SET depends_on=NULL WHERE depends_on=?")
            .bind(op.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .map_err(error)?;
        queue::complete(&mut tx, op).await.map_err(error)?;
        tx.commit().await.map_err(error)?;
    }
    Ok(())
}
