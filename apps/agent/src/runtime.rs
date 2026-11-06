//! Headless Windows process assembly and recovery ordering.
use crate::{
    bootstrap::{HydrationConfig, PinnedVersion, connect_hydration},
    config::AgentConfig,
    credentials::CredentialVault,
    events::{Bridge, Decoder},
    importer::{Importer, pin},
    instance::InstanceGuard,
    local::{Local, error},
    remote::Remote,
    shutdown::Shutdown,
    work::{Commits, Dispatch, Worker, metadata},
};
use rand_core::{OsRng, RngCore};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use vaultsyncd_agent_core::{
    adapters::ClientStorePorts,
    bindings::{control::BindingControls, import::FirstImport},
    engine::{
        events::EventReceiver,
        scheduler::{Jitter, Limits, Scheduler},
    },
    executor::{apply::ControlledApply, upload::UploadActionHandler},
    ports::Ports,
};
use vaultsyncd_client_store::{
    bindings::{self, NewBinding, SyncMode},
    db::Database,
    entries,
};
use vaultsyncd_domain::{
    ids::{BindingId, DeviceId, EntryId, IdSource, RootId, ServerId, VersionId},
    version::{BoundError, Clock, ServerEpoch, UtcTimestamp},
};
use vaultsyncd_http_client::{
    ApiClient, AuthSession, ClientConfig, CredentialSink, Credentials, ReqwestTransport, TlsTrust,
};
use vaultsyncd_platform_windows::{
    cfapi::{
        adapter::Callbacks,
        catalog::{Catalog, Identity},
    },
    files::{RootHandle, controlled::RecoveryOutcome},
};
use vaultsyncd_protocol::generated::SessionIssued;
use vaultsyncd_transfer::{
    download::DownloadOptions,
    ports::{PortError, VersionPin},
};
type StoredPin = (Vec<u8>, Vec<u8>, Vec<u8>, i64);
struct SystemClock;
impl Clock for SystemClock {
    fn now_utc(&self) -> Result<UtcTimestamp, BoundError> {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| BoundError::Negative)?
            .as_secs();
        UtcTimestamp::from_unix_seconds(i64::try_from(n).map_err(|_| BoundError::AboveDbLimit)?)
    }
}
struct Random;
impl Jitter for Random {
    fn sample(&self) -> u64 {
        OsRng.next_u64()
    }
}
impl IdSource for Random {
    fn next_id_bytes(&mut self) -> [u8; 16] {
        let mut b = [0; 16];
        OsRng.fill_bytes(&mut b);
        b
    }
}
struct VaultSink {
    vault: CredentialVault,
    failed: AtomicBool,
    pending: Mutex<Option<Vec<u8>>>,
}
impl VaultSink {
    fn persist(&self) -> Result<(), PortError> {
        let mut pending = self.pending.lock().map_err(error)?;
        if let Some(bytes) = &*pending {
            self.vault.store(bytes).map_err(error)?;
            *pending = None;
            self.failed.store(false, Ordering::Release);
        }
        Ok(())
    }
}
impl CredentialSink for VaultSink {
    fn credentials_stored(&self, c: &Credentials) {
        let issued = SessionIssued {
            session_id: c.session_id().into(),
            access_token: c.access_token().into(),
            refresh_token: c.refresh_token().into(),
            access_expires_at: c.access_expires_at().to_string(),
            refresh_expires_at: c.refresh_expires_at().to_string(),
        };
        match serde_json::to_vec(&issued) {
            Ok(bytes) => {
                if let Ok(mut p) = self.pending.lock() {
                    *p = Some(bytes);
                    self.failed.store(true, Ordering::Release);
                }
                if self.persist().is_err() {
                    self.failed.store(true, Ordering::Release);
                }
            }
            Err(_) => self.failed.store(true, Ordering::Release),
        }
    }
    fn credentials_erased(&self) {
        if self.vault.erase().is_err() {
            self.failed.store(true, Ordering::Release);
        }
    }
}
/// Run in the actual interactive user session. Authentication input is transient stdin only.
pub async fn run(config: AgentConfig, login: bool, once: bool) -> Result<(), PortError> {
    config.validate().map_err(error)?;
    let state = RootHandle::open(&config.state_dir).map_err(error)?;
    let mut roots = Vec::new();
    let mut caches = Vec::new();
    for b in &config.bindings {
        roots.push(RootHandle::open(&b.path).map_err(error)?);
        caches.push(RootHandle::open(&b.cache).map_err(error)?);
    }
    for (i, root) in roots.iter().enumerate() {
        if root.overlaps(&state)
            || caches.iter().any(|c| root.overlaps(c))
            || roots
                .iter()
                .enumerate()
                .any(|(j, r)| i != j && root.overlaps(r))
        {
            return Err(error("binding/state/cache overlap"));
        }
    }
    let _instance = InstanceGuard::acquire(&config.state_dir, "vaultsyncd-agent").map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists || e.raw_os_error() == Some(32) {
            PortError::rejected(
                "ALREADY_RUNNING",
                "another agent owns this session or state",
            )
        } else {
            error(e)
        }
    })?;
    let sink = Arc::new(VaultSink {
        vault: CredentialVault::new(&config.server.id).map_err(error)?,
        failed: AtomicBool::new(false),
        pending: Mutex::new(None),
    });
    let session = AuthSession::with_sink(sink.clone());
    let mut http = ClientConfig::new(&config.server.url);
    if config.server.development_http {
        http = http.allow_plaintext_http();
    }
    if let Some(ca) = &config.server.ca_file {
        http = http.with_trust(TlsTrust::system_roots_with_pem_file(ca).map_err(error)?);
    }
    let client = ApiClient::new(http)
        .map_err(error)?
        .with_auth_session(session.clone());
    if login {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(16 * 1024)
            .read_to_end(&mut bytes)
            .map_err(error)?;
        let request = serde_json::from_slice(&bytes).map_err(error)?;
        client
            .login(&request)
            .await
            .map_err(vaultsyncd_http_client::transfer_error)?;
        sink.persist()?;
        eprintln!("event=login-stored");
        return Ok(());
    }
    let stored = sink.vault.load().map_err(error)?.ok_or_else(|| {
        PortError::rejected(
            "REAUTH_REQUIRED",
            "login required in current Windows user session",
        )
    })?;
    let issued: SessionIssued =
        serde_json::from_slice(&stored).map_err(|_| error("invalid stored credential"))?;
    session.install(
        Credentials::from_issued(&issued).map_err(vaultsyncd_http_client::transfer_error)?,
    );
    sink.persist()?;
    let db = Arc::new(
        Database::open(config.state_dir.join("client.sqlite"))
            .await
            .map_err(error)?,
    );
    let shutdown = Shutdown::install().map_err(error)?;
    let mut live = Vec::new();
    for binding in &config.bindings {
        let id = BindingId::from_hex(&binding.id).map_err(error)?;
        let root = RootId::from_hex(&binding.root_id).map_err(error)?;
        let mode = if binding.mode == "full" {
            SyncMode::Full
        } else {
            SyncMode::OnDemand
        };
        let mut conn = db.pool().acquire().await.map_err(error)?;
        let old = bindings::binding(&mut conn, id).await.map_err(error)?;
        drop(conn);
        let epoch = if let Some(old) = &old {
            if old.server.to_string() != config.server.id
                || old.device.to_string() != config.device_id
                || old.root != root
                || old.local_path != binding.path.to_string_lossy()
                || old.cache_path != binding.cache.to_string_lossy()
                || old.mode != mode
            {
                return Err(error("configuration differs from durable binding"));
            }
            old.server_epoch
        } else {
            let (snapshot, _) = vaultsyncd_http_client::sync::snapshot(&client, &root.to_string())
                .await
                .map_err(vaultsyncd_http_client::transfer_error)?;
            let epoch = ServerEpoch::from_decimal(&snapshot.server_epoch).map_err(error)?;
            let mut tx = db.begin_write().await.map_err(error)?;
            bindings::insert_binding(
                &mut tx,
                &NewBinding {
                    id,
                    server: ServerId::from_hex(&config.server.id).map_err(error)?,
                    device: DeviceId::from_hex(&config.device_id).map_err(error)?,
                    root,
                    local_path: binding.path.to_string_lossy().into_owned(),
                    cache_path: binding.cache.to_string_lossy().into_owned(),
                    mode,
                    rules_version: "1".into(),
                    server_epoch: epoch,
                },
            )
            .await
            .map_err(error)?;
            tx.commit().await.map_err(error)?;
            epoch
        };
        // Metadata-only connection keeps offline CFAPI directory queries usable; content requests
        // remain explicitly unavailable until platform and core recovery have both completed.
        let mut recovery_connection =
            vaultsyncd_platform_windows::cfapi::adapter::Adapter::connect(
                &binding.path,
                Catalog::new(id, root),
                Arc::new(vaultsyncd_platform_windows::cfapi::adapter::MetadataOnly),
            )
            .map_err(error)?;
        let local = Arc::new(Local::open(&binding.path, &binding.cache, db.clone(), id).await?);
        for outcome in local.native.recover_publications().map_err(error)? {
            if matches!(outcome, RecoveryOutcome::Blocked(_)) {
                return Err(error("platform publication requires recovery"));
            }
        }
        recover_releases(&db, &binding.path, id, root, &client).await?;
        crate::namespace::recover(&local, &recovery_connection).await?;
        let recovered = ControlledApply::new(db.clone(), local.clone())
            .recover(id)
            .await?;
        for (op, result) in recovered {
            result?;
            local
                .native
                .acknowledge_publication(&op.to_string())
                .map_err(error)?;
        }
        // Release recovery is per persisted target/version and runs before any new scheduler work.
        let remote = Arc::new(Remote {
            client: client.clone(),
            binding: id,
            root,
        });
        let ports_store = Arc::new(ClientStorePorts::new(db.clone()));
        let ports = Ports::new(
            local.clone(),
            Arc::new(SystemClock),
            remote.clone(),
            ports_store.clone(),
            ports_store.clone(),
            ports_store,
        );
        let deferred = Dispatch(Arc::new(OnceLock::new()));
        let scheduler = Arc::new(Scheduler::new(
            db.clone(),
            ports.clone(),
            deferred.clone(),
            Arc::new(BindingControls::new(db.clone())),
            Arc::new(Random),
            Limits::default(),
        )?);
        let mut catalog = Catalog::new(id, root);
        let mut versions = Vec::new();
        let pins: Vec<StoredPin> = sqlx::query_as(
            "SELECT entry_id,version_id,digest,size FROM agent_versions WHERE binding_id=?",
        )
        .bind(id.as_bytes().as_slice())
        .fetch_all(db.pool())
        .await
        .map_err(error)?;
        for (entry, version, digest, size) in pins {
            let identity = Identity {
                binding: id,
                root,
                entry: EntryId::from_bytes(entry.try_into().map_err(|_| error("bad entry id"))?),
                version: VersionId::from_bytes(
                    version.try_into().map_err(|_| error("bad version id"))?,
                ),
            };
            let fixed = VersionPin::new(
                id.to_string(),
                identity.entry.to_string(),
                identity.version.to_string(),
                digest.try_into().map_err(|_| error("bad digest"))?,
                size as u64,
            )
            .map_err(error)?;
            versions.push(PinnedVersion::new(identity, fixed).map_err(error)?);
        }
        let placeholders = local.root.scan();
        if !placeholders.permits_missing_comparison() {
            return Err(error("incomplete startup metadata scan"));
        }
        for entry in placeholders.entries {
            if let Some(cloud) = entry.metadata.and_then(|m| m.cloud) {
                let identity = Identity::decode(&cloud.identity).map_err(error)?;
                if identity.binding != id || identity.root != root {
                    return Err(error("foreign placeholder"));
                }
                catalog.insert(identity).map_err(error)?;
            }
        }
        recovery_connection.disconnect().map_err(error)?;
        let (adapter, callbacks) = connect_hydration(
            &binding.path,
            catalog,
            db.clone(),
            client.clone(),
            scheduler.user_reads(),
            HydrationConfig {
                staging: binding.cache.clone(),
                versions,
                download: DownloadOptions::new(1024 * 1024),
                timeout: Duration::from_secs(30),
                outstanding: 32,
            },
        )
        .map_err(error)?;
        let importer = Arc::new(Importer {
            remote: remote.clone(),
            local,
            adapter: Arc::new(Mutex::new(adapter)),
            callbacks,
            content: vaultsyncd_agent_core::ports::remote::RemotePort::content(&*remote, id)
                .await?,
        });
        let uploads = Arc::new(UploadActionHandler::new(
            &config.device_id,
            epoch,
            Arc::new(Commits {
                importer: importer.clone(),
            }),
        ));
        let worker = Worker {
            importer: importer.clone(),
            uploads,
            mode,
        };
        deferred
            .0
            .set(worker.clone())
            .map_err(|_| error("handler already installed"))?;
        live.push((
            id,
            root,
            FirstImport::new(db.clone(), ports, importer.clone(), remote.clone()),
            worker,
            scheduler,
            remote,
        ));
    }
    let ipc =
        crate::ipc::start(db.clone(), live.iter().map(|(id, ..)| *id).collect()).map_err(error)?;
    eprintln!("event=recovery-complete bindings={}", live.len());
    let mut initialized = HashMap::new();
    let mut receivers = HashMap::new();
    loop {
        if shutdown.requested() {
            break;
        }
        if ipc.is_finished() {
            return Err(error("IPC listener stopped unexpectedly"));
        }
        sink.persist()?;
        if sink.failed.load(Ordering::Acquire) {
            return Err(error("credential persistence failed"));
        }
        for (id, root, first, worker, scheduler, remote) in &live {
            let result: Result<(), PortError> = async {
                if !initialized.contains_key(id) {
                    let report = first.run(*id, &mut Random, 128).await?;
                    if !report.complete {
                        return Ok(());
                    }
                    seed_uploaded_facts(&worker.importer).await?;
                    worker.restore_uploads().await?;
                    let decoder = Arc::new(Decoder {
                        rows: Mutex::new(metadata(&worker.importer).await?),
                        specifications: Mutex::new(HashMap::new()),
                    });
                    let bridge = Arc::new(Bridge {
                        worker: worker.clone(),
                        scheduler: scheduler.clone(),
                        decoder: decoder.clone(),
                    });
                    let receiver = EventReceiver::new(
                        db.clone(),
                        *root,
                        *id,
                        remote.clone(),
                        decoder,
                        bridge,
                        128,
                    )
                    .map_err(error)?;
                    receivers.insert(*id, receiver);
                    initialized.insert(*id, true);
                }
                scheduler.run_round(*id).await?;
                let received = receivers
                    .get(id)
                    .ok_or_else(|| error("receiver missing"))?
                    .run_cycle(None)
                    .await
                    .map_err(error);
                // An unavailable pull must not suppress durable local discovery. Conversely,
                // a local scan failure must not prevent already received work from progressing.
                let discovered = worker.discover().await;
                scheduler.run_round(*id).await?;
                received?;
                discovered?;
                Ok(())
            }
            .await;
            match result {
                Ok(()) => eprintln!("event=cycle binding={id}"),
                Err(e) => {
                    eprintln!(
                        "event=cycle-blocked binding={id} code={}",
                        e.code().unwrap_or("UNAVAILABLE")
                    );
                    if once {
                        return Err(e);
                    }
                }
            }
        }
        if once {
            break;
        }
        let end = std::time::Instant::now() + Duration::from_millis(config.poll_millis);
        while !shutdown.requested() && std::time::Instant::now() < end {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    for (_, _, _, worker, _, _) in &live {
        worker.importer.callbacks.disconnect();
        worker
            .importer
            .adapter
            .lock()
            .map_err(error)?
            .disconnect()
            .map_err(error)?;
        worker
            .importer
            .callbacks
            .wait_idle(Duration::from_secs(5))
            .await
            .map_err(error)?;
    }
    ipc.shutdown().await.map_err(error)?;
    drop(receivers);
    drop(live);
    db.pool().close().await;
    eprintln!("event=stopped");
    Ok(())
}
async fn seed_uploaded_facts(importer: &Importer) -> Result<(), PortError> {
    let rows = metadata(importer).await?;
    let paths = crate::work::paths(&rows)?;
    for (id, row) in rows {
        if row.kind != vaultsyncd_protocol::generated::EntryKind::File || row.hash.is_some() {
            continue;
        }
        let path = paths
            .get(&id)
            .ok_or_else(|| error("unknown imported path"))?;
        let snapshot = importer
            .local
            .observe_snapshot(path)
            .await?
            .ok_or_else(|| error("missing imported file"))?;
        let mut conn = importer.local.db.pool().acquire().await.map_err(error)?;
        let state = entries::local_entry(
            &mut conn,
            importer.local.binding,
            EntryId::from_hex(&id).map_err(error)?,
        )
        .await
        .map_err(error)?
        .ok_or_else(|| error("unknown import state"))?;
        drop(conn);
        if state.dirty != vaultsyncd_domain::state::Dirty::Clean
            || state.generation.get() != snapshot.evidence().generation
            || state.fingerprint.as_deref() != Some(snapshot.evidence().digest.as_slice())
        {
            return Err(error("unconfirmed imported content"));
        }
        let mut complete = row;
        complete.hash = Some(vaultsyncd_transfer::hash::to_hex(
            &snapshot.evidence().digest,
        ));
        complete.size = Some(snapshot.evidence().size.to_string());
        let fixed = pin(&importer.local, &complete).await?;
        importer.adopt(path, &fixed)?;
    }
    Ok(())
}
struct ReleaseMux {
    targets: HashMap<String, Arc<crate::release::NativeReleaseLocal>>,
}
impl vaultsyncd_agent_core::executor::release::ReleaseLocal for ReleaseMux {
    fn protect<'a>(
        &'a self,
        path: &'a vaultsyncd_fs_common::path::RelativePath,
    ) -> vaultsyncd_transfer::ports::PortFuture<
        'a,
        Result<Box<dyn vaultsyncd_agent_core::executor::release::ReleaseProtection>, PortError>,
    > {
        match self.targets.get(&path.to_string()) {
            Some(target) => target.protect(path),
            None => Box::pin(async { Err(error("unknown release recovery target")) }),
        }
    }
}
async fn recover_releases(
    db: &Arc<Database>,
    path: &Path,
    binding: BindingId,
    root: RootId,
    client: &ApiClient<ReqwestTransport>,
) -> Result<(), PortError> {
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM controlled_releases WHERE binding_id=? AND completed=0)",
    )
    .bind(binding.as_bytes().as_slice())
    .fetch_one(db.pool())
    .await
    .map_err(error)?;
    if !pending {
        return Ok(());
    }
    let specs =
        vaultsyncd_agent_core::executor::release::ManualRelease::pending_specs(db.clone(), binding)
            .await?;
    if specs.is_empty() {
        return Ok(());
    }
    let mut targets = HashMap::new();
    for spec in specs {
        let identity = Identity {
            binding,
            root,
            entry: spec.entry,
            version: VersionId::from_hex(&spec.pin.version).map_err(error)?,
        };
        targets.insert(
            spec.path.to_string(),
            Arc::new(crate::release::NativeReleaseLocal::new(
                path,
                db.clone(),
                identity,
                spec.path,
                spec.pin,
            )?),
        );
    }
    let remote = Arc::new(crate::release::HttpReleaseRemote::new(
        client.clone(),
        root,
    )?);
    for (_, result) in vaultsyncd_agent_core::executor::release::ManualRelease::new(
        db.clone(),
        Arc::new(ReleaseMux { targets }),
        remote,
    )
    .recover(binding)
    .await?
    {
        result?;
    }
    Ok(())
}
