//! S063 ManualRelease assembly with real HTTP GC leases and continuous native protection.
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use vaultsyncd_agent_core::{
    executor::{
        apply::{ContentObservation, ControlledLocal},
        release::{ReleaseLocal, ReleaseProtection, ReleaseRemote, ReleasedFile, VersionLease},
    },
    ports::local::{ApplyReceipt, ApplyRequest, LocalEntryKind, LocalFacts},
    recovery::ContentEvidence,
};
use vaultsyncd_client_store::{db::Database, entries};
use vaultsyncd_domain::{ids::VaultId, state::Usage, version::Generation};
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_http_client::{ApiClient, ReqwestTransport, version_leases::LeaseEndpoint};
use vaultsyncd_platform_windows::{
    cfapi::{catalog::Identity, release::verified::ReleaseHandle},
    files::{VaultHandle, controlled::Evidence},
};
use vaultsyncd_transfer::ports::{PortError, PortFuture, SnapshotSource, VersionPin};
fn local(e: impl std::fmt::Display) -> PortError {
    PortError::rejected("LOCAL_IN_USE", e.to_string())
}

/// An explicit target's native release adapter; instantiate from durable fixed metadata.
/// Supports only its configured relative path, never a caller-selected arbitrary file.
pub struct NativeReleaseLocal {
    vault: Arc<VaultHandle>,
    database: Arc<Database>,
    identity: Identity,
    path: RelativePath,
    pin: VersionPin,
}
impl NativeReleaseLocal {
    /// Bind verified logical identity/pin to a safe vault. No file is changed or recalled here.
    pub fn new(
        vault: &Path,
        database: Arc<Database>,
        identity: Identity,
        path: RelativePath,
        pin: VersionPin,
    ) -> Result<Self, PortError> {
        if identity.binding.to_string() != pin.binding
            || identity.entry.to_string() != pin.entry
            || identity.version.to_string() != pin.version
        {
            return Err(PortError::protocol("release identity/pin mismatch"));
        }
        Ok(Self {
            vault: Arc::new(VaultHandle::open(vault).map_err(local)?),
            database,
            identity,
            path,
            pin,
        })
    }
}
struct Guard {
    handle: Arc<ReleaseHandle>,
    path: RelativePath,
    pin: VersionPin,
    generation: u64,
}
impl ReleaseLocal for NativeReleaseLocal {
    fn protect<'a>(
        &'a self,
        path: &'a RelativePath,
    ) -> PortFuture<'a, Result<Box<dyn ReleaseProtection>, PortError>> {
        Box::pin(async move {
            if *path != self.path {
                return Err(PortError::rejected(
                    "INVALID_ARGUMENT",
                    "release target differs",
                ));
            }
            let mut conn = self.database.pool().acquire().await.map_err(local)?;
            let state = entries::local_entry(&mut conn, self.identity.binding, self.identity.entry)
                .await
                .map_err(local)?
                .ok_or_else(|| local("unknown local entry"))?;
            let pending:i64=sqlx::query_scalar("SELECT count(*) FROM controlled_releases WHERE binding_id=? AND completed=0 AND target_path=?")
                .bind(self.identity.binding.as_bytes().as_slice()).bind(path.to_string()).fetch_one(&mut *conn).await.map_err(local)?;
            drop(conn);
            let handle = ReleaseHandle::open(
                &self.vault,
                &path.to_string(),
                self.identity,
                state.generation.get(),
                self.pin.size,
                pending > 0,
            )
            .map_err(local)?;
            // Physical identity is checked even on restart's metadata-only path.
            let actual = handle.metadata().map_err(local)?.identity;
            let mut physical = actual.volume.to_le_bytes().to_vec();
            physical.extend(actual.file);
            if state.file_identity.as_ref() != Some(&physical) {
                return Err(PortError::rejected(
                    "LOCAL_CHANGED",
                    "release object identity changed",
                ));
            }
            Ok(Box::new(Guard {
                handle: Arc::new(handle),
                path: path.clone(),
                pin: self.pin.clone(),
                generation: state.generation.get(),
            }) as Box<dyn ReleaseProtection>)
        })
    }
}
impl ReleaseProtection for Guard {
    fn content(&self) -> &dyn ControlledLocal {
        self
    }
    fn released(&self) -> PortFuture<'_, Result<Option<ReleasedFile>, PortError>> {
        Box::pin(async move {
            Ok(self
                .handle
                .released_identity()
                .map_err(local)?
                .map(|(generation, identity)| ReleasedFile {
                    generation,
                    identity,
                    pin: self.pin.clone(),
                }))
        })
    }
    fn dehydrate_verified<'a>(
        &'a self,
        expected: &'a ContentEvidence,
        lease: &'a dyn VersionLease,
    ) -> PortFuture<'a, Result<ReleasedFile, PortError>> {
        Box::pin(async move {
            let actual = self.handle.evidence().map_err(local)?;
            if evidence(&actual) != *expected
                || actual.digest != self.pin.digest
                || actual.size != self.pin.size
                || lease.pin() != &self.pin
            {
                return Err(PortError::rejected(
                    "LOCAL_CHANGED",
                    "release evidence/pin differs",
                ));
            }
            lease.ensure_valid().await?;
            self.handle.dehydrate(&actual).map_err(local)?;
            let (generation, identity) = self
                .handle
                .released_identity()
                .map_err(local)?
                .ok_or_else(|| local("dehydration unconfirmed"))?;
            Ok(ReleasedFile {
                generation,
                identity,
                pin: self.pin.clone(),
            })
        })
    }
}
fn evidence(e: &Evidence) -> ContentEvidence {
    ContentEvidence::File {
        generation: e.generation,
        identity: e.identity(),
        digest: e.digest,
        size: e.size,
    }
}
struct GuardSnapshot {
    handle: Arc<ReleaseHandle>,
    size: u64,
    identity: String,
}
impl SnapshotSource for GuardSnapshot {
    fn identity(&self) -> &str {
        &self.identity
    }
    fn total_size(&self) -> u64 {
        self.size
    }
    fn read(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move { self.handle.read_at(offset, length).map_err(local) })
    }
}
impl ControlledLocal for Guard {
    fn observe_content(
        &self,
        path: &RelativePath,
    ) -> PortFuture<'_, Result<ContentObservation, PortError>> {
        let matches = *path == self.path;
        Box::pin(async move {
            if !matches {
                return Err(local("wrong release path"));
            }
            let actual = self.handle.evidence().map_err(local)?;
            Ok(ContentObservation::File {
                facts: LocalFacts {
                    path: self.path.clone(),
                    kind: LocalEntryKind::File,
                    generation: Generation::new(self.generation).map_err(local)?,
                    size: actual.size,
                    file_identity: Some(actual.identity()),
                    usage: Usage::Idle,
                },
                snapshot: Arc::new(GuardSnapshot {
                    handle: self.handle.clone(),
                    size: actual.size,
                    identity: self.pin.version.clone(),
                }),
            })
        })
    }
    fn replace_verified<'a>(
        &'a self,
        _: &'a ApplyRequest,
        _: &'a ContentEvidence,
    ) -> PortFuture<'a, Result<ApplyReceipt, PortError>> {
        Box::pin(async {
            Err(PortError::rejected(
                "UNSUPPORTED",
                "release protection never publishes replacement content",
            ))
        })
    }
}

