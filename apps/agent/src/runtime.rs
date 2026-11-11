//! Headless Windows process assembly and recovery ordering.
use crate::{
    bootstrap::{HydrationConfig, PinnedVersion, connect_hydration},
    config::AgentConfig,
    credentials::CredentialVault,
    events::{Bridge, Decoder},
    importer::{Importer, pin},
    instance::InstanceGuard,
    local::{Local, error},
    management::{self, reject},
    remote::Remote,
    shutdown::Shutdown,
    work::{Commits, Dispatch, Worker, metadata},
};
use rand_core::{OsRng, RngCore};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot, watch};
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
    ids::{BindingId, DeviceId, EntryId, IdSource, ServerId, VaultId, VersionId},
    version::{BoundError, Clock, ServerEpoch, UtcTimestamp},
};
use vaultsyncd_http_client::{
    ApiClient, AuthSession, CredentialSink, Credentials, ReqwestTransport,
};
use vaultsyncd_local_ipc::generated::{
    Command, CommandName, ConnectionInfo, ManagementResult, Problem,
};
use vaultsyncd_platform_windows::{
    cfapi::{
        adapter::Callbacks,
        catalog::{Catalog, Identity},
    },
    files::controlled::RecoveryOutcome,
};
use vaultsyncd_protocol::generated::SessionIssued;
use vaultsyncd_transfer::{
    download::DownloadOptions,
    ports::{PortError, VersionPin},
};
#[cfg(test)]
mod rebind_tests;
mod server;

