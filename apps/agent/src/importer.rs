//! First-import production ports and immutable version registry.
use crate::{
    bootstrap::{HydrationCallbacks, PinnedVersion},
    local::{DiskTemp, Local, error},
};
use std::sync::{Arc, Mutex};
use vaultsyncd_agent_core::{
    adapters::ClientStorePorts,
    bindings::import::*,
    executor::apply::{ApplySpec, ControlledApply},
};
use vaultsyncd_client_store::{
    bindings::{BindingRecord, SyncMode},
    entries::{self, LocalEntry},
};
use vaultsyncd_domain::{
    ids::{BindingId, EntryId, OpId, VersionId},
    state::{Dirty, Residency},
    version::Generation,
};
use vaultsyncd_fs_common::{name::NameEquivalence, path::RelativePath};
use vaultsyncd_platform_windows::{
    cfapi::{adapter::Adapter, catalog::Identity},
    files::NativeNames,
};
use vaultsyncd_protocol::generated::{EntryKind, SnapshotEntry};
use vaultsyncd_transfer::{
    download::DownloadOptions,
    ports::{PortError, PortFuture, VersionPin, VersionedContent},
};

pub(crate) async fn pin(local: &Local, row: &SnapshotEntry) -> Result<VersionPin, PortError> {
    let entry = EntryId::from_hex(&row.entry_id).map_err(error)?;
    let version = VersionId::from_hex(
        row.version_id
            .as_deref()
            .ok_or_else(|| error("missing version"))?,
    )
    .map_err(error)?;
    let digest = vaultsyncd_transfer::hash::from_hex(
        row.hash.as_deref().ok_or_else(|| error("missing hash"))?,
    )
    .map_err(error)?;
    let size: u64 = row
        .size
        .as_deref()
        .ok_or_else(|| error("missing size"))?
        .parse()
        .map_err(error)?;
    let mut tx = local.db.begin_write().await.map_err(error)?;
    let old: Option<(Vec<u8>, Vec<u8>, i64)> = sqlx::query_as(
        "SELECT entry_id,digest,size FROM agent_versions WHERE binding_id=? AND version_id=?",
    )
    .bind(local.binding.as_bytes().as_slice())
    .bind(version.as_bytes().as_slice())
    .fetch_optional(&mut *tx)
    .await
    .map_err(error)?;
    if old.is_some_and(|(e, h, n)| e != entry.as_bytes() || h != digest || n as u64 != size) {
        return Err(error("immutable version facts changed"));
    }
    sqlx::query("INSERT OR IGNORE INTO agent_versions VALUES(?,?,?,?,?)")
        .bind(local.binding.as_bytes().as_slice())
        .bind(entry.as_bytes().as_slice())
        .bind(version.as_bytes().as_slice())
        .bind(digest.as_slice())
        .bind(i64::try_from(size).map_err(error)?)
        .execute(&mut *tx)
        .await
        .map_err(error)?;
    tx.commit().await.map_err(error)?;
    VersionPin::new(
        local.binding.to_string(),
        entry.to_string(),
        version.to_string(),
        digest,
        size,
    )
    .map_err(error)
}
pub(crate) async fn action(
    local: &Local,
    op: OpId,
    value: &serde_json::Value,
) -> Result<serde_json::Value, PortError> {
    let text = serde_json::to_string(value).map_err(error)?;
    let mut tx = local.db.begin_write().await.map_err(error)?;
    let old: Option<String> = sqlx::query_scalar(
        "SELECT specification FROM agent_actions WHERE op_id=? AND binding_id=?",
    )
    .bind(op.as_bytes().as_slice())
    .bind(local.binding.as_bytes().as_slice())
    .fetch_optional(&mut *tx)
    .await
    .map_err(error)?;
    if let Some(old) = old {
        if old != text {
            return Err(error("OP_ID_REUSED: application action differs"));
        }
    } else {
        sqlx::query("INSERT INTO agent_actions(op_id,binding_id,specification) VALUES(?,?,?)")
            .bind(op.as_bytes().as_slice())
            .bind(local.binding.as_bytes().as_slice())
            .bind(text)
            .execute(&mut *tx)
            .await
            .map_err(error)?;
    }
    tx.commit().await.map_err(error)?;
    Ok(value.clone())
}
pub(crate) struct Importer {
    pub local: Arc<Local>,
    pub adapter: Arc<Mutex<Adapter>>,
    pub callbacks: Arc<HydrationCallbacks>,
    pub content: Arc<dyn VersionedContent>,
    pub remote: Arc<crate::remote::Remote>,
}
impl Importer {
    pub(crate) fn adopt(&self, path: &str, fixed: &VersionPin) -> Result<(), PortError> {
        let adapter = self.adapter.lock().map_err(error)?;
        let catalog = adapter.catalog();
        let root = catalog.read().map_err(error)?.root;
        let id = Identity {
            binding: self.local.binding,
            root,
            entry: EntryId::from_hex(&fixed.entry).map_err(error)?,
            version: VersionId::from_hex(&fixed.version).map_err(error)?,
        };
        self.callbacks
            .register_version(PinnedVersion::new(id, fixed.clone()).map_err(error)?)
            .map_err(error)?;
        let meta = adapter.inspect(path).map_err(error)?;
        adapter
            .adopt_complete(path, &meta, id, fixed.digest, fixed.size)
            .map_err(error)?;
        catalog.write().map_err(error)?.insert(id).map_err(error)?;
        Ok(())
    }
}
impl NameEquivalence for Importer {
    fn equivalent(&self, a: &str, b: &str) -> bool {
        NativeNames.equivalent(a, b)
    }
}
impl ImportLocal for Importer {
    fn validate<'a>(
        &'a self,
        binding: &'a BindingRecord,
        other: &'a [String],
    ) -> PortFuture<'a, Result<RootAdmission, PortError>> {
        Box::pin(async move {
            self.local.root.verify().map_err(error)?;
            let state = vaultsyncd_platform_windows::files::RootHandle::open(&self.local.cache)
                .map_err(error)?;
            let mut disjoint = !self.local.root.overlaps(&state);
            for p in other {
                let other =
                    vaultsyncd_platform_windows::files::RootHandle::open(std::path::Path::new(p))
                        .map_err(error)?;
                disjoint &= !self.local.root.overlaps(&other);
            }
            if binding.id != self.local.binding {
                return Err(error("binding mismatch"));
            }
            Ok(RootAdmission {
                identity: format!("{:?}", self.local.root.identity()),
                healthy: true,
                directory: true,
                disjoint,
                representable: true,
            })
        })
    }
    fn scan(&self, binding: BindingId) -> PortFuture<'_, Result<Vec<LocalImportEntry>, PortError>> {
        Box::pin(async move {
            if binding != self.local.binding {
                return Err(error("binding mismatch"));
            }
            let scan = self.local.root.scan();
            if !scan.permits_missing_comparison() {
                return Err(error("incomplete scan"));
            }
            let mut result = Vec::new();
            for entry in scan.entries {
                let meta = entry
                    .metadata
                    .ok_or_else(|| error("unknown scan metadata"))?;
                if meta.directory {
                    result.push(LocalImportEntry {
                        path: entry.path,
                        kind: EntryKind::Directory,
                        generation: 0,
                        size: None,
                        hash: None,
                    });
                } else {
                    let snapshot = self
                        .local
                        .observe_snapshot(&entry.path)
                        .await?
                        .ok_or_else(|| error("scan changed"))?;
                    let e = snapshot.evidence();
                    result.push(LocalImportEntry {
                        path: entry.path,
                        kind: EntryKind::File,
                        generation: e.generation,
                        size: Some(e.size),
                        hash: Some(e.digest),
                    });
                }
            }
            Ok(result)
        })
    }
    fn preserve<'a>(
        &'a self,
        binding: BindingId,
        op: OpId,
        from: &'a RelativePath,
        to: &'a RelativePath,
        generation: Generation,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            if binding != self.local.binding {
                return Err(error("binding mismatch"));
            }
            let text:String=sqlx::query_scalar("SELECT evidence FROM agent_evidence WHERE binding_id=? AND path=? AND generation=?").bind(binding.as_bytes().as_slice()).bind(from.to_string()).bind(generation.to_db_integer()).fetch_one(self.local.db.pool()).await.map_err(error)?;
            let expected: vaultsyncd_platform_windows::files::controlled::Evidence =
                serde_json::from_str(&text).map_err(error)?;
            action(&self.local,op,&serde_json::json!({"kind":"preserve","from":from.to_string(),"to":to.to_string(),"before":expected})).await?;
            match self.local.root.inspect(&from.to_string()) {
                Ok(_) => {
                    self.local
                        .native
                        .preserve(&from.to_string(), &to.to_string(), &expected)
                        .map_err(error)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let _ = self.local.native.seed(&to.to_string(), expected.clone());
                    let actual = self
                        .local
                        .observe_snapshot(&to.to_string())
                        .await?
                        .ok_or_else(|| error("preserved file missing"))?;
                    if actual.evidence() != &expected {
                        return Err(error("preserve recovery differs"));
                    }
                }
                Err(e) => return Err(error(e)),
            }
            Ok(())
        })
    }
    fn install(&self, request: &ImportInstall) -> PortFuture<'_, Result<Generation, PortError>> {
        let request = request.clone();
        Box::pin(async move {
            if request.binding != self.local.binding {
                return Err(error("binding mismatch"));
            }
            if request.remote.kind == EntryKind::Directory {
                action(&self.local,request.op,&serde_json::json!({"kind":"import_directory","path":request.path.to_string(),"remote":request.remote})).await?;
                let intent = vaultsyncd_agent_core::ports::state::ActionIntent {
                    op: request.op,
                    binding: request.binding,
                    kind: vaultsyncd_agent_core::ports::state::LocalWriteKind::Import,
                    entry: Some(EntryId::from_hex(&request.remote.entry_id).map_err(error)?),
                    target_path: request.path.clone(),
                    source_path: None,
                    content_hash: None,
                    generation: Generation::ZERO,
                };
                let intent_id = vaultsyncd_agent_core::ports::state::IntentStore::begin_intent(
                    &ClientStorePorts::new(self.local.db.clone()),
                    &intent,
                )
                .await?;
                let meta = self
                    .local
                    .root
                    .ensure_directory(&request.path.to_string())
                    .map_err(error)?;
                let mut physical = meta.identity.volume.to_le_bytes().to_vec();
                physical.extend(meta.identity.file);
                let mut tx = self.local.db.begin_write().await.map_err(error)?;
                entries::put_baseline(
                    &mut tx,
                    request.binding,
                    &crate::work::baseline(&request.remote)?,
                )
                .await
                .map_err(error)?;
                entries::put_local(
                    &mut tx,
                    request.binding,
                    &LocalEntry {
                        entry: EntryId::from_hex(&request.remote.entry_id).map_err(error)?,
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
                return Ok(Generation::ZERO);
            }
            let fixed = pin(&self.local, &request.remote).await?;
            if request.mode == SyncMode::Full {
                let spec = ApplySpec::for_import_file(&request)?;
                let temp = DiskTemp::new(&self.local.cache, request.op);
                let receipt = ControlledApply::new(self.local.db.clone(), self.local.clone())
                    .apply(
                        &spec,
                        &*self.content,
                        &temp,
                        &ClientStorePorts::new(self.local.db.clone()),
                        &DownloadOptions::new(1024 * 1024),
                    )
                    .await?;
                self.adopt(&request.path.to_string(), &fixed)?;
                if self
                    .local
                    .native
                    .publication_present(&request.op.to_string())
                    .map_err(error)?
                {
                    self.local
                        .native
                        .acknowledge_publication(&request.op.to_string())
                        .map_err(error)?;
                }
                return Ok(receipt.generation);
            }
            let id = Identity {
                binding: request.binding,
                root: self
                    .adapter
                    .lock()
                    .map_err(error)?
                    .catalog()
                    .read()
                    .map_err(error)?
                    .root,
                entry: EntryId::from_hex(&request.remote.entry_id).map_err(error)?,
                version: VersionId::from_hex(&fixed.version).map_err(error)?,
            };
            self.callbacks
                .register_version(PinnedVersion::new(id, fixed.clone()).map_err(error)?)
                .map_err(error)?;
            action(&self.local,request.op,&serde_json::json!({"kind":"placeholder","path":request.path.to_string(),"remote":request.remote})).await?;
            let meta = {
                let adapter = self.adapter.lock().map_err(error)?;
                let result = match adapter.inspect(&request.path.to_string()) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        adapter.create(&request.path.to_string(), id, fixed.size, false)
                    }
                    Ok(meta)
                        if meta
                            .cloud
                            .as_ref()
                            .is_some_and(|c| c.identity == id.encode()) =>
                    {
                        Ok(meta)
                    }
                    Ok(_) => return Err(error("placeholder target occupied")),
                    Err(e) => Err(e),
                };
                let meta = result.map_err(error)?;
                adapter
                    .catalog()
                    .write()
                    .map_err(error)?
                    .insert(id)
                    .map_err(error)?;
                meta
            };
            let mut physical = meta.identity.volume.to_le_bytes().to_vec();
            physical.extend(meta.identity.file);
            let mut tx = self.local.db.begin_write().await.map_err(error)?;
            entries::put_local(
                &mut tx,
                request.binding,
                &LocalEntry {
                    entry: id.entry,
                    generation: Generation::new(1).map_err(error)?,
                    dirty: Dirty::Clean,
                    residency: Residency::MetadataOnly,
                    fingerprint: Some(fixed.digest.to_vec()),
                    file_identity: Some(physical),
                },
            )
            .await
            .map_err(error)?;
            tx.commit().await.map_err(error)?;
            Generation::new(1).map_err(error)
        })
    }
}