/// Production fixed-version remote adapter. Ordinary downloads cannot satisfy this port.
pub struct HttpReleaseRemote {
    endpoint: LeaseEndpoint<ReqwestTransport>,
}
impl HttpReleaseRemote {
    /// Bind the authenticated client to its exact remote vault.
    pub fn new(client: ApiClient<ReqwestTransport>, vault: VaultId) -> Result<Self, PortError> {
        Ok(Self {
            endpoint: LeaseEndpoint::new(client, vault.to_string())?,
        })
    }
}
struct HttpLease {
    endpoint: LeaseEndpoint<ReqwestTransport>,
    pin: VersionPin,
    id: String,
    failed: Arc<AtomicBool>,
    heartbeat: tokio::task::JoinHandle<()>,
}
impl ReleaseRemote for HttpReleaseRemote {
    fn acquire<'a>(
        &'a self,
        pin: &'a VersionPin,
    ) -> PortFuture<'a, Result<Box<dyn VersionLease>, PortError>> {
        Box::pin(async move {
            use rand_core::{OsRng, RngCore};
            let mut bytes = [0; 16];
            OsRng.fill_bytes(&mut bytes);
            let id = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            let start = Instant::now();
            let lease = self.endpoint.acquire(pin, &id).await?;
            valid_ttl(&lease.ttl_seconds, start)?;
            let failed = Arc::new(AtomicBool::new(false));
            let worker_failed = failed.clone();
            let ep = self.endpoint.clone();
            let p = pin.clone();
            let key = id.clone();
            let heartbeat = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    let start = Instant::now();
                    let valid = match ep.renew(&p, &key).await {
                        Ok(l) => valid_ttl(&l.ttl_seconds, start).is_ok(),
                        Err(_) => false,
                    };
                    if !valid {
                        worker_failed.store(true, Ordering::Release);
                        break;
                    }
                }
            });
            Ok(Box::new(HttpLease {
                endpoint: self.endpoint.clone(),
                pin: pin.clone(),
                id,
                failed,
                heartbeat,
            }) as Box<dyn VersionLease>)
        })
    }
}
fn valid_ttl(ttl: &str, start: Instant) -> Result<(), PortError> {
    let seconds = ttl
        .parse::<u64>()
        .map_err(|_| PortError::protocol("invalid TTL"))?;
    if seconds.saturating_sub(start.elapsed().as_secs() + 1) < 30 {
        return Err(PortError::rejected(
            "LEASE_EXPIRED",
            "lease deadline too close or response too slow",
        ));
    }
    Ok(())
}
impl VersionLease for HttpLease {
    fn pin(&self) -> &VersionPin {
        &self.pin
    }
    fn ensure_valid(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            if self.failed.load(Ordering::Acquire) {
                return Err(PortError::rejected(
                    "LEASE_EXPIRED",
                    "lease heartbeat failed",
                ));
            }
            let start = Instant::now();
            let result = self.endpoint.renew(&self.pin, &self.id).await?;
            valid_ttl(&result.ttl_seconds, start)
        })
    }
}
impl Drop for HttpLease {
    fn drop(&mut self) {
        self.heartbeat.abort();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let ep = self.endpoint.clone();
            let pin = self.pin.clone();
            let id = self.id.clone();
            runtime.spawn(async move {
                if let Err(e) = ep.release(&pin, &id).await {
                    eprintln!("release lease cleanup deferred to bounded expiry: {e}");
                }
            });
        }
    }
}