type StoredPin = (Vec<u8>, Vec<u8>, Vec<u8>, i64);
type LiveBinding = (
    BindingId,
    VaultId,
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
        let erased = self.pending.lock().is_ok_and(|mut pending| {
            *pending = None;
            self.vault.erase().is_ok()
        });
        self.failed.store(!erased, Ordering::Release);
    }
}
/// Run in the actual interactive user session. Authentication input is transient stdin only.
pub async fn run(config: AgentConfig, login: bool, once: bool) -> Result<(), PortError> {
    run_mode(config, login, once, false).await
}
/// Start a controller-owned update trial; synchronization and write IPC remain commit-gated.
pub async fn run_update_trial(config: AgentConfig) -> Result<(), PortError> {
    run_mode(config, false, false, true).await
}
fn admit(config: &AgentConfig, trial: bool) -> Result<(), PortError> {
    if trial {
        crate::maintenance::admit_update_trial(config)
    } else {
        crate::maintenance::admit_startup(config)
    }
}
async fn run_mode(
    mut config: AgentConfig,
    login: bool,
    once: bool,
    trial: bool,
) -> Result<(), PortError> {
    config.validate().map_err(error)?;
    admit(&config, trial)?;
    // Durable detached controls are loaded under the writer lock before path admission.
    // Checking the legacy configuration alone would revive its historical reservations.
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
    // Recheck after ownership acquisition: a concurrent ZIP claim must not be bypassed.
    admit(&config, trial)?;
    let db = Arc::new(
        Database::open(config.state_dir.join("client.sqlite"))
            .await
            .map_err(error)?,
    );
    let servers = management::load_connections(&db, &config).await?;
    let assignments = management::load_bindings(&db, &mut config).await?;
    // Policy adoption is local and must also run without credentials or network access.
    // Include detached bindings: retained state must not change meaning after a restart.
    let bindings: Vec<Vec<u8>> = sqlx::query_scalar("SELECT id FROM bindings")
        .fetch_all(db.pool())
        .await
        .map_err(error)?;
    let controls = BindingControls::new(db.clone());
    for binding in bindings {
        controls
            .adopt_default_policy(BindingId::from_bytes(
                binding
                    .try_into()
                    .map_err(|_| error("invalid binding identity"))?,
            ))
            .await?;
    }
    management::admit(&db, &config).await?;
    let (report_wake, mut reports) = mpsc::channel(1);
    let (connection_updates, connections) = watch::channel(Vec::new());
    let (completed, mut completions) = mpsc::unbounded_channel();
    let mut runtime = Runtime {
        db: db.clone(),
        config,
        assignments,
        servers: BTreeMap::new(),
        blocked_servers: HashSet::new(),
        pending: HashMap::new(),
        report_wake,
        connection_updates,
        completed,
    };
    if login {
        let [server] = servers.as_slice() else {
            return Err(reject(if servers.is_empty() {
                "NOT_FOUND"
            } else {
                "SERVER_REQUIRED"
            }));
        };
        return server::Connection::open(server.clone())?
            .login_stdin()
            .await;
    }
    for server in servers {
        let id = ServerId::from_hex(&server.id).map_err(error)?;
        let connection = server::Connection::open(server)?;
        runtime.start_server(id, connection, once);
    }
    if once {
        if runtime.servers.is_empty() {
            return Err(reject("REAUTH_REQUIRED"));
        }
        let tasks: Vec<_> = std::mem::take(&mut runtime.servers)
            .into_values()
            .map(server::Handle::into_task)
            .collect();
        return wait_for_servers(tasks).await;
    }

    let shutdown = Shutdown::install().map_err(error)?;
    let (commands, mut requests) = mpsc::channel::<crate::ipc::CommandRequest>(8);
    let (problem_updates, problems) = watch::channel(Vec::new());
    let (offline_updates, offline) = watch::channel(runtime.offline());
    let ipc =
        crate::ipc::start(db.clone(), commands, connections, problems, offline).map_err(error)?;
    eprintln!(
        "event=management-ready configured={}",
        runtime.config.bindings.len()
    );
    let result = loop {
        if shutdown.requested() {
            break Ok(());
        }
        if ipc.is_finished() {
            break Err(error("IPC listener stopped unexpectedly"));
        }
        tokio::select! {
            _ = async {
                while !shutdown.requested() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            } => break Ok(()),
            job = requests.recv() => {
                let Some(job) = job else { break Err(error("IPC command channel closed")); };
                if job.reply.is_closed() {
                    continue;
                }
                if job.request.command.name == CommandName::PrepareMaintenance {
                    let result = runtime.prepare_maintenance(&job.request.command).await;
                    let _ = job.reply.send(crate::ipc::management_response(&job.request, result));
                    // Let the bounded transport flush the preparation acknowledgment.
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    break Ok(());
                }
                runtime.dispatch(job).await;
            }
            id = completions.recv() => {
                if let Some(id) = id {
                    if let Some(pending) = runtime.pending.get_mut(&id) { pending.completed = true; }
                    runtime.complete_binding(id).await;
                }
            }
            _ = reports.recv() => {}
            _ = tokio::time::sleep(Duration::from_millis(runtime.config.poll_millis)) => {}
        }
        runtime.retry_completed_bindings().await;
        offline_updates.send_if_modified(|previous| update(previous, runtime.offline()));
        let (startup, cycles) = runtime.problems();
        let projected = crate::issue_projection::sync(&db, &runtime.config, &startup, &cycles)
            .await
            .unwrap_or_else(|_| {
                eprintln!("event=problem-persistence-unavailable code=NEEDS_RECOVERY");
                startup.into_values().chain(cycles.into_values()).collect()
            });
        problem_updates.send_if_modified(|previous| update(previous, projected));
    };
    let stopped = runtime.stop_servers().await;
    let closed = ipc.shutdown().await.map_err(error);
    db.pool().close().await;
    eprintln!("event=stopped");
    result.and(stopped).and(closed)
}

fn update<T: PartialEq>(previous: &mut T, value: T) -> bool {
    if *previous == value {
        return false;
    }
    *previous = value;
    true
}

struct PendingBinding {
    server: ServerId,
    completed: bool,
}

struct Runtime {
    db: Arc<Database>,
    config: AgentConfig,
    assignments: HashMap<BindingId, ServerId>,
    servers: BTreeMap<ServerId, server::Handle>,
    blocked_servers: HashSet<ServerId>,
    pending: HashMap<BindingId, PendingBinding>,
    report_wake: mpsc::Sender<()>,
    connection_updates: watch::Sender<Vec<ConnectionInfo>>,
    completed: mpsc::UnboundedSender<BindingId>,
}

