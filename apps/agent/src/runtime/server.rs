//! A server owns its authentication, worker lifecycle and remote reconciliation cadence.
use super::*;
use crate::{
    config::{BindingConfig, ServerConfig},
    management::{self, Running, reject},
};
use std::collections::HashSet;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use vaultsyncd_local_ipc::generated::{CommandName, ConnectionInfo, ManagementResult, Problem};

type Receiver = EventReceiver<Remote, Decoder, Bridge>;

pub(super) struct Job {
    pub request: crate::ipc::CommandRequest,
    pub config: AgentConfig,
    pub create: Option<(BindingConfig, oneshot::Sender<()>)>,
}

#[derive(Clone, PartialEq)]
pub(super) struct Report {
    pub connection: ConnectionInfo,
    pub startup: HashMap<BindingId, Problem>,
    pub cycles: HashMap<BindingId, Problem>,
    pub offline: HashSet<BindingId>,
}

pub(super) struct Handle {
    pub commands: mpsc::Sender<Job>,
    pub adopt: mpsc::Sender<BindingConfig>,
    pub report: watch::Receiver<Report>,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), PortError>>,
}

impl Handle {
    pub fn task_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub fn into_task(self) -> JoinHandle<Result<(), PortError>> {
        // Keep both admission channels alive until the single round finishes.
        let Self {
            commands,
            adopt,
            report,
            stop,
            task,
        } = self;
        tokio::spawn(async move {
            let _commands = commands;
            let _adopt = adopt;
            let _report = report;
            let _stop = stop;
            task.await.map_err(error)?
        })
    }

    pub fn stop(self) -> JoinHandle<Result<(), PortError>> {
        let _ = self.stop.send(());
        self.task
    }
}

pub(super) struct Connection {
    server: ServerConfig,
    client: Result<ApiClient<ReqwestTransport>, PortError>,
    session: AuthSession,
    sink: Arc<VaultSink>,
}

impl Connection {
    pub fn open(server: ServerConfig) -> Result<Self, PortError> {
        let sink = Arc::new(VaultSink {
            vault: CredentialVault::new(&server.id).map_err(error)?,
            failed: AtomicBool::new(false),
            pending: Mutex::new(None),
        });
        let session = AuthSession::with_sink(sink.clone());
        let client = management::server_client(&server, session.clone());
        if client.is_ok() {
            // The pinned client proves the remote identity on its next business request.
            // Restore into memory only. Do not rewrite existing credentials on startup.
            let restored = (|| {
                let Some(bytes) = sink.vault.load().map_err(error)? else {
                    return Ok(None);
                };
                let issued: SessionIssued = serde_json::from_slice(&bytes)
                    .map_err(|_| error("invalid stored credential"))?;
                Credentials::from_issued(&issued)
                    .map(Some)
                    .map_err(vaultsyncd_http_client::transfer_error)
            })();
            match restored {
                Ok(Some(credential)) => {
                    session.restore(credential);
                }
                Ok(None) => {}
                Err(_) => eprintln!("event=credential-load-failed server={}", server.id),
            }
        }
        Ok(Self {
            server,
            client,
            session,
            sink,
        })
    }

    pub fn info(&self) -> ConnectionInfo {
        management::connection(&self.server, self.client.is_ok().then_some(&self.session))
    }

    /// Best-effort remote revocation before the local connection is forgotten.
    ///
    /// Returns `true` only when no local session existed or the server confirmed it.
    pub async fn revoke_session(&self) -> bool {
        if !self.session.has_credentials() {
            return true;
        }
        match &self.client {
            Ok(client) => client.logout().await.is_ok(),
            Err(_) => false,
        }
    }

    /// Remove this server's stored session. Called even when revocation could not be confirmed.
    pub fn erase_credentials(&self) {
        // Clearing the session notifies the credential sink, which erases the Windows entry.
        self.session.clear();
    }

    pub async fn login_stdin(&self) -> Result<(), PortError> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(16 * 1024)
            .read_to_end(&mut bytes)
            .map_err(error)?;
        let request = serde_json::from_slice(&bytes).map_err(error)?;
        self.client
            .as_ref()
            .map_err(Clone::clone)?
            .login(&request)
            .await
            .map_err(management::connection_error)?;
        self.check_credentials()?;
        eprintln!("event=login-stored");
        Ok(())
    }

    fn check_credentials(&self) -> Result<(), PortError> {
        if self.sink.persist().is_err() || self.sink.failed.load(Ordering::Acquire) {
            self.session.clear();
            return Err(error("credential persistence failed"));
        }
        Ok(())
    }
}

