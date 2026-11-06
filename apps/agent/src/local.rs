//! Windows content adapters. No path-based replacement bypasses the platform protection.
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use vaultsyncd_agent_core::{
    executor::apply::{ContentObservation, ControlledLocal},
    ports::local::*,
    recovery::ContentEvidence,
};
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::{
    ids::{BindingId, OpId},
    state::Usage,
    version::Generation,
};
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_platform_windows::files::{
    RootHandle,
    controlled::{Evidence, FixedSnapshot, ObservedContent, WindowsFiles},
    staging::StagingFile,
};
use vaultsyncd_transfer::ports::{PortError, PortFuture, SnapshotSource, TempContent};
pub(crate) fn error(e: impl std::fmt::Debug) -> PortError {
    PortError::rejected("LOCAL_CHANGED", format!("{e:?}"))
}
/// One binding's native adapter and immutable observation registry.
pub struct Local {
    pub(crate) native: Arc<WindowsFiles>,
    pub(crate) root: Arc<RootHandle>,
    pub(crate) cache: PathBuf,
    pub(crate) db: Arc<Database>,
    pub(crate) binding: BindingId,
}
impl Local {
    /// Open safe, disjoint directories and seed previous evidence; no disk content is trusted as clean.
    pub async fn open(
        root: &Path,
        cache: &Path,
        db: Arc<Database>,
        binding: BindingId,
    ) -> Result<Self, PortError> {
        let native = Arc::new(WindowsFiles::open(root, cache).map_err(error)?);
        let rows:Vec<(String,String)>=sqlx::query_as("SELECT path,evidence FROM agent_evidence e WHERE binding_id=? AND generation=(SELECT max(generation) FROM agent_evidence x WHERE x.binding_id=e.binding_id AND x.path=e.path)").bind(binding.as_bytes().as_slice()).fetch_all(db.pool()).await.map_err(error)?;
        for (path, text) in rows {
            native
                .seed(&path, serde_json::from_str(&text).map_err(error)?)
                .map_err(error)?;
        }
        Ok(Self {
            native,
            root: Arc::new(RootHandle::open(root).map_err(error)?),
            cache: cache.into(),
            db,
            binding,
        })
    }
    pub(crate) async fn observe_snapshot(
        &self,
        path: &str,
    ) -> Result<Option<Arc<FixedSnapshot>>, PortError> {
        match self.native.observe(path).map_err(error)? {
            ObservedContent::Absent => Ok(None),
            ObservedContent::File(snapshot) => {
                let evidence = snapshot.evidence();
                let text = serde_json::to_string(evidence).map_err(error)?;
                let mut tx = self.db.begin_write().await.map_err(error)?;
                let old:Option<String>=sqlx::query_scalar("SELECT evidence FROM agent_evidence WHERE binding_id=? AND path=? AND generation=?").bind(self.binding.as_bytes().as_slice()).bind(path).bind(evidence.generation as i64).fetch_optional(&mut *tx).await.map_err(error)?;
                if old.as_ref().is_some_and(|v| v != &text) {
                    return Err(error("immutable observation changed"));
                }
                sqlx::query("INSERT OR IGNORE INTO agent_evidence VALUES(?,?,?,?)")
                    .bind(self.binding.as_bytes().as_slice())
                    .bind(path)
                    .bind(evidence.generation as i64)
                    .bind(text)
                    .execute(&mut *tx)
                    .await
                    .map_err(error)?;
                tx.commit().await.map_err(error)?;
                Ok(Some(snapshot))
            }
        }
    }
}
pub(crate) fn facts(path: RelativePath, e: &Evidence) -> Result<LocalFacts, PortError> {
    Ok(LocalFacts {
        path,
        kind: LocalEntryKind::File,
        generation: Generation::new(e.generation).map_err(error)?,
        size: e.size,
        file_identity: Some(e.identity()),
        usage: Usage::Idle,
    })
}
pub(crate) fn evidence(value: &ContentEvidence) -> Result<Option<Evidence>, PortError> {
    match value {
        ContentEvidence::Absent => Ok(None),
        ContentEvidence::File {
            generation,
            identity,
            digest,
            size,
        } => {
            if identity.len() != 24 {
                return Err(error("invalid physical identity"));
            }
            Ok(Some(Evidence {
                generation: *generation,
                volume: u64::from_le_bytes(identity[..8].try_into().map_err(error)?),
                file: identity[8..].try_into().map_err(error)?,
                digest: *digest,
                size: *size,
            }))
        }
    }
}
struct Snapshot {
    inner: Arc<FixedSnapshot>,
    identity: String,
}
impl Snapshot {
    fn new(inner: Arc<FixedSnapshot>) -> Self {
        let e = inner.evidence();
        let identity = format!(
            "{}-{}-{}-{}",
            e.volume,
            vaultsyncd_transfer::hash::to_hex(&e.digest),
            e.generation,
            e.size
        );
        Self { inner, identity }
    }
}
impl SnapshotSource for Snapshot {
    fn identity(&self) -> &str {
        &self.identity
    }
    fn total_size(&self) -> u64 {
        self.inner.evidence().size
    }
    fn read(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            let mut result = Vec::new();
            let mut at = offset;
            while at
                < offset
                    .checked_add(length)
                    .ok_or_else(|| error("range overflow"))?
            {
                let n = (offset + length - at).min(1024 * 1024);
                result.extend(self.inner.read_at(at, n).map_err(error)?);
                at += n;
            }
            Ok(result)
        })
    }
}
impl ControlledLocal for Local {
    fn observe_content(
        &self,
        path: &RelativePath,
    ) -> PortFuture<'_, Result<ContentObservation, PortError>> {
        let path = path.clone();
        Box::pin(async move {
            match self.observe_snapshot(&path.to_string()).await? {
                None => Ok(ContentObservation::Absent),
                Some(snapshot) => Ok(ContentObservation::File {
                    facts: facts(path, snapshot.evidence())?,
                    snapshot: Arc::new(Snapshot::new(snapshot)),
                }),
            }
        })
    }
    fn replace_verified<'a>(
        &'a self,
        request: &'a ApplyRequest,
        before: &'a ContentEvidence,
    ) -> PortFuture<'a, Result<ApplyReceipt, PortError>> {
        Box::pin(async move {
            if request.binding != self.binding {
                return Err(error("binding mismatch"));
            }
            let expected = evidence(before)?;
            let after = self
                .native
                .replace(
                    &request.op.to_string(),
                    &request.target.to_string(),
                    Path::new(
                        request
                            .staged
                            .as_deref()
                            .ok_or_else(|| error("missing stage"))?,
                    ),
                    expected.as_ref(),
                    request
                        .expected_digest
                        .ok_or_else(|| error("missing digest"))?,
                )
                .map_err(error)?;
            Ok(ApplyReceipt {
                stage: ApplyStage::Applied,
                generation: Generation::new(after.generation).map_err(error)?,
                file_identity: Some(after.identity()),
            })
        })
    }
}
impl LocalPort for Local {
    fn capabilities(&self) -> LocalCapabilities {
        LocalCapabilities {
            atomic_replace: false,
            ..LocalCapabilities::full()
        }
    }
    fn observe(&self, path: &RelativePath) -> PortFuture<'_, Result<LocalFacts, PortError>> {
        let path = path.clone();
        Box::pin(async move {
            let snapshot = self
                .observe_snapshot(&path.to_string())
                .await?
                .ok_or_else(|| error("missing local file"))?;
            facts(path, snapshot.evidence())
        })
    }
    fn snapshot(
        &self,
        path: &RelativePath,
        generation: Generation,
    ) -> PortFuture<'_, Result<Arc<dyn SnapshotSource>, PortError>> {
        let path = path.clone();
        Box::pin(async move {
            let snapshot = self
                .observe_snapshot(&path.to_string())
                .await?
                .ok_or_else(|| error("missing local file"))?;
            if snapshot.evidence().generation != generation.get() {
                return Err(error("generation changed"));
            }
            Ok(Arc::new(Snapshot::new(snapshot)) as Arc<dyn SnapshotSource>)
        })
    }
    fn apply(&self, _: &ApplyRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async { Err(error("use ControlledApply with complete before evidence")) })
    }
    fn move_entry(&self, _: &MoveRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async {
            Err(PortError::rejected(
                "UNSUPPORTED_ENTRY",
                "namespace move requires durable evidence",
            ))
        })
    }
    fn remove(&self, _: &RemoveRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async {
            Err(PortError::rejected(
                "UNSUPPORTED_ENTRY",
                "delete requires tombstone coordination",
            ))
        })
    }
    fn release(&self, _: &ReleaseRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async { Err(error("use explicit ManualRelease with live server lease")) })
    }
}
/// Each operation opens/closes its safe internal stage; no stage handle prevents guarded publication.
pub struct DiskTemp {
    directory: PathBuf,
    name: String,
    path: String,
}
impl DiskTemp {
    pub(crate) fn new(directory: &Path, op: OpId) -> Self {
        let name = format!("download-{op}");
        Self {
            directory: directory.into(),
            path: directory.join(&name).to_string_lossy().into_owned(),
            name,
        }
    }
    fn open(&self) -> Result<StagingFile, PortError> {
        StagingFile::open(&self.directory, &self.name).map_err(error)
    }
}
impl TempContent for DiskTemp {
    fn path(&self) -> &str {
        &self.path
    }
    fn ensure_capacity(&self, len: u64) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.open()?.ensure_capacity(len).await })
    }
    fn write_at<'a>(
        &'a self,
        offset: u64,
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move { self.open()?.write_at(offset, bytes).await })
    }
    fn read_at(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move { self.open()?.read_at(offset, length).await })
    }
    fn sync(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.open()?.sync().await })
    }
    fn discard(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.open()?.discard().await })
    }
}