impl Runtime {
    fn start_server(&mut self, id: ServerId, connection: server::Connection, once: bool) {
        let bindings = self
            .config
            .bindings
            .iter()
            .filter(|binding| {
                BindingId::from_hex(&binding.id)
                    .ok()
                    .and_then(|id| self.assignments.get(&id))
                    == Some(&id)
            })
            .cloned()
            .collect();
        self.servers.insert(
            id,
            server::start(
                self.db.clone(),
                self.config.clone(),
                connection,
                bindings,
                self.report_wake.clone(),
                self.connection_updates.clone(),
                once,
            ),
        );
    }

    fn connections(&self) -> Vec<ConnectionInfo> {
        self.connection_updates.borrow().clone()
    }

    fn offline(&self) -> HashSet<BindingId> {
        let mut offline = self
            .servers
            .values()
            .flat_map(|worker| {
                worker
                    .report
                    .borrow()
                    .offline
                    .iter()
                    .copied()
                    .collect::<Vec<_>>()
            })
            .collect::<HashSet<_>>();
        offline.extend(self.assignments.iter().filter_map(|(binding, server)| {
            self.servers
                .get(server)
                .is_none_or(|worker| worker.task_finished())
                .then_some(*binding)
        }));
        offline
    }

    fn problems(&self) -> (HashMap<BindingId, Problem>, HashMap<BindingId, Problem>) {
        let mut startup = HashMap::new();
        let mut cycles = HashMap::new();
        for server in self.servers.values() {
            let report = server.report.borrow();
            startup.extend(report.startup.iter().map(|(id, p)| (*id, p.clone())));
            cycles.extend(report.cycles.iter().map(|(id, p)| (*id, p.clone())));
        }
        for (&binding, server) in &self.assignments {
            if self.blocked_servers.contains(server) {
                startup.insert(
                    binding,
                    Problem {
                        binding_id: binding.to_string(),
                        problem_id: format!("disconnect:{binding}"),
                        entry_id: None,
                        path: None,
                        code: "NEEDS_RECOVERY".into(),
                        reason: "该服务器的原生连接尚未完成安全断开".into(),
                        local_data: "原有连接、绑定、目录、缓存与待办保留".into(),
                        next_step: "退出并重新启动客户端 agent 后重试".into(),
                        retryable: false,
                    },
                );
            }
        }
        (startup, cycles)
    }

    fn selected_server(&self, command: &Command) -> Result<ServerId, PortError> {
        let connections = self.connections();
        if let Some(id) = command
            .parameters
            .as_ref()
            .and_then(|p| p.server_id.as_deref())
        {
            let id = ServerId::from_hex(id).map_err(error)?;
            return connections
                .iter()
                .any(|connection| connection.server_id == id.to_string())
                .then_some(id)
                .ok_or_else(|| reject("NOT_FOUND"));
        }
        let [connection] = connections.as_slice() else {
            return Err(reject(if connections.is_empty() {
                "NOT_FOUND"
            } else {
                "SERVER_REQUIRED"
            }));
        };
        ServerId::from_hex(&connection.server_id).map_err(error)
    }

    fn fence_server(&mut self, id: ServerId) {
        self.blocked_servers.insert(id);
        self.connection_updates.send_if_modified(|connections| {
            connections
                .iter_mut()
                .find(|connection| connection.server_id == id.to_string())
                .is_some_and(|connection| update(&mut connection.authenticated, false))
        });
    }