pub(super) fn start(
    db: Arc<Database>,
    config: AgentConfig,
    connection: Connection,
    bindings: Vec<BindingConfig>,
    reports_wake: mpsc::Sender<()>,
    connections: watch::Sender<Vec<ConnectionInfo>>,
    once: bool,
) -> Handle {
    let (commands, mut requests) = mpsc::channel::<Job>(8);
    let (adopt, mut adoptions) = mpsc::channel::<BindingConfig>(1024);
    let (stop, mut stopping) = oneshot::channel();
    let (reporting, report) = watch::channel(Report {
        connection: connection.info(),
        startup: HashMap::new(),
        cycles: HashMap::new(),
        offline: HashSet::new(),
    });
    let (hint_wake, mut hint_woken) = mpsc::channel(1);
    let mut server = Server {
        connection,
        bindings,
        uncertain: HashMap::new(),
        live: Vec::new(),
        receivers: HashMap::new(),
        hints: HashMap::new(),
        hint_wake,
        startup: HashMap::new(),
        cycles: HashMap::new(),
        offline: HashSet::new(),
    };
    publish_connection(&connections, report.borrow().connection.clone());
    let task = tokio::spawn(async move {
        let result = async {
            let mut next = tokio::time::Instant::now();
            loop {
                let response = tokio::select! {
                    biased;
                    _ = &mut stopping => break,
                    binding = adoptions.recv() => {
                        let Some(binding) = binding else { break };
                        let id = BindingId::from_hex(&binding.id).map_err(error)?;
                        if !server.bindings.iter().any(|known| known.id == binding.id) {
                            server.uncertain.insert(id, binding);
                        }
                        server.reconcile_uncertain(&db, &config.device_id).await?;
                        None
                    }
                    job = requests.recv() => {
                        let Some(job) = job else { break };
                        server.command(&db, job).await
                    }
                    _ = tokio::time::sleep_until(next) => {
                        if !crate::maintenance::trial_pending(&config.state_dir).map_err(error)? {
                            server.round(&db, &config.device_id, once).await?;
                        }
                        if once {
                            if !server.authenticated() {
                                return Err(reject("REAUTH_REQUIRED"));
                            }
                            break;
                        }
                        next = tokio::time::Instant::now() + Duration::from_millis(config.poll_millis);
                        None
                    }
                    _ = hint_woken.recv() => {
                        next = tokio::time::Instant::now();
                        None
                    }
                };
                let updated = server.report();
                publish_connection(&connections, updated.connection.clone());
                if reporting.send_if_modified(|previous| update(previous, updated)) {
                    let _ = reports_wake.try_send(());
                }
                if let Some((request, result)) = response {
                    let _ = request.reply.send(crate::ipc::management_response(&request.request, result));
                }
            }
            Ok(())
        }.await;
        let disconnected = server.disconnect().await;
        let mut connection = server.connection.info();
        connection.authenticated = false;
        publish_connection(&connections, connection);
        let _ = reports_wake.try_send(());
        if let Err(failure) = result {
            eprintln!(
                "event=server-worker-blocked server={} code={}",
                server.connection.server.id,
                failure.code().unwrap_or("UNAVAILABLE")
            );
            if once {
                return Err(failure);
            }
        }
        disconnected
    });
    Handle {
        commands,
        adopt,
        report,
        stop,
        task,
    }
}

fn publish_connection(updates: &watch::Sender<Vec<ConnectionInfo>>, value: ConnectionInfo) {
    updates.send_if_modified(|connections| {
        match connections.binary_search_by(|connection| connection.server_id.cmp(&value.server_id))
        {
            Ok(index) => update(&mut connections[index], value),
            Err(index) => {
                connections.insert(index, value);
                true
            }
        }
    });
}

struct Server {
    connection: Connection,
    bindings: Vec<BindingConfig>,
    uncertain: HashMap<BindingId, BindingConfig>,
    live: Vec<LiveBinding>,
    receivers: HashMap<BindingId, Receiver>,
    hints: HashMap<BindingId, crate::events::Hints>,
    hint_wake: mpsc::Sender<()>,
    startup: HashMap<BindingId, Problem>,
    cycles: HashMap<BindingId, Problem>,
    offline: HashSet<BindingId>,
}

impl Server {
    fn authenticated(&self) -> bool {
        self.connection.client.is_ok()
            && self.connection.session.has_credentials()
            && !self.connection.session.needs_reauth()
            && !self.connection.sink.failed.load(Ordering::Acquire)
    }

    fn report(&self) -> Report {
        let mut offline = self.offline.clone();
        if !self.authenticated() {
            offline.extend(
                self.bindings
                    .iter()
                    .filter_map(|b| BindingId::from_hex(&b.id).ok()),
            );
        }
        Report {
            connection: self.connection.info(),
            startup: self.startup.clone(),
            cycles: self.cycles.clone(),
            offline,
        }
    }

    fn running(&self) -> Running {
        self.live
            .iter()
            .map(|(id, _, _, worker, scheduler, _)| (*id, (worker.clone(), scheduler.clone())))
            .collect()
    }

