//! Consume committed preserve-both ACKs using fixed server identities, never guessed copy names.
use crate::{
    importer::{Importer, action, pin},
    local::{DiskTemp, Local, error},
    work::{baseline, load_spec, paths},
};
use std::collections::HashMap;
use vaultsyncd_agent_core::{
    adapters::ClientStorePorts,
    executor::{
        apply::{ApplySpec, ControlledApply},
        upload::UploadCommitDisposition,
    },
    ports::PortError,
    recovery::{self, ContentEvidence},
};
use vaultsyncd_client_store::entries;
use vaultsyncd_domain::{ids::OpId, version::Generation};
use vaultsyncd_protocol::generated::{OperationResult, OperationState, SnapshotEntry};
use vaultsyncd_transfer::download::DownloadOptions;

fn child(op: OpId, role: &str) -> OpId {
    let digest = vaultsyncd_transfer::hash::digest_of(format!("S070:{op}:{role}").as_bytes());
    OpId::from_bytes(digest[..16].try_into().expect("digest prefix"))
}
pub(crate) async fn record_ack(
    local: &Local,
    op: OpId,
    result: &OperationResult,
) -> Result<(), PortError> {
    if result.op_id != op.to_string() || result.state != OperationState::Committed {
        return Err(error(
            "only the original committed operation may be acknowledged",
        ));
    }
    let mut tx = local.db.begin_write().await.map_err(error)?;
    let old: Option<String> =
        sqlx::query_scalar("SELECT result FROM agent_actions WHERE op_id=? AND binding_id=?")
            .bind(op.as_bytes().as_slice())
            .bind(local.binding.as_bytes().as_slice())
            .fetch_one(&mut *tx)
            .await
            .map_err(error)?;
    if let Some(old) = old {
        let mut old: OperationResult = serde_json::from_str(&old).map_err(error)?;
        old.replayed = result.replayed;
        if old != *result {
            return Err(error("committed ACK changed for the same operation"));
        }
    } else {
        sqlx::query(
            "UPDATE agent_actions SET result=? WHERE op_id=? AND binding_id=? AND result IS NULL",
        )
        .bind(serde_json::to_string(result).map_err(error)?)
        .bind(op.as_bytes().as_slice())
        .bind(local.binding.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .map_err(error)?;
    }
    tx.commit().await.map_err(error)
}
pub(crate) async fn settle(
    importer: &Importer,
    op: OpId,
    generation: Generation,
    digest: [u8; 32],
    size: u64,
    result: &OperationResult,
) -> Result<UploadCommitDisposition, PortError> {
    let local = &importer.local;
    let conflict = result
        .conflict
        .as_ref()
        .ok_or_else(|| error("missing conflict ACK"))?;
    let spec = load_spec(local, op).await?;
    let source = spec["path"]
        .as_str()
        .ok_or_else(|| error("missing conflict source"))?;
    let key = child(op, "resolution");
    let old: Option<String> = sqlx::query_scalar(
        "SELECT specification FROM agent_actions WHERE op_id=? AND binding_id=?",
    )
    .bind(key.as_bytes().as_slice())
    .bind(local.binding.as_bytes().as_slice())
    .fetch_optional(local.db.pool())
    .await
    .map_err(error)?;
    let facts: serde_json::Value = if let Some(old) = old {
        serde_json::from_str(&old).map_err(error)?
    } else {
        let rows: HashMap<_, _> = crate::namespace::remote_snapshot(&importer.remote)
            .await?
            .into_iter()
            .map(|r| (r.entry_id.clone(), r))
            .collect();
        let locations = paths(&rows)?;
        let copy = rows
            .get(&conflict.conflict_entry_id)
            .ok_or_else(|| error("committed conflict copy missing from fixed snapshot"))?;
        let head = rows
            .get(&conflict.entry_id)
            .ok_or_else(|| error("conflict source missing from fixed snapshot"))?;
        let value = serde_json::json!({"kind":"conflict_resolution","copy":copy,"head":head,"copy_path":locations[&copy.entry_id],"head_path":locations[&head.entry_id]});
        action(local, key, &value).await?
    };
    let copy: SnapshotEntry = serde_json::from_value(facts["copy"].clone()).map_err(error)?;
    let head: SnapshotEntry = serde_json::from_value(facts["head"].clone()).map_err(error)?;
    if copy.entry_id != conflict.conflict_entry_id
        || head.entry_id != conflict.entry_id
        || copy.hash.as_deref() != Some(&vaultsyncd_transfer::hash::to_hex(&digest))
        || copy.size.as_deref() != Some(&size.to_string())
    {
        return Err(error(
            "fixed conflict copy does not confirm the uploaded bytes",
        ));
    }
    let copy_path = facts["copy_path"]
        .as_str()
        .ok_or_else(|| error("missing canonical conflict path"))?;
    install(importer, child(op, "copy"), &copy, copy_path, None, None).await?;
    let head_path = facts["head_path"]
        .as_str()
        .ok_or_else(|| error("missing canonical source path"))?;
    if head_path != source {
        return Ok(UploadCommitDisposition::StaleGeneration);
    }
    let current = local.observe_snapshot(source).await?;
    let Some(current) = current else {
        return Ok(UploadCommitDisposition::StaleGeneration);
    };
    let e = current.evidence();
    let head_pin = pin(local, &head).await?;
    let uploaded = e.generation == generation.get() && e.digest == digest && e.size == size;
    let restored = e.digest == head_pin.digest && e.size == head_pin.size;
    if !uploaded && !restored {
        // The old generation is confirmed under the copy identity; a later local save remains dirty.
        return Ok(UploadCommitDisposition::StaleGeneration);
    }
    install(
        importer,
        child(op, "head"),
        &head,
        head_path,
        Some(ContentEvidence::File {
            generation: e.generation,
            identity: e.identity(),
            digest: e.digest,
            size: e.size,
        }),
        Some(child(op, "copy")),
    )
    .await?;
    Ok(UploadCommitDisposition::Applied)
}
async fn install(
    importer: &Importer,
    op: OpId,
    row: &SnapshotEntry,
    path: &str,
    allowed: Option<ContentEvidence>,
    confirmed_copy: Option<OpId>,
) -> Result<(), PortError> {
    let local = &importer.local;
    let fixed = pin(local, row).await?;
    let controlled = ControlledApply::new(local.db.clone(), local.clone());
    let spec = if let Some(old) = controlled.specification(op).await? {
        old
    } else {
        let target = vaultsyncd_fs_common::path::RelativePath::parse(path).map_err(error)?;
        let actual = recovery::inspect(&**local, &target).await?;
        if actual != ContentEvidence::Absent
            && !actual.has_content(&fixed.digest, fixed.size)
            && allowed.as_ref() != Some(&actual)
        {
            return Err(error(
                "canonical conflict destination occupied by unconfirmed content",
            ));
        }
        let after = baseline(row)?;
        let mut conn = local.db.pool().acquire().await.map_err(error)?;
        let before = entries::baseline_entry(&mut conn, local.binding, after.entry)
            .await
            .map_err(error)?;
        drop(conn);
        ApplySpec {
            op,
            binding: local.binding,
            target,
            baseline_before: before,
            baseline_after: after,
            pin: fixed.clone(),
            expected: actual,
        }
    };
    if spec.binding != local.binding || spec.pin != fixed || spec.target.to_string() != path {
        return Err(error("persisted conflict publication differs"));
    }
    let temp = DiskTemp::new(&local.cache, op);
    let progress = ClientStorePorts::new(local.db.clone());
    let options = DownloadOptions::new(1024 * 1024);
    if let Some(copy) = confirmed_copy {
        controlled
            .apply_after_confirmed_copy(&spec, &*importer.content, &temp, &progress, &options, copy)
            .await?;
    } else {
        controlled
            .apply(&spec, &*importer.content, &temp, &progress, &options)
            .await?;
    }
    importer.adopt(path, &fixed)?;
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