    async fn set_connection(
        &mut self,
        request: &crate::ipc::CommandRequest,
    ) -> Result<ManagementResult, PortError> {
        let proposed =
            management::proposed_connection(&self.db, &self.config, &request.request.command)
                .await?;
        let id = ServerId::from_hex(&proposed.id).map_err(error)?;
        if self.blocked_servers.contains(&id) {
            return Err(reject("NEEDS_RECOVERY"));
        }
        management::verify_connection(&proposed).await?;
        if request.reply.is_closed() {
            return Err(reject("UNAVAILABLE"));
        }
        // A legacy file-only connection remains available after adding another server.
        if !self.servers.contains_key(&id)
            && self.servers.len() == 1
            && vaultsyncd_client_store::connections::list(&self.db)
                .await
                .map_err(error)?
                .is_empty()
            && let [legacy] = management::load_connections(&self.db, &self.config)
                .await?
                .as_slice()
        {
            management::save_connection(&self.db, legacy).await?;
        }
        // Stop the matching server before replacing its transport. Other servers keep running.
        // If the caller times out while it drains, reload the unchanged durable endpoint.
        let old = self.servers.remove(&id);
        if let Some(worker) = old {
            let stopped = worker.stop().await.map_err(error).and_then(|result| result);
            self.complete_server_bindings(id).await;
            if let Err(cause) = stopped {
                // A failed native disconnect/drain cannot prove the old callbacks are gone.
                // Preserve the saved endpoint and block recreation until this process exits.
                self.fence_server(id);
                return Err(cause);
            }
        }
        let change = async {
            if request.reply.is_closed() {
                return Err(reject("UNAVAILABLE"));
            }
            let connection = server::Connection::open(proposed.clone())?;
            management::save_connection(&self.db, &proposed).await?;
            Ok::<_, PortError>(connection)
        }
        .await;
        let connection = match change {
            Ok(connection) => connection,
            Err(cause) => {
                self.reload_server(id).await?;
                return Err(cause);
            }
        };
        let result = connection.info();
        self.start_server(id, connection, false);
        Ok(ManagementResult {
            connection: Some(result),
            ..Default::default()
        })
    }

    /// Forget a saved server. Bindings must be detached first; the local entry always goes away.
    async fn remove_connection(
        &mut self,
        request: &crate::ipc::CommandRequest,
    ) -> Result<ManagementResult, PortError> {
        use vaultsyncd_client_store::connections::{self, Removal};
        let parameters = request
            .request
            .command
            .parameters
            .as_ref()
            .ok_or_else(|| reject("INVALID_ARGUMENT"))?;
        let id = ServerId::from_hex(
            parameters
                .server_id
                .as_deref()
                .ok_or_else(|| reject("INVALID_ARGUMENT"))?,
        )
        .map_err(error)?;
        if self.blocked_servers.contains(&id) {
            // A failed native disconnect cannot prove the old callbacks are gone.
            return Err(reject("NEEDS_RECOVERY"));
        }
        let config = management::load_connections(&self.db, &self.config)
            .await?
            .into_iter()
            .find(|server| server.id == id.to_string())
            .ok_or_else(|| reject("NOT_FOUND"))?;
        // Refuse before stopping anything so a rejected removal has no side effect.
        if connections::in_use(&self.db, &id).await.map_err(error)? {
            return Err(reject("CONNECTION_IN_USE"));
        }
        if let Some(worker) = self.servers.remove(&id) {
            let stopped = worker.stop().await.map_err(error).and_then(|result| result);
            self.complete_server_bindings(id).await;
            if let Err(cause) = stopped {
                self.fence_server(id);
                return Err(cause);
            }
        }
        // Revocation is best-effort: an offline server must still be removable, but the
        // caller needs to know the remote session was not confirmed revoked.
        let session = server::Connection::open(config)?;
        let revoked = session.revoke_session().await;
        session.erase_credentials();
        match connections::remove(&self.db, &id).await.map_err(error)? {
            Removal::Removed => {}
            Removal::HasBindings => {
                self.reload_server(id).await?;
                return Err(reject("CONNECTION_IN_USE"));
            }
        }
        self.connection_updates.send_if_modified(|connections| {
            let before = connections.len();
            connections.retain(|connection| connection.server_id != id.to_string());
            connections.len() != before
        });
        Ok(ManagementResult {
            session_revoked: Some(revoked),
            ..Default::default()
        })
    }

    async fn reload_server(&mut self, id: ServerId) -> Result<(), PortError> {
        let servers = management::load_connections(&self.db, &self.config).await?;
        if let Some(config) = servers
            .into_iter()
            .find(|server| server.id == id.to_string())
        {
            let connection = server::Connection::open(config)?;
            self.start_server(id, connection, false);
        }
        Ok(())
    }