    async fn command(
        &mut self,
        db: &Arc<Database>,
        job: Job,
    ) -> Option<(
        crate::ipc::CommandRequest,
        Result<ManagementResult, PortError>,
    )> {
        let Job {
            request,
            config,
            create,
        } = job;
        if request.reply.is_closed() {
            return None;
        }
        let command = &request.request.command;
        if matches!(command.name, CommandName::Login | CommandName::Logout) {
            self.hints.clear();
        }
        let quiesced = if command.name == CommandName::DetachBinding {
            match command
                .parameters
                .as_ref()
                .and_then(|p| p.binding_id.as_deref())
            {
                Some(id) => match BindingId::from_hex(id).map_err(error) {
                    Ok(id) => self.quiesce_binding(id).await,
                    Err(cause) => Err(cause),
                },
                None => Err(reject("INVALID_ARGUMENT")),
            }
        } else {
            Ok(())
        };
        let mut result = if let Err(cause) = quiesced {
            Err(cause)
        } else if let Some((binding, complete)) = create {
            let result = self.create(db, &config.device_id, binding).await;
            let _ = complete.send(());
            result
        } else {
            let running = self.running();
            match self.connection.client.as_ref() {
                Err(cause)
                    if matches!(
                        command.name,
                        CommandName::Login | CommandName::Logout | CommandName::ListVaults
                    ) =>
                {
                    Err(cause.clone())
                }
                client => {
                    management::execute(
                        db,
                        &config,
                        client.ok().map(|client| {
                            (
                                &mut self.connection.server,
                                client,
                                &self.connection.session,
                            )
                        }),
                        running,
                        command,
                    )
                    .await
                }
            }
        };
        if command.name == CommandName::ListProblems
            && let Ok(result) = &mut result
        {
            let parameters = command.parameters.as_ref();
            result.problems.get_or_insert_default().extend(
                self.startup
                    .values()
                    .filter(|problem| {
                        parameters
                            .and_then(|p| p.binding_id.as_ref())
                            .is_none_or(|id| id == &problem.binding_id)
                            && parameters
                                .and_then(|p| p.search.as_ref())
                                .is_none_or(|search| {
                                    problem.code.to_lowercase().contains(&search.to_lowercase())
                                })
                    })
                    .cloned(),
            );
        }
        let persistence = self.connection.check_credentials();
        let result = result.and_then(|value| persistence.map(|()| value));
        if !self.authenticated() {
            self.hints.clear();
        }
        Some((request, result))
    }

    async fn create(
        &mut self,
        db: &Arc<Database>,
        device: &str,
        binding: BindingConfig,
    ) -> Result<ManagementResult, PortError> {
        if !self.authenticated() {
            return Err(reject("REAUTH_REQUIRED"));
        }
        let id = BindingId::from_hex(&binding.id).map_err(error)?;
        let server = ServerId::from_hex(&self.connection.server.id).map_err(error)?;
        let client = self.connection.client.as_ref().map_err(Clone::clone)?;
        let result = assemble(device, server, &binding, db, client).await;
        if result.is_ok() {
            self.bindings.push(binding);
        } else {
            match management::binding_server(db, id, device).await {
                Ok(Some(owner)) if owner == server => self.bindings.push(binding),
                Ok(None) => {}
                Ok(Some(_)) => return Err(reject("BINDING_BLOCKED")),
                Err(cause) => {
                    // Remember the candidate until the durable outcome can be read again.
                    // A candidate with no stored row is discarded and never created by polling.
                    self.uncertain.insert(id, binding);
                    return Err(cause);
                }
            }
        }
        self.live.push(result?);
        Ok(ManagementResult::default())
    }

