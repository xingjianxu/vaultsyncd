//! Explicit downloads and releases; identities and fixed versions come from the local baseline.
use crate::{
    config::AgentConfig,
    local::error,
    management::admit,
    work::{Worker, metadata, paths},
};
use std::{io::Read, sync::Arc};
use vaultsyncd_agent_core::{
    bindings::control::{FullFile, FullMaterializer},
    executor::release::{ManualRelease, ReleaseSpec},
};
use vaultsyncd_client_store::entries::{self, LocalEntry};
use vaultsyncd_domain::{
    ids::{EntryId, OpId, RootId, VersionId},
    state::{Dirty, Residency},
    version::Generation,
};
use vaultsyncd_http_client::{ApiClient, ReqwestTransport};
use vaultsyncd_platform_windows::cfapi::catalog::Identity;
use vaultsyncd_transfer::ports::{PortError, PortFuture};

pub(crate) async fn download(worker: &Worker, entry: EntryId, op: OpId) -> Result<(), PortError> {
    let importer = &worker.importer;
    let local = &importer.local;
    let rows = metadata(importer).await?;
    let row = rows
        .get(&entry.to_string())
        .ok_or_else(|| PortError::rejected("NOT_FOUND", "entry not in active baseline"))?;
    if row.kind != vaultsyncd_protocol::generated::EntryKind::File {
        return Err(PortError::rejected("INVALID_ARGUMENT", "file required"));
    }
    let path = paths(&rows)?
        .remove(&entry.to_string())
        .ok_or_else(|| error("unknown path"))?;
    let fixed = crate::importer::pin(local, row).await?;
    crate::importer::action(local,op,&serde_json::json!({"kind":"manual_download","entry":entry.to_string(),"path":path,"version":fixed.version})).await?;
    let root = local.root.clone();
    let selected = path.clone();
    let expected = Identity {
        binding: local.binding,
        root: importer.remote.root,
        entry,
        version: VersionId::from_hex(&fixed.version).map_err(error)?,
    };
    tokio::task::spawn_blocking(move || -> Result<(), PortError> {
        let before = root.inspect(&selected).map_err(error)?;
        if before
            .cloud
            .as_ref()
            .is_some_and(|c| c.identity != expected.encode())
        {
            return Err(error("placeholder identity changed"));
        }
        let mut file = root.open_read(&selected).map_err(error)?;
        let mut buffer = vec![0; 256 * 1024];
        loop {
            if file.read(&mut buffer).map_err(error)? == 0 {
                break;
            }
        }
        Ok(())
    })
    .await
    .map_err(error)??;
    // Observe actual complete bytes; preserve dirty/awaiting_commit instead of clearing it.
    let observed = local
        .observe_snapshot(&path)
        .await?
        .ok_or_else(|| error("downloaded file vanished"))?;
    let e = observed.evidence();
    if e.digest != fixed.digest || e.size != fixed.size {
        return Err(error("download bytes differ from fixed remote version"));
    }
    let mut tx = local.db.begin_write().await.map_err(error)?;
    let mut state = entries::local_entry(&mut tx, local.binding, entry)
        .await
        .map_err(error)?
        .ok_or_else(|| error("missing local state"))?;
    if state.dirty != Dirty::Clean {
        return Err(error("unconfirmed content retained"));
    }
    state.generation = Generation::new(e.generation).map_err(error)?;
    state.residency = Residency::Materialized;
    state.fingerprint = Some(e.digest.to_vec());
    state.file_identity = Some(e.identity());
    entries::put_local(&mut tx, local.binding, &state)
        .await
        .map_err(error)?;
    tx.commit().await.map_err(error)?;
    Ok(())
}
pub(crate) async fn release(
    worker: &Worker,
    config: &AgentConfig,
    client: &ApiClient<ReqwestTransport>,
    entry: EntryId,
    op: OpId,
) -> Result<(), PortError> {
    admit(config)?;
    let local = &worker.importer.local;
    let rows = metadata(&worker.importer).await?;
    let row = rows
        .get(&entry.to_string())
        .ok_or_else(|| PortError::rejected("NOT_FOUND", "entry missing"))?;
    let path = vaultsyncd_fs_common::path::RelativePath::parse(
        &paths(&rows)?
            .remove(&entry.to_string())
            .ok_or_else(|| error("path missing"))?,
    )
    .map_err(error)?;
    let fixed = crate::importer::pin(local, row).await?;
    let configured = config
        .bindings
        .iter()
        .find(|b| b.id == local.binding.to_string())
        .ok_or_else(|| error("binding missing"))?;
    let root = RootId::from_hex(&configured.root_id).map_err(error)?;
    let identity = Identity {
        binding: local.binding,
        root,
        entry,
        version: VersionId::from_hex(&fixed.version).map_err(error)?,
    };
    ManualRelease::new(
        local.db.clone(),
        Arc::new(crate::release::NativeReleaseLocal::new(
            &configured.path,
            local.db.clone(),
            identity,
            path.clone(),
            fixed.clone(),
        )?),
        Arc::new(crate::release::HttpReleaseRemote::new(
            client.clone(),
            root,
        )?),
    )
    .release(&ReleaseSpec {
        op,
        binding: local.binding,
        entry,
        path,
        pin: fixed,
    })
    .await?;
    Ok(())
}
/// Full-mode proof uses the same real download/read adapter and core's post-action CAS.
pub(crate) struct Materializer(pub Worker);
impl FullMaterializer for Materializer {
    fn ensure_full<'a>(
        &'a self,
        file: &'a FullFile,
    ) -> PortFuture<'a, Result<LocalEntry, PortError>> {
        Box::pin(async move {
            let local = &self.0.importer.local;
            let meta = local.root.inspect(&file.path.to_string()).map_err(error)?;
            if meta.cloud.as_ref().is_some_and(|c| c.on_disk < meta.size) {
                if file.local.as_ref().is_some_and(|l| l.dirty != Dirty::Clean) {
                    return Err(error("unconfirmed partial content"));
                }
                download(&self.0, file.entry, crate::work::new_op()).await?;
            }
            let _stable = local
                .root
                .open_read(&file.path.to_string())
                .map_err(error)?;
            let snapshot = local
                .observe_snapshot(&file.path.to_string())
                .await?
                .ok_or_else(|| error("missing full file"))?;
            let mut conn = local.db.pool().acquire().await.map_err(error)?;
            let state = entries::local_entry(&mut conn, file.binding, file.entry)
                .await
                .map_err(error)?
                .ok_or_else(|| error("missing full state"))?;
            drop(conn);
            let e = snapshot.evidence();
            if state.residency != Residency::Materialized
                || state.generation.get() != e.generation
                || state.fingerprint.as_deref() != Some(e.digest.as_slice())
                || state.file_identity.as_deref() != Some(e.identity().as_slice())
            {
                if state.dirty != Dirty::Clean {
                    return Err(error("unconfirmed full evidence changed"));
                }
                let rows = metadata(&self.0.importer).await?;
                let row = rows
                    .get(&file.entry.to_string())
                    .ok_or_else(|| error("full target missing"))?;
                if file.version.map(|v| v.to_string()) != row.version_id {
                    return Err(error("full target changed"));
                }
                let fixed = crate::importer::pin(local, row).await?;
                if e.digest != fixed.digest || e.size != fixed.size {
                    return Err(error("full bytes are not the committed fixed version"));
                }
                let verified = LocalEntry {
                    entry: file.entry,
                    generation: Generation::new(e.generation).map_err(error)?,
                    dirty: Dirty::Clean,
                    residency: Residency::Materialized,
                    fingerprint: Some(e.digest.to_vec()),
                    file_identity: Some(e.identity()),
                };
                let mut tx = local.db.begin_write().await.map_err(error)?;
                if entries::local_entry(&mut tx, file.binding, file.entry)
                    .await
                    .map_err(error)?
                    .as_ref()
                    != Some(&state)
                {
                    return Err(error("full local CAS changed"));
                }
                entries::put_local(&mut tx, file.binding, &verified)
                    .await
                    .map_err(error)?;
                tx.commit().await.map_err(error)?;
                return Ok(verified);
            }
            Ok(state)
        })
    }
}