    async fn dispatch(&mut self, request: crate::ipc::CommandRequest) {
        let command = &request.request.command;
        let result = match command.name {
            CommandName::GetConnection => self.selected_server(command).and_then(|id| {
                let connection = self
                    .connections()
                    .into_iter()
                    .find(|connection| connection.server_id == id.to_string())
                    .ok_or_else(|| reject("NOT_FOUND"))?;
                Ok(ManagementResult {
                    connection: Some(connection),
                    ..Default::default()
                })
            }),
            CommandName::ListConnections => Ok(ManagementResult {
                connections: Some(self.connections()),
                ..Default::default()
            }),
            CommandName::SetConnection => self.set_connection(&request).await,
            CommandName::RemoveConnection => self.remove_connection(&request).await,
            CommandName::Login | CommandName::Logout | CommandName::ListVaults => {
                match self.selected_server(command) {
                    Ok(id) => return self.send_job(id, request, None),
                    Err(cause) => Err(cause),
                }
            }
            CommandName::CreateBinding => match self.prepare_binding(command).await {
                Ok((server, id, binding)) => {
                    let (complete, completion) = oneshot::channel();
                    self.pending.insert(
                        id,
                        PendingBinding {
                            server,
                            completed: false,
                        },
                    );
                    self.config.bindings.push(binding.clone());
                    self.send_job(server, request, Some((binding, complete)));
                    let completed = self.completed.clone();
                    tokio::spawn(async move {
                        let _ = completion.await;
                        let _ = completed.send(id);
                    });
                    return;
                }
                Err(cause) => Err(cause),
            },
            _ => {
                let binding = command
                    .parameters
                    .as_ref()
                    .and_then(|p| p.binding_id.as_deref())
                    .and_then(|id| BindingId::from_hex(id).ok());
                if let Some(id) = binding {
                    match management::binding_server(&self.db, id, &self.config.device_id).await {
                        Ok(Some(server))
                            if command.name == CommandName::DetachBinding
                                && (self.blocked_servers.contains(&server)
                                    || self
                                        .servers
                                        .get(&server)
                                        .is_some_and(|worker| worker.task_finished())) =>
                        {
                            let _ = request.reply.send(crate::ipc::management_response(
                                &request.request,
                                Err(reject("NEEDS_RECOVERY")),
                            ));
                            return;
                        }
                        Ok(Some(server))
                            if self
                                .servers
                                .get(&server)
                                .is_some_and(|worker| !worker.task_finished()) =>
                        {
                            return self.send_job(server, request, None);
                        }
                        Err(cause) => {
                            let _ = request.reply.send(crate::ipc::management_response(
                                &request.request,
                                Err(cause),
                            ));
                            return;
                        }
                        Ok(_) => {}
                    }
                }
                let mut result =
                    management::execute(&self.db, &self.config, None, HashMap::new(), command)
                        .await;
                if command.name == CommandName::ListProblems
                    && let Ok(result) = &mut result
                {
                    let (startup, _) = self.problems();
                    let search = command
                        .parameters
                        .as_ref()
                        .and_then(|p| p.search.as_deref())
                        .map(str::to_lowercase);
                    result
                        .problems
                        .get_or_insert_default()
                        .extend(startup.into_iter().filter_map(|(id, problem)| {
                            (binding.is_none_or(|b| b == id)
                                && search
                                    .as_ref()
                                    .is_none_or(|s| problem.code.to_lowercase().contains(s)))
                            .then_some(problem)
                        }));
                }
                result
            }
        };
        let _ = request
            .reply
            .send(crate::ipc::management_response(&request.request, result));
    }

    fn send_job(
        &self,
        id: ServerId,
        request: crate::ipc::CommandRequest,
        create: Option<(crate::config::BindingConfig, oneshot::Sender<()>)>,
    ) {
        let job = server::Job {
            request,
            config: self.config.clone(),
            create,
        };
        let Some(server) = self
            .servers
            .get(&id)
            .filter(|_| !self.blocked_servers.contains(&id))
        else {
            let request = job.request;
            let _ = request.reply.send(crate::ipc::management_response(
                &request.request,
                Err(reject("NEEDS_RECOVERY")),
            ));
            return;
        };
        if let Err(failure) = server.commands.try_send(job) {
            let request = failure.into_inner().request;
            let cause = PortError::unavailable("server worker unavailable", None);
            let _ = request.reply.send(crate::ipc::management_response(
                &request.request,
                Err(cause),
            ));
        }
    }

