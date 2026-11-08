//! Content-bearing Create: the same fixed-content publication and durable recovery fence as Put.
use crate::{
    executor::{ExecutorContext, SqlIntentLog, WorkTreeTarget},
    journal::Binding,
    uploads::random_bytes,
};
use vaultsyncd_domain::{
    ids::{EntryId, IntentId, TxId, UserId, VersionId},
    version::{Revision, UtcTimestamp},
};
use vaultsyncd_protocol::generated::{OperationResult, OperationState};
use vaultsyncd_server_core::{
    api::operations::{OperationFailure, OperationRequest},
    commit::intent::{NewPutIntent, TargetAfter, TargetBefore, publish_put},
    objects::ExpectedContent,
    ports::target::TargetPort,
};
use vaultsyncd_server_store::{
    db::Database,
    entries, intents,
    operations::{self, OperationKey, RequestDigest},
    versions,
};
fn failure(e: impl std::fmt::Display) -> OperationFailure {
    OperationFailure::Storage {
        detail: e.to_string(),
    }
}
/// Canonical Create recovery binding, shared by execution and durable-boundary acceptance probes.
pub fn recovery_binding(
    key: OperationKey,
    request: &OperationRequest,
    entry: EntryId,
    version: VersionId,
) -> Result<Binding, OperationFailure> {
    let parent = request
        .parent
        .ok_or_else(|| failure("missing Create parent"))?;
    let name = request
        .name
        .as_ref()
        .ok_or_else(|| failure("missing Create name"))?;
    Ok(Binding::new(
        key,
        "put",
        serde_json::json!({"create_file":true,"request_fields":request.digest_fields(),"entry":entry.to_string(),"parent":parent.to_string(),"name":name,"version":version.to_string(),"local_generation":request.local_generation.map(|g|g.to_string())}),
    ))
}
/// Validate the persisted Create binding before recovery is allowed to touch a filesystem target.
pub async fn validate(
    db: &Database,
    owner: UserId,
    binding: &Binding,
    digest: RequestDigest,
    put: &vaultsyncd_server_core::commit::intent::PutIntent,
) -> Result<(), OperationFailure> {
    let fields: Vec<(String, String)> =
        serde_json::from_value(binding.context["request_fields"].clone()).map_err(failure)?;
    let pairs: Vec<(&str, &[u8])> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_bytes()))
        .collect();
    if operations::request_digest(&pairs).map_err(failure)? != digest
        || binding.context["create_file"] != true
        || put.before != TargetBefore::Absent
        || put.root != binding.root
    {
        return Err(failure("invalid bound Create semantics"));
    }
    for key in ["parent_id", "name"] {
        let value = fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str());
        let expected = binding.context[if key == "parent_id" { "parent" } else { "name" }].as_str();
        if value != expected {
            return Err(failure("Create namespace context changed"));
        }
    }
    let parent = EntryId::from_hex(
        binding.context["parent"]
            .as_str()
            .ok_or_else(|| failure("missing parent"))?,
    )
    .map_err(failure)?;
    let upload = vaultsyncd_domain::ids::UploadId::from_hex(
        fields
            .iter()
            .find(|(k, _)| k == "upload_id")
            .map(|(_, v)| v.as_str())
            .ok_or_else(|| failure("missing upload identity"))?,
    )
    .map_err(failure)?;
    let mut conn = db.pool().acquire().await.map_err(failure)?;
    let path = entries::path_of(&mut conn, owner, parent)
        .await
        .map_err(failure)?
        .ok_or_else(|| failure("missing Create parent"))?;
    let name = binding.context["name"]
        .as_str()
        .ok_or_else(|| failure("missing name"))?;
    if put.target_path
        != if path.is_empty() {
            name.into()
        } else {
            format!("{path}/{name}")
        }
    {
        return Err(failure("Create target path mismatch"));
    }
    let source = vaultsyncd_server_store::uploads::upload_in_scope(&mut conn, owner, upload)
        .await
        .map_err(failure)?
        .ok_or_else(|| failure("missing finalized Create source"))?;
    if source.root != binding.root
        || source.state != vaultsyncd_server_store::uploads::UploadState::Finalized
        || source.hash != *put.after.digest.as_bytes()
        || source.expected_size != put.after.size
    {
        return Err(failure("Create content is not its finalized upload"));
    }
    Ok(())
}
/// Execute a finalized-content Create without ever publishing an empty placeholder on the server.
pub async fn execute(
    executor: &ExecutorContext,
    owner: UserId,
    key: OperationKey,
    digest: RequestDigest,
    request: &OperationRequest,
    content: ExpectedContent,
    at: UtcTimestamp,
) -> Result<OperationResult, OperationFailure> {
    let parent = request.parent.ok_or_else(|| failure("missing parent"))?;
    let name = request
        .name
        .as_ref()
        .ok_or_else(|| failure("missing name"))?;
    let _gate = executor.commit_gate().lock().await;
    executor.ensure_no_unfinished(owner, key.root).await?;
    let parent_path = crate::commands::path_of(executor, owner, parent).await?;
    let path = if parent_path.is_empty() {
        name.clone()
    } else {
        format!("{parent_path}/{name}")
    };
    {
        let mut conn = executor
            .database()
            .pool()
            .acquire()
            .await
            .map_err(failure)?;
        let p = entries::entry_in_scope(&mut conn, owner, parent)
            .await
            .map_err(failure)?
            .ok_or_else(|| failure("parent missing"))?;
        if p.root != key.root || p.kind != entries::EntryKind::Directory || p.deleted_at.is_some() {
            return Err(OperationFailure::InvalidRequest {
                detail: "invalid Create parent".into(),
            });
        }
        if entries::active_child_by_name(&mut conn, owner, key.root, parent, name)
            .await
            .map_err(failure)?
            .is_some()
        {
            return Err(OperationFailure::BaseMismatch {
                detail: "Create expected_absent target occupied".into(),
            });
        }
        let root = vaultsyncd_server_store::roots::root_in_scope(&mut conn, owner, key.root)
            .await
            .map_err(failure)?
            .ok_or(OperationFailure::RootUnavailable)?;
        if root.epoch != request.expected_epoch {
            return Err(OperationFailure::EpochChanged {
                expected: request.expected_epoch,
                actual: root.epoch,
            });
        }
    }
    let handle = executor.root_handle(owner, key.root).await?;
    let mut target =
        WorkTreeTarget::new(handle, executor.objects(key.root, &[content.digest]).await?);
    if !matches!(
        target.observe(&path).map_err(failure)?,
        vaultsyncd_server_core::ports::target::TargetObservation::Absent
    ) {
        return Err(OperationFailure::ReconcileRequired {
            detail: "Create physical target occupied".into(),
        });
    }
    executor.begin_operation(owner, key, digest).await?;
    let entry = EntryId::from_bytes(random_bytes());
    let version = VersionId::from_bytes(random_bytes());
    let intent = IntentId::from_bytes(random_bytes());
    let binding = recovery_binding(key, request, entry, version)?;
    let mut log = SqlIntentLog::new(executor.database().clone(), owner).bound(binding.clone());
    let new = NewPutIntent {
        intent_id: intent,
        root: key.root,
        target_path: path.clone(),
        staging_path: format!("create-{intent}"),
        before: TargetBefore::Absent,
        after: TargetAfter {
            digest: content.digest,
            size: content.bytes,
        },
    };
    let published = publish_put(&mut log, &mut target, at, &new)
        .await
        .map_err(|e| crate::executor::map_put_failure(&e))?;
    let result = finish(
        executor.database(),
        owner,
        &binding,
        digest,
        published.intent.intent_id,
        content,
        at,
    )
    .await?;
    executor
        .note_own_content(key.root, &path, published.target.identity, content)
        .await;
    Ok(result)
}
/// Complete only the metadata half of a published, bound Create; used by startup replay too.
pub async fn finish(
    db: &Database,
    owner: UserId,
    binding: &Binding,
    digest: RequestDigest,
    intent: IntentId,
    content: ExpectedContent,
    at: UtcTimestamp,
) -> Result<OperationResult, OperationFailure> {
    let id = |name: &str| {
        binding.context[name]
            .as_str()
            .ok_or_else(|| failure("malformed Create context"))
    };
    let entry = EntryId::from_hex(id("entry")?).map_err(failure)?;
    let parent = EntryId::from_hex(id("parent")?).map_err(failure)?;
    let version = VersionId::from_hex(id("version")?).map_err(failure)?;
    let name = id("name")?;
    if binding.context["create_file"] != true {
        return Err(failure("not a content Create context"));
    }
    let mut tx = db.begin_write().await.map_err(failure)?;
    crate::journal::ensure_digest(&mut tx, owner, &binding.key(), &digest)
        .await
        .map_err(failure)?;
    let stored = intents::intent_in_scope(&mut tx, owner, intent)
        .await
        .map_err(failure)?
        .ok_or_else(|| failure("missing Create intent"))?;
    if stored.root != binding.root || stored.phase != intents::FsIntentPhase::Published {
        return Err(failure("Create intent is not published"));
    }
    let put = crate::executor::put_intent_of(stored).map_err(failure)?;
    validate(db, owner, binding, digest, &put).await?;
    if put.before != TargetBefore::Absent
        || put.after.digest != content.digest
        || put.after.size != content.bytes
    {
        return Err(failure("Create context/intent mismatch"));
    }
    entries::insert_entry(
        &mut tx,
        owner,
        &entries::NewEntry {
            id: entry,
            root: binding.root,
            parent,
            name: name.into(),
            name_key: name.into(),
            kind: entries::EntryKind::File,
            revision: Revision::ZERO,
            content_version: None,
        },
        at,
    )
    .await
    .map_err(failure)?;
    let result = OperationResult {
        op_id: binding.op.to_string(),
        state: OperationState::Committed,
        entry_id: Some(entry.to_string()),
        version_id: Some(version.to_string()),
        revision: Some("0".into()),
        root_seq: None,
        replayed: None,
        conflict: None,
        error_code: None,
        error_message: None,
        echo_local_generation: binding.context["local_generation"]
            .as_str()
            .map(str::to_owned),
    };
    let events = crate::executor::events_json(
        "put",
        entry,
        Some(&put.target_path),
        Revision::ZERO,
        Some((version, content.digest, content.bytes)),
        "client",
    );
    let committed = operations::commit_operation(
        &mut tx,
        owner,
        &binding.key(),
        TxId::from_bytes(random_bytes()),
        &events,
        &serde_json::to_string(&result).map_err(failure)?,
        at,
    )
    .await
    .map_err(failure)?;
    versions::record_version(
        &mut tx,
        owner,
        &versions::NewVersion {
            id: version,
            root: binding.root,
            entry,
            object_hash: *content.digest.as_bytes(),
            size: content.bytes,
            created_seq: committed.batch.seq,
            origin: versions::VersionOrigin::Client,
            at,
        },
    )
    .await
    .map_err(failure)?;
    intents::advance_intent_phase(
        &mut tx,
        owner,
        intent,
        intents::FsIntentPhase::Finalized,
        at,
    )
    .await
    .map_err(failure)?;
    tx.commit().await.map_err(failure)?;
    let mut result = result;
    result.root_seq = Some(committed.batch.seq.to_string());
    Ok(result)
}