    /// Release the old native writer before publishing detached=1. The coordinator may
    /// admit a new binding as soon as it observes that durable flag, even on another server.
    async fn quiesce_binding(&mut self, id: BindingId) -> Result<(), PortError> {
        if let Some(index) = self.live.iter().position(|item| item.0 == id) {
            let worker = &self.live[index].3;
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
            self.live.remove(index);
        }
        self.receivers.remove(&id);
        self.hints.remove(&id);
        self.startup.remove(&id);
        self.cycles.remove(&id);
        self.offline.remove(&id);
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), PortError> {
        self.hints.clear();
        for (_, _, _, worker, _, _) in &self.live {
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
        self.receivers.clear();
        self.live.clear();
        Ok(())
    }

    async fn reconcile_uncertain(&mut self, db: &Database, device: &str) -> Result<(), PortError> {
        let server = ServerId::from_hex(&self.connection.server.id).map_err(error)?;
        for id in self.uncertain.keys().copied().collect::<Vec<_>>() {
            match management::binding_server(db, id, device).await {
                Ok(Some(owner)) if owner == server => {
                    if let Some(binding) = self.uncertain.remove(&id) {
                        self.bindings.push(binding);
                    }
                }
                Ok(None) => {
                    self.uncertain.remove(&id);
                }
                Ok(Some(_)) => return Err(reject("BINDING_BLOCKED")),
                Err(_) => {
                    eprintln!("event=binding-creation-outcome-unavailable binding={id}");
                }
            }
        }
        Ok(())
    }

    async fn round(
        &mut self,
        db: &Arc<Database>,
        device: &str,
        once: bool,
    ) -> Result<(), PortError> {
        self.reconcile_uncertain(db, device).await?;
        if self.connection.check_credentials().is_err() || !self.authenticated() {
            self.hints.clear();
            return Ok(());
        }
        let client = self.connection.client.as_ref().map_err(Clone::clone)?;
        let server = ServerId::from_hex(&self.connection.server.id).map_err(error)?;
        for binding in &self.bindings {
            let id = BindingId::from_hex(&binding.id).map_err(error)?;
            if self.live.iter().any(|(existing, ..)| *existing == id) {
                continue;
            }
            let detached: Option<bool> =
                sqlx::query_scalar("SELECT detached FROM binding_controls WHERE binding_id=?")
                    .bind(id.as_bytes().as_slice())
                    .fetch_optional(db.pool())
                    .await
                    .map_err(error)?;
            if detached == Some(true) {
                continue;
            }
            match assemble(device, server, binding, db, client).await {
                Ok(value) => {
                    self.live.push(value);
                    self.startup.remove(&id);
                }
                Err(e) if e.is_retryable() => {
                    self.offline.insert(id);
                    if once {
                        return Err(e);
                    }
                }
                Err(e) => {
                    let code = e
                        .code()
                        .filter(|s| vaultsyncd_domain::error::ErrorCode::from_code(s).is_some())
                        .unwrap_or("NEEDS_RECOVERY");
                    self.startup.insert(
                        id,
                        Problem {
                            binding_id: id.to_string(),
                            problem_id: format!("startup:{id}"),
                            entry_id: None,
                            path: Some(String::new()),
                            code: code.into(),
                            reason: "绑定尚未完成恢复或连接，不能显示为同步完成".into(),
                            local_data: "本地目录、缓存与待办保留；尚未取得安全恢复证明".into(),
                            next_step: "核对连接、证书、目录权限和恢复现场，恢复后 agent 会重新检查".into(),
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
        let mut rebuilt = Vec::new();
        for (id, vault, first, worker, scheduler, remote) in &self.live {
            let mut observed = false;
            let result: Result<(), PortError> = async {
                if self.connection.session.needs_reauth() || !self.connection.session.has_credentials()
                    || !crate::exclusions::Rules::load(db, *id).await?.active
                {
                    return Ok(());
                }
                observed = true;
                self.hints.entry(*id).or_insert_with(|| {
                    crate::events::Hints::start(remote.clone(), self.hint_wake.clone())
                });
                worker.importer.local.vault.verify().map_err(|_| {
                    PortError::rejected("VAULT_UNAVAILABLE", "vault identity changed or unavailable")
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
                if !self.receivers.contains_key(id) {
                    let report = first.run(*id, &mut Random, 128).await?;
                    if !report.complete {
                        return Ok(());
                    }
                    crate::management_history::report_existing_conflicts(
                        db,
                        &remote.client,
                        server,
                        &vault.to_string(),
                    )
                    .await?;
                    seed_uploaded_facts(&worker.importer).await?;
                    worker.restore_uploads().await?;
                    let decoder = Arc::new(Decoder::open(db, *id, metadata(&worker.importer).await?).await?);
                    let bridge = Arc::new(Bridge {
                        worker: worker.clone(),
                        scheduler: scheduler.clone(),
                        decoder: decoder.clone(),
                    });
                    let receiver = EventReceiver::new(
                        db.clone(),
                        *vault,
                        *id,
                        remote.clone(),
                        decoder,
                        bridge,
                        128,
                    )
                    .map_err(error)?;
                    self.receivers.insert(*id, receiver);
                }
                scheduler.run_round(*id).await?;
                let received = self.receivers
                    .get(id)
                    .ok_or_else(|| error("receiver missing"))?
                    .run_cycle(None)
                    .await
                    .map_err(error);
                // An unavailable pull must not suppress durable local discovery. Conversely,
                // a local scan failure must not prevent already received work from progressing.
                if received.is_ok() { crate::exclusions::reconcile(worker).await?; }
                let discovered = worker.discover().await;
                scheduler.run_round(*id).await?;
                received?;
                discovered?;
                // Full-mode proof runs only after durable local discovery and once no
                // upload/intent is unsettled: an unreachable server must not hide new local
                // content, and a dirty edit must not be mistaken for an unapplied remote version.
                // Entries parked by an exclusion stay queued but are never dispatched until
                // re-inclusion is reconciled; they must not block unrelated full-mode work.
                let unsettled = crate::exclusions::unsettled(db, *id).await?;
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
                        self.cycles.remove(id);
                        self.offline.remove(id);
                    }
                    eprintln!("event=cycle binding={id}");
                }
                Err(e) => {
                    self.offline.insert(*id);
                    if let Some(code) = e
                        .code()
                        .and_then(vaultsyncd_domain::error::ErrorCode::from_code)
                        .filter(|c| c.class().is_severe())
                    {
                        self.cycles.insert(
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
            self.receivers.remove(&id);
            self.hints.remove(&id);
            if let Some(index) = self.live.iter().position(|item| item.0 == id) {
                let (_, _, _, worker, _, _) = self.live.remove(index);
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
        Ok(())
    }
}

async fn assemble(
    device: &str,
    server: ServerId,
    binding: &BindingConfig,
    db: &Arc<Database>,
    client: &ApiClient<ReqwestTransport>,
) -> Result<LiveBinding, PortError> {
    let id = BindingId::from_hex(&binding.id).map_err(error)?;
    let vault = VaultId::from_hex(&binding.vault_id).map_err(error)?;
    let mode = if binding.mode == "full" {
        SyncMode::Full
    } else {
        SyncMode::OnDemand
    };
    let mut conn = db.pool().acquire().await.map_err(error)?;
    let old = bindings::binding(&mut conn, id).await.map_err(error)?;
    drop(conn);
    let epoch = if let Some(old) = &old {
        if old.server.to_string() != server.to_string()
            || old.device.to_string() != device
            || old.vault != vault
            || old.local_path != binding.path.to_string_lossy()
            || old.cache_path != binding.cache.to_string_lossy()
        {
            return Err(error("configuration differs from durable binding"));
        }
        old.server_epoch
    } else {
        let (snapshot, _) = vaultsyncd_http_client::sync::snapshot(client, &vault.to_string())
            .await
            .map_err(vaultsyncd_http_client::transfer_error)?;
        let epoch = ServerEpoch::from_decimal(&snapshot.server_epoch).map_err(error)?;
        let mut tx = db.begin_write().await.map_err(error)?;
        bindings::insert_binding(
            &mut tx,
            &NewBinding {
                id,
                server,
                device: DeviceId::from_hex(device).map_err(error)?,
                vault,
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
    // Adopt the schema-fenced effective policy before recovery or any new worker activity.
    BindingControls::new(db.clone())
        .adopt_default_policy(id)
        .await?;
    // Metadata-only connection keeps offline CFAPI directory queries usable; content requests
    // remain explicitly unavailable until platform and core recovery have both completed.
    // An open rebuild quarantines old operations from its first transaction; assembly must still
    // continue, otherwise admission could never resume the durable journal (and the fence itself
    // would block startup forever). Old operation recovery is deferred to the post-rebuild assembly.
    let rebuilding = rebuild_open(db, id).await?;
    let mut recovery_connection = vaultsyncd_platform_windows::cfapi::adapter::Adapter::connect(
        &binding.path,
        Catalog::new(id, vault),
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
        recover_releases(db, &binding.path, id, vault, client).await?;
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
        vault,
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
        Arc::new(crate::exclusions::Gate(db.clone())),
        Arc::new(Random),
        Limits::default(),
    )?);
    let mut catalog = Catalog::new(id, vault);
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
            vault,
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
    let placeholders = local.vault.scan();
    if !placeholders.permits_missing_comparison() {
        return Err(error("incomplete startup metadata scan"));
    }
    for entry in placeholders.entries {
        if let Some(cloud) = entry.metadata.and_then(|m| m.cloud) {
            let identity = Identity::decode(&cloud.identity).map_err(error)?;
            if identity.binding != id || identity.vault != vault {
                return Err(error("foreign placeholder"));
            }
            catalog.insert(identity).map_err(error)?;
        }
    }
    // Both startup and create_binding use this assembly path. Repair older CFAPI-only
    // vaults without unregistering or changing any file's hydration/pin/in-sync state.
    // Keep the metadata connection live while WinRT resolves the existing folder.
    let shell_path = binding.path.clone();
    tokio::task::spawn_blocking(move || {
        vaultsyncd_platform_windows::shell::register(&shell_path, &Catalog::new(id, vault))
    })
    .await
    .map_err(error)?
    .map_err(error)?;
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
        content: vaultsyncd_agent_core::ports::remote::RemotePort::content(&*remote, id).await?,
    });
    let uploads = Arc::new(UploadActionHandler::new(
        device,
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
        vault,
        FirstImport::new(db.clone(), ports, importer.clone(), remote.clone()),
        worker,
        scheduler,
        remote,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn uncertain_creation_recovers_after_store_reads_resume_without_creating_absent_candidates()
     {
        let path = std::env::temp_dir().join(format!(
            "s189-creation-recovery-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir(&path).unwrap();
        let db = Arc::new(Database::open(path.join("client.sqlite")).await.unwrap());
        let server_id = ServerId::from_bytes([6; 16]);
        let device_id = DeviceId::from_bytes([7; 16]);
        let binding_id = BindingId::from_bytes([8; 16]);
        let absent_id = BindingId::from_bytes([9; 16]);
        let config: AgentConfig = serde_json::from_value(serde_json::json!({
            "server": { "id": server_id.to_string(), "url": "https://example.invalid" },
            "device_id": device_id.to_string(),
            "state_dir": path,
            "poll_millis": 250,
            "bindings": [],
        }))
        .unwrap();
        let binding = BindingConfig {
            id: binding_id.to_string(),
            vault_id: VaultId::from_bytes([10; 16]).to_string(),
            path: path.join("vault"),
            cache: path.join("cache"),
            mode: "full".into(),
        };
        let mut absent = binding.clone();
        absent.id = absent_id.to_string();
        absent.path = path.join("absent-vault");
        absent.cache = path.join("absent-cache");
        let mut tx = db.begin_write().await.unwrap();
        bindings::insert_binding(
            &mut tx,
            &NewBinding {
                id: binding_id,
                server: server_id,
                device: device_id,
                vault: VaultId::from_hex(&binding.vault_id).unwrap(),
                local_path: binding.path.to_string_lossy().into_owned(),
                cache_path: binding.cache.to_string_lossy().into_owned(),
                mode: SyncMode::Full,
                rules_version: "1".into(),
                server_epoch: ServerEpoch::new(1).unwrap(),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let (report_wake, _) = mpsc::channel(1);
        let (connection_updates, _) = watch::channel(Vec::new());
        let (completed, _) = mpsc::unbounded_channel();
        let mut runtime = Runtime {
            db: db.clone(),
            config: config.clone(),
            assignments: HashMap::new(),
            servers: BTreeMap::new(),
            blocked_servers: HashSet::new(),
            pending: HashMap::from([
                (
                    binding_id,
                    PendingBinding {
                        server: server_id,
                        completed: true,
                    },
                ),
                (
                    absent_id,
                    PendingBinding {
                        server: server_id,
                        completed: false,
                    },
                ),
            ]),
            report_wake,
            connection_updates,
            completed,
        };
        runtime.config.bindings = vec![binding.clone(), absent.clone()];
        let (hint_wake, _) = mpsc::channel(1);
        let mut server = Server {
            connection: Connection {
                server: config.server.clone(),
                client: Err(reject("NOT_READY")),
                session: AuthSession::new(),
                sink: Arc::new(VaultSink {
                    vault: CredentialVault::new(&server_id.to_string()).unwrap(),
                    failed: AtomicBool::new(false),
                    pending: Mutex::new(None),
                }),
            },
            bindings: Vec::new(),
            uncertain: HashMap::from([(binding_id, binding.clone()), (absent_id, absent)]),
            live: Vec::new(),
            receivers: HashMap::new(),
            hints: HashMap::new(),
            hint_wake,
            startup: HashMap::new(),
            cycles: HashMap::new(),
            offline: HashSet::new(),
        };
        sqlx::query("ALTER TABLE bindings RENAME TO s189_unavailable_bindings")
            .execute(db.pool())
            .await
            .unwrap();
        runtime.retry_completed_bindings().await;
        server
            .reconcile_uncertain(&db, &config.device_id)
            .await
            .unwrap();
        assert_eq!(runtime.pending.len(), 2);
        assert_eq!(runtime.config.bindings.len(), 2);
        assert!(runtime.assignments.is_empty());
        assert_eq!(server.uncertain.len(), 2);
        assert!(server.bindings.is_empty());
        sqlx::query("ALTER TABLE s189_unavailable_bindings RENAME TO bindings")
            .execute(db.pool())
            .await
            .unwrap();
        runtime.retry_completed_bindings().await;
        server
            .reconcile_uncertain(&db, &config.device_id)
            .await
            .unwrap();
        assert_eq!(runtime.assignments.get(&binding_id), Some(&server_id));
        assert!(!runtime.pending.contains_key(&binding_id));
        assert!(
            runtime.pending.contains_key(&absent_id),
            "in-flight path reservation must remain"
        );
        assert_eq!(server.bindings.len(), 1);
        assert_eq!(server.bindings[0].id, binding_id.to_string());
        assert!(server.uncertain.is_empty());
        // A replacement worker must receive a binding whose commit was proved after it started.
        server.bindings.clear();
        let (commands, _requests) = mpsc::channel(1);
        let (adopt, mut adoptions) = mpsc::channel(1);
        let (stop, stopped) = oneshot::channel();
        let (_report, report) = watch::channel(server.report());
        let task = tokio::spawn(async move {
            let _ = stopped.await;
            Ok(())
        });
        runtime.servers.insert(
            server_id,
            Handle {
                commands,
                adopt,
                stop,
                report,
                task,
            },
        );
        runtime.pending.insert(
            binding_id,
            PendingBinding {
                server: server_id,
                completed: true,
            },
        );
        runtime.retry_completed_bindings().await;
        let adopted = adoptions.try_recv().unwrap();
        assert_eq!(adopted.id, binding_id.to_string());
        server.uncertain.insert(binding_id, adopted);
        server
            .reconcile_uncertain(&db, &config.device_id)
            .await
            .unwrap();
        assert_eq!(server.bindings.len(), 1);
        assert!(server.uncertain.is_empty());
        runtime.stop_servers().await.unwrap();
        management::save_connection(&db, &config.server)
            .await
            .unwrap();
        let mut info = management::connection(&config.server, None);
        info.authenticated = true;
        runtime.connection_updates.send_replace(vec![info]);
        runtime.fence_server(server_id);
        assert!(!runtime.connections()[0].authenticated);
        assert_eq!(
            runtime
                .selected_server(&Command {
                    name: CommandName::GetConnection,
                    parameters: None
                })
                .unwrap(),
            server_id
        );
        let (reply, _response) = oneshot::channel();
        let request = crate::ipc::CommandRequest {
            request: vaultsyncd_local_ipc::generated::Request {
                request_id: "fenced-server".into(),
                ipc_version: vaultsyncd_local_ipc::version::supported(),
                command: Command {
                    name: CommandName::SetConnection,
                    parameters: Some(vaultsyncd_local_ipc::generated::CommandParameters {
                        server_id: Some(server_id.to_string()),
                        server_url: Some("https://replacement.invalid".into()),
                        ..Default::default()
                    }),
                },
            },
            reply,
        };
        assert_eq!(
            runtime.set_connection(&request).await.unwrap_err().code(),
            Some("NEEDS_RECOVERY")
        );
        assert_eq!(
            vaultsyncd_client_store::connections::list(&db)
                .await
                .unwrap()[0]
                .server_url,
            config.server.url
        );
        assert!(runtime.problems().0.contains_key(&binding_id));
        runtime.pending.get_mut(&absent_id).unwrap().completed = true;
        runtime.retry_completed_bindings().await;
        assert!(runtime.pending.is_empty());
        assert_eq!(runtime.config.bindings.len(), 1);
        assert!(
            management::binding_server(&db, absent_id, &config.device_id)
                .await
                .unwrap()
                .is_none()
        );
        drop(runtime);
        drop(server);
        db.pool().close().await;
        for attempt in 0..100 {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => return,
                Err(_) if attempt < 99 => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(cause) => {
                    panic!("cannot clean the closed creation-recovery test database: {cause}")
                }
            }
        }
    }

    #[tokio::test]
    async fn removing_a_saved_server_is_refused_while_managed_and_erases_its_windows_credential() {
        use crate::config::ServerConfig;
        use crate::credentials::CredentialVault;
        use vaultsyncd_client_store::connections;
        use vaultsyncd_protocol::generated::SessionIssued;

        let path = std::env::temp_dir().join(format!(
            "s193-remove-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir(&path).unwrap();
        let db = Arc::new(Database::open(path.join("client.sqlite")).await.unwrap());
        let bound = ServerId::from_bytes([6; 16]);
        let other = ServerId::from_bytes([7; 16]);
        let device_id = DeviceId::from_bytes([8; 16]);
        let binding_id = BindingId::from_bytes([9; 16]);
        let config: AgentConfig = serde_json::from_value(serde_json::json!({
            "server": { "id": bound.to_string(), "url": "https://example.invalid" },
            "device_id": device_id.to_string(),
            "state_dir": path,
            "poll_millis": 250,
            "bindings": [],
        }))
        .unwrap();
        let mut tx = db.begin_write().await.unwrap();
        bindings::insert_binding(
            &mut tx,
            &NewBinding {
                id: binding_id,
                server: bound,
                device: device_id,
                vault: VaultId::from_bytes([10; 16]),
                local_path: path.join("vault").to_string_lossy().into_owned(),
                cache_path: path.join("cache").to_string_lossy().into_owned(),
                mode: SyncMode::Full,
                rules_version: "1".into(),
                server_epoch: ServerEpoch::new(1).unwrap(),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        for (id, url) in [
            (bound, "https://bound.example"),
            (other, "https://other.example"),
        ] {
            management::save_connection(
                &db,
                &ServerConfig {
                    id: id.to_string(),
                    url: url.into(),
                    ca_file: None,
                    development_http: false,
                    username: None,
                },
            )
            .await
            .unwrap();
            sqlx::query("INSERT INTO agent_login_profiles(server_id,username) VALUES(?,?)")
                .bind(id.to_string())
                .bind("原账号")
                .execute(db.pool())
                .await
                .unwrap();
        }
        let (report_wake, _wake) = mpsc::channel(1);
        let (connection_updates, _connections) = watch::channel(Vec::new());
        let (completed, _completed) = mpsc::unbounded_channel();
        let mut runtime = Runtime {
            db: db.clone(),
            config: config.clone(),
            assignments: HashMap::new(),
            servers: BTreeMap::new(),
            blocked_servers: HashSet::new(),
            pending: HashMap::new(),
            report_wake,
            connection_updates,
            completed,
        };
        for id in [bound, other] {
            let (commands, _requests) = mpsc::channel(1);
            let (adopt, _adoptions) = mpsc::channel(1);
            let (stop, stopped) = oneshot::channel();
            let (_report, report) = watch::channel(Report {
                connection: ConnectionInfo {
                    server_id: id.to_string(),
                    server_url: "https://example.invalid".into(),
                    ca_file: None,
                    username: None,
                    authenticated: false,
                },
                startup: HashMap::new(),
                cycles: HashMap::new(),
                offline: HashSet::new(),
            });
            let task = tokio::spawn(async move {
                let _ = stopped.await;
                Ok(())
            });
            runtime.servers.insert(
                id,
                Handle {
                    commands,
                    adopt,
                    stop,
                    report,
                    task,
                },
            );
        }
        // A stored session for the removable server: removal must erase this Windows entry even
        // though the unreachable endpoint cannot confirm revocation.
        let issued = SessionIssued {
            session_id: "ab".repeat(16),
            access_token: "access-for-s193".into(),
            refresh_token: "refresh-for-s193".into(),
            access_expires_at: "2000000000".into(),
            refresh_expires_at: "2000000100".into(),
        };
        let vault = CredentialVault::new(&other.to_string()).unwrap();
        vault.store(&serde_json::to_vec(&issued).unwrap()).unwrap();
        assert!(vault.load().unwrap().is_some());
        let bound_info = runtime.servers[&bound].report.borrow().connection.clone();
        let other_info = runtime.servers[&other].report.borrow().connection.clone();
        runtime
            .connection_updates
            .send_replace(vec![bound_info, other_info]);
        let request = |id: ServerId| {
            let (reply, _response) = oneshot::channel();
            crate::ipc::CommandRequest {
                request: vaultsyncd_local_ipc::generated::Request {
                    request_id: "remove-server".into(),
                    ipc_version: vaultsyncd_local_ipc::version::supported(),
                    command: Command {
                        name: CommandName::RemoveConnection,
                        parameters: Some(vaultsyncd_local_ipc::generated::CommandParameters {
                            server_id: Some(id.to_string()),
                            ..Default::default()
                        }),
                    },
                },
                reply,
            }
        };

        // A server with a durable binding (even a detached one) can only be removed after
        // unbinding; the refusal must not stop its worker or touch stored state.
        assert_eq!(
            runtime
                .remove_connection(&request(bound))
                .await
                .unwrap_err()
                .code(),
            Some("CONNECTION_IN_USE")
        );
        assert!(runtime.servers.contains_key(&bound));
        assert_eq!(connections::list(&db).await.unwrap().len(), 2);

        let result = runtime.remove_connection(&request(other)).await.unwrap();
        // The server is unreachable, so revocation is unconfirmed; the local entry still goes away.
        assert_eq!(result.session_revoked, Some(false));
        assert!(vault.load().unwrap().is_none());
        assert!(!runtime.servers.contains_key(&other));
        assert!(
            runtime
                .connections()
                .iter()
                .all(|connection| connection.server_id != other.to_string())
        );
        assert_eq!(
            connections::list(&db)
                .await
                .unwrap()
                .into_iter()
                .map(|record| record.server_id)
                .collect::<Vec<_>>(),
            vec![bound]
        );
        assert!(connections::removed(&db, &other).await.unwrap());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_login_profiles")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            runtime
                .remove_connection(&request(ServerId::from_bytes([11; 16])))
                .await
                .unwrap_err()
                .code(),
            Some("NOT_FOUND")
        );

        runtime.stop_servers().await.unwrap();
        drop(runtime);
        db.pool().close().await;
        for attempt in 0..100 {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => return,
                Err(_) if attempt < 99 => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(cause) => panic!("cannot clean the removed-connection database: {cause}"),
            }
        }
    }

    #[tokio::test]
    async fn a_single_round_handle_keeps_its_command_and_stop_channels_alive() {
        let (commands, mut requests) = mpsc::channel::<Job>(1);
        let (adopt, _adoptions) = mpsc::channel(1);
        let (stop, mut stopped) = oneshot::channel();
        let (_report, report) = watch::channel(Report {
            connection: ConnectionInfo {
                server_id: "11".repeat(16),
                server_url: "https://example.invalid".into(),
                ca_file: None,
                username: None,
                authenticated: false,
            },
            startup: HashMap::new(),
            cycles: HashMap::new(),
            offline: HashSet::new(),
        });
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = requests.recv() => Err(error("command channel closed before the round")),
                _ = &mut stopped => Err(error("stop channel closed before the round")),
                _ = tokio::time::sleep(Duration::from_millis(20)) => Ok(()),
            }
        });
        Handle {
            commands,
            adopt,
            report,
            stop,
            task,
        }
        .into_task()
        .await
        .unwrap()
        .unwrap();
    }
}