    async fn prepare_binding(
        &self,
        command: &Command,
    ) -> Result<(ServerId, BindingId, crate::config::BindingConfig), PortError> {
        let server = self.selected_server(command)?;
        if self.blocked_servers.contains(&server) {
            return Err(reject("NEEDS_RECOVERY"));
        }
        let binding = management::new_binding(&self.db, &self.config, command).await?;
        let id = BindingId::from_hex(&binding.id).map_err(error)?;
        if self.assignments.contains_key(&id)
            || management::binding_server(&self.db, id, &self.config.device_id)
                .await?
                .is_some()
        {
            return Err(reject("ALREADY_EXISTS"));
        }
        Ok((server, id, binding))
    }

    async fn complete_binding(&mut self, binding: BindingId) {
        let Some(pending) = self
            .pending
            .get(&binding)
            .filter(|pending| pending.completed)
        else {
            return;
        };
        let server = pending.server;
        match management::binding_server(&self.db, binding, &self.config.device_id).await {
            Ok(Some(owner)) if owner == server => {
                self.assignments.insert(binding, server);
                if let Some(worker) = self
                    .servers
                    .get(&server)
                    .filter(|worker| !worker.task_finished())
                    && let Some(configured) = self
                        .config
                        .bindings
                        .iter()
                        .find(|configured| configured.id == binding.to_string())
                    && worker.adopt.try_send(configured.clone()).is_err()
                {
                    // The durable outcome is known, but the replacement worker still needs it.
                    return;
                }
            }
            Ok(None) => self
                .config
                .bindings
                .retain(|config| config.id != binding.to_string()),
            _ => {
                // Keep the local paths reserved if the durable outcome could not be proved.
                eprintln!("event=binding-reservation-failed binding={binding}");
                return;
            }
        }
        self.pending.remove(&binding);
    }

    async fn complete_server_bindings(&mut self, server: ServerId) {
        let bindings = self
            .pending
            .iter()
            .filter_map(|(&binding, pending)| (pending.server == server).then_some(binding))
            .collect::<Vec<_>>();
        for binding in bindings {
            if let Some(pending) = self.pending.get_mut(&binding) {
                pending.completed = true;
            }
            self.complete_binding(binding).await;
        }
    }

    async fn retry_completed_bindings(&mut self) {
        let bindings = self
            .pending
            .iter()
            .filter_map(|(&binding, pending)| pending.completed.then_some(binding))
            .collect::<Vec<_>>();
        for binding in bindings {
            self.complete_binding(binding).await;
        }
    }

    async fn prepare_maintenance(
        &mut self,
        command: &Command,
    ) -> Result<ManagementResult, PortError> {
        let destination = command
            .parameters
            .as_ref()
            .and_then(|p| p.destination.as_deref())
            .ok_or_else(|| reject("INVALID_ARGUMENT"))?;
        self.stop_servers().await?;
        for pending in self.pending.values_mut() {
            pending.completed = true;
        }
        self.retry_completed_bindings().await;
        if !self.pending.is_empty() {
            return Err(error("binding persistence not resolved before maintenance"));
        }
        crate::maintenance_prepare::prepare(&self.db, &self.config, HashMap::new(), destination)
            .await
    }

    async fn stop_servers(&mut self) -> Result<(), PortError> {
        let tasks = std::mem::take(&mut self.servers)
            .into_values()
            .map(server::Handle::stop)
            .collect();
        wait_for_servers(tasks).await
    }
}

async fn wait_for_servers(
    tasks: Vec<tokio::task::JoinHandle<Result<(), PortError>>>,
) -> Result<(), PortError> {
    let mut failure = None;
    for task in tasks {
        if let Err(cause) = task.await.map_err(error).and_then(|result| result) {
            failure.get_or_insert(cause);
        }
    }
    failure.map_or(Ok(()), Err)
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
    vault: VaultId,
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
            vault,
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
        vault,
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
