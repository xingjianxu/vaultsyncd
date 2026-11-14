//! Windows content adapters. No path-based replacement bypasses the platform protection.
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Semaphore;
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
    VaultHandle,
    controlled::{Evidence, FixedSnapshot, ObservedContent, WindowsFiles},
    staging::StagingFile,
};
use vaultsyncd_transfer::ports::{PortError, PortFuture, SnapshotSource, TempContent};
pub(crate) fn error(e: impl std::fmt::Debug) -> PortError {
    PortError::rejected("LOCAL_CHANGED", format!("{e:?}"))
}

static BLOCKING: Semaphore = Semaphore::const_new(1);

/// Bound native scans, opens, recovery and content observation across all bindings.
/// The closure owns the permit: cancelling its async waiter cannot admit a second job
/// while the first is still running. Never call this recursively from a blocking job.
pub(crate) async fn blocking<T, F>(work: F) -> Result<T, PortError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, PortError> + Send + 'static,
{
    let permit = BLOCKING.acquire().await.map_err(error)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(error)?
}

/// One binding's native adapter and immutable observation registry.
pub struct Local {
    pub(crate) native: Arc<WindowsFiles>,
    pub(crate) vault: Arc<VaultHandle>,
    pub(crate) cache: PathBuf,
    pub(crate) db: Arc<Database>,
    pub(crate) binding: BindingId,
}
impl Local {
    /// Open safe, disjoint directories and seed previous evidence; no disk content is trusted as clean.
    pub async fn open(
        vault: &Path,
        cache: &Path,
        db: Arc<Database>,
        binding: BindingId,
    ) -> Result<Self, PortError> {
        let rows:Vec<(String,String)>=sqlx::query_as("SELECT path,evidence FROM agent_evidence e WHERE binding_id=? AND generation=(SELECT max(generation) FROM agent_evidence x WHERE x.binding_id=e.binding_id AND x.path=e.path)").bind(binding.as_bytes().as_slice()).fetch_all(db.pool()).await.map_err(error)?;
        let vault_path = vault.to_owned();
        let cache_path = cache.to_owned();
        let (native, vault) = blocking(move || {
            let native = Arc::new(WindowsFiles::open(&vault_path, &cache_path).map_err(error)?);
            for (path, text) in rows {
                native
                    .seed(&path, serde_json::from_str(&text).map_err(error)?)
                    .map_err(error)?;
            }
            let vault = Arc::new(VaultHandle::open(&vault_path).map_err(error)?);
            Ok((native, vault))
        })
        .await?;
        Ok(Self {
            native,
            vault,
            cache: cache.into(),
            db,
            binding,
        })
    }
    /// Recover a moved object's generation by physical identity, never trusting
    /// metadata as content evidence. The native observation must still hash it.
    async fn prior_evidence(&self, path: &str) -> Result<Option<Evidence>, PortError> {
        let vault = self.vault.clone();
        let owned_path = path.to_owned();
        let meta = blocking(move || Ok(vault.inspect(&owned_path))).await?;
        // Native observation below decides whether inspection failure is a healthy
        // absence or an error; failed metadata inspection alone never proves deletion.
        if let Ok(meta) = &meta {
            if !meta.directory {
                let mut physical = meta.identity.volume.to_le_bytes().to_vec();
                physical.extend(meta.identity.file);
                let prior:Option<(i64,Option<Vec<u8>>)>=sqlx::query_as("SELECT generation,fingerprint FROM local_entries WHERE binding_id=? AND file_identity=? ORDER BY generation DESC LIMIT 1").bind(self.binding.as_bytes().as_slice()).bind(physical).fetch_optional(self.db.pool()).await.map_err(error)?;
                if let Some((generation, Some(digest))) = prior {
                    let digest: [u8; 32] = digest
                        .try_into()
                        .map_err(|_| error("invalid persisted fingerprint"))?;
                    return Ok(Some(Evidence {
                        generation: Generation::from_db_integer(generation)
                            .map_err(error)?
                            .get(),
                        volume: meta.identity.volume,
                        file: meta.identity.file,
                        digest,
                        size: meta.size,
                    }));
                }
            }
        }
        Ok(None)
    }

    async fn observe_native<T, F>(&self, path: &str, observe: F) -> Result<T, PortError>
    where
        T: Send + 'static,
        F: FnOnce(&WindowsFiles, &str) -> Result<T, PortError> + Send + 'static,
    {
        let prior = self.prior_evidence(path).await?;
        let native = self.native.clone();
        let path = path.to_owned();
        blocking(move || {
            if let Some(prior) = prior {
                native.seed_if_unknown(&path, prior).map_err(error)?;
            }
            observe(&native, &path)
        })
        .await
    }

