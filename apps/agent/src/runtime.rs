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
    recovery::resync::rebuild_open,
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
type LiveBinding = (
    BindingId,
    RootId,
    FirstImport,
    Worker,
    Arc<Scheduler<Dispatch>>,
    Arc<Remote>,
);
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
pub async fn run(mut config: AgentConfig, login: bool, once: bool) -> Result<(), PortError> {
    config.validate().map_err(error)?;
    crate::maintenance::admit_startup(&config)?;
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
    let db = Arc::new(
        Database::open(config.state_dir.join("client.sqlite"))
            .await
            .map_err(error)?,
    );
    crate::management::load_connection(&db, &mut config).await?;
    let mut sink = Arc::new(VaultSink {
        vault: CredentialVault::new(&config.server.id).map_err(error)?,
        failed: AtomicBool::new(false),
        pending: Mutex::new(None),
    });
    let mut session = AuthSession::with_sink(sink.clone());
    let mut http = ClientConfig::new(&config.server.url);
    if config.server.development_http {
        http = http.allow_plaintext_http();
    }
    if let Some(ca) = &config.server.ca_file {
        http = http.with_trust(TlsTrust::system_roots_with_pem_file(ca).map_err(error)?);
    }
    let mut client = ApiClient::new(http)
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
    if let Some(stored) = sink.vault.load().map_err(error)? {
        let issued: SessionIssued =
            serde_json::from_slice(&stored).map_err(|_| error("invalid stored credential"))?;
        session.install(
            Credentials::from_issued(&issued).map_err(vaultsyncd_http_client::transfer_error)?,
        );
    }
    sink.persist()?;
    let shutdown = Shutdown::install().map_err(error)?;
    // DB is the authority for dynamically created bindings and mutable modes on restart.
    crate::management::load_bindings(&db, &mut config).await?;
    crate::management::admit(&config)?;
    let assemble = |config: AgentConfig,
                    binding: crate::config::BindingConfig,
                    db: Arc<Database>,
                    client: ApiClient<ReqwestTransport>| async move {
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
        // An open rebuild quarantines old operations from its first transaction; assembly must still
        // continue, otherwise admission could never resume the durable journal (and the fence itself
        // would block startup forever). Old operation recovery is deferred to the post-rebuild assembly.
        let rebuilding = rebuild_open(&db, id).await?;
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
        if rebuilding {
            eprintln!("event=rebuild-open binding={id} old-operations=quarantined");
        } else {
            recover_releases(&db, &binding.path, id, root, &client).await?;
        }
        crate::namespace::recover(&local, &recovery_connection).await?;
        if !rebuilding {
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
        };
        deferred
            .0
            .set(worker.clone())
            .map_err(|_| error("handler already installed"))?;
        eprintln!("event=recovery-complete binding={id}");
        Ok::<_, PortError>((
            id,
            root,
            FirstImport::new(db.clone(), ports, importer.clone(), remote.clone()),
            worker,
            scheduler,
            remote,
        ))
    };
    let mut live: Vec<LiveBinding> = Vec::new();
    let (commands, mut requests) = tokio::sync::mpsc::channel::<crate::ipc::CommandRequest>(8);
    let (connection_updates, connection) =
        tokio::sync::watch::channel(crate::management::connection(&config, &session));
    let (problem_updates, problems) = tokio::sync::watch::channel(Vec::new());
    let mut startup_problems: HashMap<BindingId, vaultsyncd_local_ipc::generated::Problem> =
        HashMap::new();
    let (offline_updates, offline) = tokio::sync::watch::channel(std::collections::HashSet::new());
    let mut offline_bindings = std::collections::HashSet::new();
    let mut cycle_problems: HashMap<BindingId, vaultsyncd_local_ipc::generated::Problem> =
        HashMap::new();
    let ipc =
        crate::ipc::start(db.clone(), commands, connection, problems, offline).map_err(error)?;
    eprintln!(
        "event=management-ready configured={}",
        config.bindings.len()
    );
    let mut initialized = HashMap::new();
    let mut receivers = HashMap::new();
    let mut hints = HashMap::new();
    let (hint_wake, mut hint_woken) = tokio::sync::mpsc::channel(1);
    let mut maintenance_stop = false;
    loop {
        if shutdown.requested() {
            break;
        }
        if ipc.is_finished() {
            return Err(error("IPC listener stopped unexpectedly"));
        }
        sink.persist()?;
        connection_updates.send_replace(crate::management::connection(&config, &session));
        if sink.failed.load(Ordering::Acquire) {
            return Err(error("credential persistence failed"));
        }
        while let Ok(command) = requests.try_recv() {
            if command.reply.is_closed() {
                continue;
            }
            let request = command.request;
            let mut result = if request.command.name
                == vaultsyncd_local_ipc::generated::CommandName::PrepareMaintenance
            {
                let result = crate::maintenance_prepare::prepare(
                    &db,
                    &config,
                    live.iter()
                        .map(|(id, _, _, worker, scheduler, _)| {
                            (*id, (worker.clone(), scheduler.clone()))
                        })
                        .collect(),
                    request
                        .command
                        .parameters
                        .as_ref()
                        .unwrap()
                        .destination
                        .as_deref()
                        .unwrap(),
                )
                .await;
                maintenance_stop = true;
                result
            } else if request.command.name
                == vaultsyncd_local_ipc::generated::CommandName::SetConnection
            {
                async {
                    let proposed =
                        crate::management::proposed_connection(&db, &config, &request.command)
                            .await?;
                    let next_sink = Arc::new(VaultSink {
                        vault: CredentialVault::new(&proposed.server.id).map_err(error)?,
                        failed: AtomicBool::new(false),
                        pending: Mutex::new(None),
                    });
                    let next_session = AuthSession::with_sink(next_sink.clone());
                    let mut http = ClientConfig::new(&proposed.server.url);
                    if proposed.server.development_http {
                        http = http.allow_plaintext_http();
                    }
                    if let Some(ca) = &proposed.server.ca_file {
                        http = http
                            .with_trust(TlsTrust::system_roots_with_pem_file(ca).map_err(error)?);
                    }
                    let next_client = ApiClient::new(http)
                        .map_err(error)?
                        .with_auth_session(next_session.clone());
                    if let Some(bytes) = next_sink.vault.load().map_err(error)? {
                        let issued: SessionIssued = serde_json::from_slice(&bytes)
                            .map_err(|_| error("invalid stored credential"))?;
                        next_session.install(
                            Credentials::from_issued(&issued)
                                .map_err(vaultsyncd_http_client::transfer_error)?,
                        );
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
                    receivers.clear();
                    hints.clear();
                    initialized.clear();
                    live.clear();
                    crate::management::save_connection(&db, &proposed).await?;
                    config = proposed;
                    client = next_client;
                    session = next_session;
                    sink = next_sink;
                    Ok(vaultsyncd_local_ipc::generated::ManagementResult {
                        connection: Some(crate::management::connection(&config, &session)),
                        ..Default::default()
                    })
                }
                .await
            } else if request.command.name
                == vaultsyncd_local_ipc::generated::CommandName::CreateBinding
            {
                async {
                    if !session.has_credentials() {
                        return Err(PortError::rejected("REAUTH_REQUIRED", "login required"));
                    }
                    let binding = crate::management::new_binding(&config, &request.command).await?;
                    let value =
                        assemble(config.clone(), binding.clone(), db.clone(), client.clone())
                            .await?;
                    config.bindings.push(binding);
                    live.push(value);
                    Ok(vaultsyncd_local_ipc::generated::ManagementResult::default())
                }
                .await
            } else {
                crate::management::execute(
                    &db,
                    &config,
                    &client,
                    &session,
                    live.iter()
                        .map(|(id, _, _, worker, scheduler, _)| {
                            (*id, (worker.clone(), scheduler.clone()))
                        })
                        .collect(),
                    &request.command,
                )
                .await
            };
            if request.command.name == vaultsyncd_local_ipc::generated::CommandName::ListProblems
                && let Ok(result) = &mut result
            {
                let parameters = request.command.parameters.as_ref();
                result.problems.get_or_insert_default().extend(
                    startup_problems
                        .values()
                        .filter(|p| {
                            parameters
                                .and_then(|p| p.binding_id.as_ref())
                                .is_none_or(|b| b == &p.binding_id)
                                && parameters.and_then(|p| p.search.as_ref()).is_none_or(|s| {
                                    p.code.to_lowercase().contains(&s.to_lowercase())
                                })
                        })
                        .cloned(),
                );
            }
            if result.is_ok() {
                sink.persist()?;
                if sink.failed.load(Ordering::Acquire) {
                    return Err(error("credential persistence failed"));
                }
            }
            connection_updates.send_replace(crate::management::connection(&config, &session));
            let _ = command
                .reply
                .send(crate::ipc::management_response(&request, result));
            if maintenance_stop {
                break;
            }
        }
        if maintenance_stop {
            // Let the bounded transport flush the successful preparation acknowledgment.
            tokio::time::sleep(Duration::from_millis(150)).await;
            break;
        }
        if session.has_credentials() {
            for binding in &config.bindings {
                let id = BindingId::from_hex(&binding.id).map_err(error)?;
                if live.iter().any(|(existing, ..)| *existing == id) {
                    continue;
                }
                let mut conn = db.pool().acquire().await.map_err(error)?;
                let detached: Option<bool> =
                    sqlx::query_scalar("SELECT detached FROM binding_controls WHERE binding_id=?")
                        .bind(id.as_bytes().as_slice())
                        .fetch_optional(&mut *conn)
                        .await
                        .map_err(error)?;
                drop(conn);
                if detached == Some(true) {
                    continue;
                }
                match assemble(config.clone(), binding.clone(), db.clone(), client.clone()).await {
                    Ok(value) => {
                        live.push(value);
                        startup_problems.remove(&id);
                    }
                    Err(e) if e.is_retryable() => {
                        offline_bindings.insert(id);
                        if once {
                            return Err(e);
                        }
                    }
                    Err(e) => {
                        let code = e
                            .code()
                            .filter(|s| vaultsyncd_domain::error::ErrorCode::from_code(s).is_some())
                            .unwrap_or("NEEDS_RECOVERY");
                        startup_problems.insert(
                            id,
                            vaultsyncd_local_ipc::generated::Problem {
                                binding_id: id.to_string(),
                                problem_id: format!("startup:{id}"),
                                entry_id: None,
                                path: Some(String::new()),
                                code: code.into(),
                                reason: "绑定尚未完成恢复或连接，不能显示为同步完成".into(),
                                local_data: "本地目录、缓存与待办保留；尚未取得安全恢复证明".into(),
                                next_step:
                                    "核对连接、证书、目录权限和恢复现场，恢复后 agent 会重新检查"
                                        .into(),
                                retryable: false,
                            },
                        );
                        if once {
                            return Err(e);
                        }
                        eprintln!(
                            "event=binding-start-blocked binding={id} code={}",
                            e.code().unwrap_or("UNAVAILABLE")
                        );
                    }
                }
            }
        }
        problem_updates.send_replace(startup_problems.values().cloned().collect());
        let mut rebuilt = Vec::new();
        for (id, root, first, worker, scheduler, remote) in &live {
            let mut observed = false;
            let result: Result<(), PortError> = async {
                if !session.has_credentials()
                    || BindingControls::new(db.clone())
                        .execution_gate(*id, None)
                        .await?
                        != vaultsyncd_agent_core::bindings::control::ExecutionGate::Ready
                {
                    return Ok(());
                }
                observed = true;
                hints.entry(*id).or_insert_with(|| {
                    crate::events::Hints::start(remote.clone(), hint_wake.clone())
                });
                worker.importer.local.root.verify().map_err(|_| {
                    PortError::rejected("ROOT_UNAVAILABLE", "root identity changed or unavailable")
                })?;
                match crate::resync::admit(db.clone(), *id, worker, remote.clone()).await {
                    Ok(true) => {
                        rebuilt.push(*id);
                        return Ok(());
                    }
                    Ok(false) => {}
                    Err(e) => {
                        // Preserve offline discovery, but do not dispatch any remote command until
                        // the authenticated epoch and durable event window have been checked.
                        if e.is_retryable() {
                            let fenced: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM binding_resyncs WHERE binding_id=? AND complete=0)")
                                .bind(id.as_bytes().as_slice()).fetch_one(db.pool()).await.map_err(error)?;
                            if !fenced {
                                worker.discover().await?;
                            }
                        }
                        return Err(e);
                    }
                }
                if !initialized.contains_key(id) {
                    let report = first.run(*id, &mut Random, 128).await?;
                    if !report.complete {
                        return Ok(());
                    }
                    crate::management_history::report_existing_conflicts(
                        &db,
                        &client,
                        &root.to_string(),
                    )
                    .await?;
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
                // Full-mode proof runs only after durable local discovery and once no
                // upload/intent is unsettled: an unreachable server must not hide new local
                // content, and a dirty edit must not be mistaken for an unapplied remote version.
                let unsettled: i64 = sqlx::query_scalar(
                    "SELECT (SELECT count(*) FROM pending_ops WHERE binding_id=?) + \
                     (SELECT count(*) FROM local_intents WHERE binding_id=?)",
                )
                .bind(id.as_bytes().as_slice())
                .bind(id.as_bytes().as_slice())
                .fetch_one(db.pool())
                .await
                .map_err(error)?;
                if unsettled == 0 {
                    let mode = BindingControls::new(db.clone()).mode_progress(*id).await?;
                    if mode.mode == SyncMode::Full && !mode.complete {
                        BindingControls::new(db.clone())
                            .resume_mode(
                                *id,
                                &crate::management_files::Materializer(worker.clone()),
                                16,
                            )
                            .await?;
                    }
                }
                Ok(())
            }
            .await;
            match result {
                Ok(()) => {
                    if observed {
                        cycle_problems.remove(id);
                        offline_bindings.remove(id);
                    }
                    eprintln!("event=cycle binding={id}");
                }
                Err(e) => {
                    if e.is_retryable() {
                        offline_bindings.insert(*id);
                    } else if let Some(code) = e
                        .code()
                        .and_then(vaultsyncd_domain::error::ErrorCode::from_code)
                        .filter(|c| c.class().is_severe())
                    {
                        cycle_problems.insert(
                            *id,
                            vaultsyncd_local_ipc::generated::Problem {
                                binding_id: id.to_string(),
                                problem_id: format!("cycle:{id}:{code}"),
                                entry_id: None,
                                path: None,
                                code: code.to_string(),
                                reason: format!("同步受阻（{code}）"),
                                local_data: "本地内容、队列、缓存和恢复现场保留".into(),
                                next_step: "恢复存储、挂载或权限后重新核对；不要删除未确认内容"
                                    .into(),
                                retryable: false,
                            },
                        );
                    }
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
        for id in rebuilt {
            initialized.remove(&id);
            receivers.remove(&id);
            hints.remove(&id);
            if let Some(index) = live.iter().position(|item| item.0 == id) {
                let (_, _, _, worker, _, _) = live.remove(index);
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
        }
        offline_updates.send_replace(offline_bindings.clone());
        match crate::issue_projection::sync(&db, &config, &startup_problems, &cycle_problems).await
        {
            Ok(problems) => {
                problem_updates.send_replace(problems);
            }
            Err(_) => {
                problem_updates.send_replace(
                    startup_problems
                        .values()
                        .chain(cycle_problems.values())
                        .cloned()
                        .collect(),
                );
                eprintln!("event=problem-persistence-unavailable code=NEEDS_RECOVERY");
            }
        }
        if once {
            if !session.has_credentials() {
                return Err(PortError::rejected("REAUTH_REQUIRED", "login required"));
            }
            break;
        }
        let end = std::time::Instant::now() + Duration::from_millis(config.poll_millis);
        while !shutdown.requested() && requests.is_empty() && std::time::Instant::now() < end {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                _ = hint_woken.recv() => break,
            }
        }
    }
    drop(hints);
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