    /// Observe protected, fully hashed contents without creating a fixed snapshot.
    pub(crate) async fn observe_evidence(&self, path: &str) -> Result<Option<Evidence>, PortError> {
        self.observe_evidence_cancellable(path, Arc::new(AtomicBool::new(false)))
            .await
    }

    /// Share the worker's shutdown flag with native hashing. Interrupted observations
    /// return cancellation without persisting evidence or synthesizing absence.
    pub(crate) async fn observe_evidence_cancellable(
        &self,
        path: &str,
        cancel: Arc<AtomicBool>,
    ) -> Result<Option<Evidence>, PortError> {
        if cancel.load(Ordering::Acquire) {
            return Err(PortError::Cancelled);
        }
        let worker_cancel = cancel.clone();
        let observed = self
            .observe_native(path, move |native, path| {
                if worker_cancel.load(Ordering::Acquire) {
                    return Err(PortError::Cancelled);
                }
                native
                    .observe_evidence_cancellable(path, &worker_cancel)
                    .map_err(|e| {
                        if e.kind() == std::io::ErrorKind::Interrupted
                            && worker_cancel.load(Ordering::Acquire)
                        {
                            PortError::Cancelled
                        } else {
                            error(e)
                        }
                    })
            })
            .await?;
        if cancel.load(Ordering::Acquire) {
            return Err(PortError::Cancelled);
        }
        if let Some(evidence) = &observed {
            persist_evidence(&self.db, self.binding, path, evidence).await?;
        }
        Ok(observed)
    }

    pub(crate) async fn observe_snapshot(
        &self,
        path: &str,
    ) -> Result<Option<Arc<FixedSnapshot>>, PortError> {
        match self
            .observe_native(path, |native, path| native.observe(path).map_err(error))
            .await?
        {
            ObservedContent::Absent => Ok(None),
            ObservedContent::File(snapshot) => {
                persist_evidence(&self.db, self.binding, path, snapshot.evidence()).await?;
                Ok(Some(snapshot))
            }
        }
    }
}

async fn persist_evidence(
    db: &Database,
    binding: BindingId,
    path: &str,
    evidence: &Evidence,
) -> Result<(), PortError> {
    let text = serde_json::to_string(evidence).map_err(error)?;
    let generation = Generation::new(evidence.generation)
        .map_err(error)?
        .to_db_integer();
    let read = || {
        sqlx::query_scalar::<_, String>(
            "SELECT evidence FROM agent_evidence WHERE binding_id=? AND path=? AND generation=?",
        )
        .bind(binding.as_bytes().as_slice())
        .bind(path)
        .bind(generation)
    };
    if let Some(old) = read().fetch_optional(db.pool()).await.map_err(error)? {
        return same_evidence(&old, &text);
    }
    // Another observer may have inserted this generation since the unlocked read.
    // Recheck under the short write transaction before inserting immutable evidence.
    let mut tx = db.begin_write().await.map_err(error)?;
    if let Some(old) = read().fetch_optional(&mut *tx).await.map_err(error)? {
        same_evidence(&old, &text)?;
    } else {
        sqlx::query(
            "INSERT INTO agent_evidence(binding_id,path,generation,evidence) VALUES(?,?,?,?)",
        )
        .bind(binding.as_bytes().as_slice())
        .bind(path)
        .bind(generation)
        .bind(text)
        .execute(&mut *tx)
        .await
        .map_err(error)?;
    }
    tx.commit().await.map_err(error)?;
    Ok(())
}

fn same_evidence(old: &str, observed: &str) -> Result<(), PortError> {
    if old != observed {
        return Err(error("immutable observation changed"));
    }
    Ok(())
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
            "{}-{:02x?}-{}-{}-{}",
            e.volume,
            e.file,
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
            let vault = self.vault.clone();
            let owned_path = path.to_string();
            let meta = blocking(move || vault.inspect(&owned_path).map_err(error)).await?;
            if meta.directory {
                let mut physical = meta.identity.volume.to_le_bytes().to_vec();
                physical.extend(meta.identity.file);
                return Ok(LocalFacts {
                    path,
                    kind: LocalEntryKind::Directory,
                    generation: Generation::ZERO,
                    size: 0,
                    file_identity: Some(physical),
                    usage: Usage::Idle,
                });
            }
            let evidence = self
                .observe_evidence(&path.to_string())
                .await?
                .ok_or_else(|| error("missing local file"))?;
            facts(path, &evidence)
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

#[cfg(test)]
#[path = "local_startup_tests.rs"]
mod startup_tests;
