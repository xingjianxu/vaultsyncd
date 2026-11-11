//! Shared management executor. All user-facing operations resolve identities from durable state.
use crate::{
    config::{AgentConfig, BindingConfig, ServerConfig},
    local::error,
    work::{Dispatch, Worker},
};
use std::{collections::HashMap, path::Path, sync::Arc};
use vaultsyncd_agent_core::{
    bindings::control::{BindingControls, ExecutionGate},
    engine::scheduler::Scheduler,
};
use vaultsyncd_client_store::{
    bindings::{self, SyncMode},
    connections::{self, ConnectionRecord},
    db::Database,
};
use vaultsyncd_domain::ids::{BindingId, DeviceId, EntryId, OpId, ServerId};
use vaultsyncd_http_client::{ApiClient, ApiRequest, AuthSession, Method, ReqwestTransport};
use vaultsyncd_local_ipc::generated::{
    BindingInfo, Command, CommandName as C, ConnectionInfo, ManagementResult, Problem, RemoteItem,
};
use vaultsyncd_platform_windows::files::VaultHandle;
use vaultsyncd_transfer::ports::PortError;

pub(crate) type Running = HashMap<BindingId, (Worker, Arc<Scheduler<Dispatch>>)>;
pub(crate) fn reject(code: &str) -> PortError {
    PortError::rejected(code, "management command rejected")
}
pub(crate) fn connection_error(error: vaultsyncd_http_client::ClientError) -> PortError {
    if error.certificate_problem().is_some() {
        PortError::rejected(
            "CERTIFICATE_INVALID",
            "server certificate verification failed",
        )
    } else {
        vaultsyncd_http_client::transfer_error(error)
    }
}

pub(crate) async fn load_connections(
    db: &Database,
    config: &AgentConfig,
) -> Result<Vec<ServerConfig>, PortError> {
    let mut stored = connections::list(db).await.map_err(error)?;
    if stored.is_empty() {
        let legacy_id = ServerId::from_hex(&config.server.id).map_err(error)?;
        let placeholder = config.server.url == "https://localhost"
            && !config.server.development_http
            && config.server.ca_file.is_none()
            && config.bindings.is_empty();
        let legacy_state: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM bindings WHERE server_id=?)
                 OR EXISTS(SELECT 1 FROM agent_login_profiles WHERE server_id=?)",
        )
        .bind(legacy_id.as_bytes().as_slice())
        .bind(&config.server.id)
        .fetch_one(db.pool())
        .await
        .map_err(error)?;
        // A server the user removed must not reappear from agent.json, even when that
        // file-only endpoint was the only durable copy of the connection.
        let removed = connections::removed(db, &legacy_id).await.map_err(error)?;
        if (!placeholder || legacy_state) && !removed {
            stored.push(connection_record(&config.server)?);
        }
    }
    let mut servers = Vec::with_capacity(stored.len());
    for record in stored {
        let mut server = ServerConfig {
            id: record.server_id.to_string(),
            url: record.server_url,
            ca_file: record.ca_file.map(Into::into),
            development_http: record.development_http && config.server.development_http,
            username: None,
        };
        load_login_profile(db, &mut server).await?;
        servers.push(server);
    }
    Ok(servers)
}

async fn load_login_profile(db: &Database, server: &mut ServerConfig) -> Result<(), PortError> {
    server.username =
        sqlx::query_scalar("SELECT username FROM agent_login_profiles WHERE server_id=?")
            .bind(&server.id)
            .fetch_optional(db.pool())
            .await
            .map_err(error)?;
    Ok(())
}

pub(crate) async fn proposed_connection(
    db: &Database,
    config: &AgentConfig,
    command: &Command,
) -> Result<ServerConfig, PortError> {
    let p = command
        .parameters
        .as_ref()
        .ok_or_else(|| reject("INVALID_ARGUMENT"))?;
    let mut proposed = ServerConfig {
        id: p
            .server_id
            .clone()
            .ok_or_else(|| reject("INVALID_ARGUMENT"))?,
        url: p
            .server_url
            .clone()
            .ok_or_else(|| reject("INVALID_ARGUMENT"))?,
        ca_file: p.ca_file.as_ref().map(Into::into),
        development_http: config.server.development_http,
        username: None,
    };
    proposed
        .validate()
        .map_err(|_| reject("INVALID_ARGUMENT"))?;
    load_login_profile(db, &mut proposed).await?;
    Ok(proposed)
}

/// Read public identity using an isolated, credential-free client.
pub(crate) async fn discover_connection(
    config: &AgentConfig,
    command: &Command,
) -> Result<ManagementResult, PortError> {
    let p = command
        .parameters
        .as_ref()
        .ok_or_else(|| reject("INVALID_ARGUMENT"))?;
    let mut proposed = config.server.clone();
    proposed.url = p
        .server_url
        .clone()
        .ok_or_else(|| reject("INVALID_ARGUMENT"))?;
    proposed.ca_file = p.ca_file.as_ref().map(Into::into);
    proposed.username = None;
    proposed
        .validate()
        .map_err(|_| reject("INVALID_ARGUMENT"))?;
    proposed.id = read_server_id(&proposed).await?;
    Ok(ManagementResult {
        connection: Some(connection(&proposed, None)),
        ..Default::default()
    })
}

/// Recheck the confirmed identity before loading any credentials or updating the endpoint.
pub(crate) async fn verify_connection(server: &ServerConfig) -> Result<(), PortError> {
    let actual = read_server_id(server).await?;
    if actual != server.id {
        return Err(PortError::rejected(
            "CONNECTION_SERVER_MISMATCH",
            format!("{}:{actual}", server.id),
        ));
    }
    Ok(())
}

fn http_config(server: &ServerConfig) -> Result<vaultsyncd_http_client::ClientConfig, PortError> {
    use vaultsyncd_http_client::{ClientConfig, TlsTrust};
    let mut http = ClientConfig::new(&server.url);
    if server.development_http {
        http = http.allow_plaintext_http();
    }
    if let Some(ca) = &server.ca_file {
        http = http.with_trust(
            TlsTrust::system_roots_with_pem_file(ca).map_err(|_| reject("CERTIFICATE_INVALID"))?,
        );
    }
    Ok(http)
}

pub(crate) fn server_client(
    server: &ServerConfig,
    session: AuthSession,
) -> Result<ApiClient<ReqwestTransport>, PortError> {
    server.validate().map_err(|_| reject("INVALID_ARGUMENT"))?;
    let id = ServerId::from_hex(&server.id).map_err(error)?;
    Ok(ApiClient::new(http_config(server)?)
        .map_err(|_| reject("INVALID_ARGUMENT"))?
        .with_expected_server(id)
        .with_auth_session(session))
}

async fn read_server_id(server: &ServerConfig) -> Result<String, PortError> {
    use std::time::Duration;
    use vaultsyncd_http_client::Timeouts;
    let http = http_config(server)?.with_timeouts(Timeouts::new(
        Duration::from_secs(3),
        Duration::from_secs(2),
    ));
    let client = ApiClient::new(http).map_err(|_| reject("INVALID_ARGUMENT"))?;
    let value: vaultsyncd_protocol::generated::Capabilities = client
        .send_json(ApiRequest::get("/v1/capabilities"))
        .await
        .map_err(connection_error)?;
    vaultsyncd_protocol::negotiation::negotiate(&value).map_err(|_| reject("INVALID_ARGUMENT"))?;
    Ok(value.server_id)
}

fn connection_record(server: &ServerConfig) -> Result<ConnectionRecord, PortError> {
    Ok(ConnectionRecord {
        server_id: ServerId::from_hex(&server.id).map_err(error)?,
        server_url: server.url.clone(),
        ca_file: server
            .ca_file
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        development_http: server.development_http,
    })
}

pub(crate) async fn save_connection(db: &Database, server: &ServerConfig) -> Result<(), PortError> {
    connections::upsert(db, &connection_record(server)?)
        .await
        .map_err(error)
}

pub(crate) async fn binding_server(
    db: &Database,
    binding: BindingId,
    device: &str,
) -> Result<Option<ServerId>, PortError> {
    let mut conn = db.pool().acquire().await.map_err(error)?;
    let record = bindings::binding(&mut conn, binding).await.map_err(error)?;
    match record {
        Some(record) if record.device.to_string() == device => Ok(Some(record.server)),
        Some(_) => Err(reject("BINDING_BLOCKED")),
        None => Ok(None),
    }
}

pub(crate) async fn load_bindings(
    db: &Database,
    config: &mut AgentConfig,
) -> Result<HashMap<BindingId, ServerId>, PortError> {
    let legacy_server = ServerId::from_hex(&config.server.id).map_err(error)?;
    let mut assignments = HashMap::new();
    for binding in &config.bindings {
        assignments.insert(
            BindingId::from_hex(&binding.id).map_err(error)?,
            legacy_server,
        );
    }
    let rows: Vec<(String,String,String,String,String,String)> = sqlx::query_as("SELECT lower(hex(id)),lower(hex(server_id)),lower(hex(vault_id)),local_path,cache_path,mode FROM bindings WHERE device_id=? ORDER BY id")
        .bind(DeviceId::from_hex(&config.device_id).map_err(error)?.as_bytes().as_slice())
        .fetch_all(db.pool()).await.map_err(error)?;
    for (id, server_id, vault_id, path, cache, mode) in rows {
        let binding = BindingId::from_hex(&id).map_err(error)?;
        let server = ServerId::from_hex(&server_id).map_err(error)?;
        if let Some(existing) = config.bindings.iter_mut().find(|b| b.id == id) {
            if server != legacy_server
                || existing.vault_id != vault_id
                || existing.path != Path::new(&path)
                || existing.cache != Path::new(&cache)
            {
                return Err(reject("CONFIGURATION_CHANGED"));
            }
            existing.mode = mode;
        } else {
            config.bindings.push(BindingConfig {
                id,
                vault_id,
                path: path.into(),
                cache: cache.into(),
                mode,
            });
        }
        assignments.insert(binding, server);
    }
    Ok(assignments)
}

/// Recheck physical ancestry, including drive aliases; neither a GUI check nor string prefixes suffice.
pub(crate) fn admit(config: &AgentConfig) -> Result<(), PortError> {
    config.validate().map_err(error)?;
    let state = VaultHandle::open(&config.state_dir).map_err(error)?;
    let mut vaults = vec![state];
    for b in &config.bindings {
        for path in [&b.path, &b.cache] {
            let vault = VaultHandle::open(path).map_err(|_| reject("LOCAL_PATH_UNAVAILABLE"))?;
            if vaults.iter().any(|old| old.overlaps(&vault)) {
                return Err(reject("PATH_OVERLAP"));
            }
            vaults.push(vault);
        }
    }
    Ok(())
}
pub(crate) async fn new_binding(
    config: &AgentConfig,
    command: &Command,
) -> Result<BindingConfig, PortError> {
    let p = command
        .parameters
        .as_ref()
        .ok_or_else(|| reject("INVALID_ARGUMENT"))?;
    let binding = BindingConfig {
        id: p.binding_id.clone().unwrap(),
        vault_id: p.vault_id.clone().unwrap(),
        path: p.local_path.clone().unwrap().into(),
        cache: p.cache_path.clone().unwrap().into(),
        mode: p.mode.clone().unwrap(),
    };
    if config.bindings.iter().any(|b| b.id == binding.id) {
        return Err(reject("ALREADY_EXISTS"));
    }
    let mut proposed = config.clone();
    proposed.bindings.push(binding.clone());
    admit(&proposed)?;
    Ok(binding)
}
type BindingRow = (String, String, String, String, String, String, bool, bool);

pub(crate) async fn binding_list(db: &Database) -> Result<Vec<BindingInfo>, PortError> {
    let rows: Vec<BindingRow> = sqlx::query_as("SELECT lower(hex(b.id)),lower(hex(b.server_id)),lower(hex(b.vault_id)),b.local_path,b.cache_path,b.mode,coalesce(c.paused,0),coalesce(c.detached,0) FROM bindings b LEFT JOIN binding_controls c ON c.binding_id=b.id ORDER BY b.id")
        .fetch_all(db.pool()).await.map_err(error)?;
    Ok(rows
        .into_iter()
        .map(
            |(binding_id, server_id, vault_id, local_path, cache_path, mode, paused, detached)| {
                BindingInfo {
                    binding_id,
                    server_id: Some(server_id),
                    vault_id,
                    local_path,
                    cache_path,
                    mode,
                    paused,
                    detached,
                }
            },
        )
        .collect())
}

pub(crate) async fn execute(
    db: &Arc<Database>,
    config: &AgentConfig,
    server: Option<(
        &mut ServerConfig,
        &ApiClient<ReqwestTransport>,
        &AuthSession,
    )>,
    running: Running,
    command: &Command,
) -> Result<ManagementResult, PortError> {
    let empty = Default::default();
    let p = command.parameters.as_ref().unwrap_or(&empty);
    let mut result = ManagementResult::default();
    let binding = p
        .binding_id
        .as_deref()
        .map(BindingId::from_hex)
        .transpose()
        .map_err(error)?;
    let control = BindingControls::new(db.clone());
    if let Some(binding) = binding {
        let mut conn = db.pool().acquire().await.map_err(error)?;
        if bindings::binding(&mut conn, binding)
            .await
            .map_err(error)?
            .is_none()
        {
            return Err(reject("NOT_FOUND"));
        }
    }
    match command.name {
        C::ClaimNotification => {
            return crate::management_notices::claim(
                db,
                p.notification_key.as_deref().unwrap(),
                p.notification_generation
                    .as_deref()
                    .unwrap()
                    .parse()
                    .map_err(error)?,
            )
            .await;
        }
        C::ListFiles => {
            return crate::management_listing::list(
                db,
                &running,
                binding.unwrap(),
                p.cursor.as_deref(),
            )
            .await;
        }
        C::DiscoverConnection => return discover_connection(config, command).await,
        C::ListBindings => result.bindings = Some(binding_list(db).await?),
        C::PauseBinding | C::ResumeBinding => {
            control
                .set_paused(binding.unwrap(), command.name == C::PauseBinding)
                .await?
        }
        C::DetachBinding => {
            let report = control.unbind_keep_data(binding.unwrap()).await?;
            result.unconfirmed = Some(report.unconfirmed.iter().map(ToString::to_string).collect());
            result.incomplete = Some(report.incomplete.iter().map(ToString::to_string).collect());
        }
        C::GetRules | C::SetRules => {
            let binding = binding.unwrap();
            if command.name == C::SetRules {
                if !running.contains_key(&binding) {
                    return Err(reject("BINDING_BLOCKED"));
                }
                let importing: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM binding_imports WHERE binding_id=? AND complete=1)")
                    .bind(binding.as_bytes().as_slice()).fetch_one(db.pool()).await.map_err(error)?;
                if importing {
                    return Err(reject("BINDING_BLOCKED"));
                }
                vaultsyncd_fs_common::ignore::IgnoreRules::parse(p.rules.as_deref().unwrap())
                    .map_err(|_| reject("INVALID_ARGUMENT"))?;
                control
                    .change_rules(
                        binding,
                        p.rules_version.as_deref().unwrap(),
                        p.rules.as_deref().unwrap(),
                    )
                    .await
                    .map_err(|failure| {
                        if failure.code() == Some("RULES_VERSION_REUSED") {
                            reject("INVALID_ARGUMENT")
                        } else {
                            failure
                        }
                    })?;
            }
            let (version, rules): (String, String) = sqlx::query_as("SELECT b.rules_version,coalesce(c.rules,'') FROM bindings b LEFT JOIN binding_controls c ON c.binding_id=b.id WHERE b.id=?")
                .bind(binding.as_bytes().as_slice()).fetch_one(db.pool()).await.map_err(error)?;
            if let Some(entry) = p.entry_id.as_deref() {
                let (worker, _) = running
                    .get(&binding)
                    .ok_or_else(|| reject("BINDING_BLOCKED"))?;
                let rows = crate::work::metadata(&worker.importer).await?;
                let row = rows.get(entry).ok_or_else(|| reject("NOT_FOUND"))?;
                let path = crate::work::paths(&rows)?
                    .remove(entry)
                    .ok_or_else(|| reject("NOT_FOUND"))?;
                let decision =
                    vaultsyncd_fs_common::ignore::IgnoreRules::parse(&rules).map_err(error)?;
                result.rules_explanation = Some(if path.is_empty() {
                    "Vault 本身始终保留".into()
                } else {
                    format!(
                        "{:?}",
                        decision.decide(
                            &vaultsyncd_fs_common::path::RelativePath::parse(&path)
                                .map_err(error)?,
                            row.kind == vaultsyncd_protocol::generated::EntryKind::Directory
                        )
                    )
                });
            }
            result.rules_version = Some(version);
            result.rules = Some(rules);
        }
        C::SetMode => {
            let mode = if p.mode.as_deref() == Some("full") {
                SyncMode::Full
            } else {
                SyncMode::OnDemand
            };
            control.change_mode(binding.unwrap(), mode).await?;
            result.mode_remaining = Some(
                control
                    .mode_progress(binding.unwrap())
                    .await?
                    .remaining
                    .to_string(),
            );
        }
        C::ListProblems => {
            let mut items = problems(db, binding).await?;
            if let Some(search) = &p.search {
                let search = search.to_lowercase();
                items.retain(|p| {
                    p.code.to_lowercase().contains(&search)
                        || p.path
                            .as_ref()
                            .is_some_and(|p| p.to_lowercase().contains(&search))
                });
            }
            result.problems = Some(items);
        }
        C::RetryProblem => {
            let binding = binding.unwrap();
            let op = OpId::from_hex(p.operation_id.as_deref().unwrap()).map_err(error)?;
            // OpId is not authority: a different binding's operation is never resumed.
            if let Some((_, scheduler)) = running.get(&binding)
                && scheduler.status(binding).await?.iter().any(|t| t.op == op)
            {
                if control.execution_gate(binding, None).await? != ExecutionGate::Ready {
                    return Err(reject("BINDING_BLOCKED"));
                }
                scheduler.resume(op).await?;
                result.operation_id = Some(op.to_string());
            } else {
                // The scheduler may have resolved the problem between listing and retry.
                // Only report success when no persisted problem still names this operation.
                if !problems(db, Some(binding))
                    .await?
                    .iter()
                    .any(|p| p.problem_id == op.to_string())
                {
                    result.operation_id = Some(op.to_string());
                } else {
                    let (_, client, _) = server.ok_or_else(|| reject("REAUTH_REQUIRED"))?;
                    return crate::management_history::retry_restore(db, binding, op, client).await;
                }
            }
        }
        C::Login => {
            let (server, client, session) = server.ok_or_else(|| reject("NOT_READY"))?;
            client
                .login(&vaultsyncd_protocol::generated::LoginRequest {
                    login: p.username.clone().unwrap(),
                    password: p.password.clone().unwrap(),
                    device_label: "Windows agent".into(),
                })
                .await
                .map_err(connection_error)?;
            let username = p.username.as_ref().unwrap();
            if let Err(cause) = sqlx::query("INSERT INTO agent_login_profiles(server_id,username) VALUES(?,?) ON CONFLICT(server_id) DO UPDATE SET username=excluded.username")
                .bind(&server.id).bind(username).execute(db.pool()).await {
                session.clear();
                return Err(error(cause));
            }
            server.username = Some(username.clone());
            result.connection = Some(connection(server, Some(session)));
        }
        C::Logout => {
            let (server, client, session) = server.ok_or_else(|| reject("NOT_READY"))?;
            client
                .logout()
                .await
                .map_err(vaultsyncd_http_client::transfer_error)?;
            result.connection = Some(connection(server, Some(session)));
        }
        C::ListVaults => {
            let (_, client, _) = server.ok_or_else(|| reject("REAUTH_REQUIRED"))?;
            let vaults: vaultsyncd_protocol::generated::VaultList = client
                .send_json(
                    ApiRequest::new(Method::Get, "/v1/vaults")
                        .with_auth(vaultsyncd_http_client::AuthMode::Bearer),
                )
                .await
                .map_err(connection_error)?;
            result.items = Some(
                vaults
                    .vaults
                    .into_iter()
                    .map(|r| RemoteItem {
                        id: r.vault_id,
                        name: r
                            .name
                            .filter(|name| !name.trim().is_empty())
                            .unwrap_or_else(|| "未提供目录名（请更新服务器）".to_owned()),
                        entry_id: None,
                        version_id: None,
                        revision: None,
                        parent_id: None,
                        size: None,
                        hash: None,
                        expires_at: None,
                        conflict_source: None,
                    })
                    .collect(),
            );
        }
        C::DownloadEntry | C::ReleaseEntry => {
            let binding = binding.unwrap();
            let (worker, _) = running.get(&binding).ok_or_else(|| reject("NOT_READY"))?;
            let entry = EntryId::from_hex(p.entry_id.as_deref().unwrap()).map_err(error)?;
            let op = OpId::from_hex(p.operation_id.as_deref().unwrap()).map_err(error)?;
            if control.execution_gate(binding, Some(entry)).await? != ExecutionGate::Ready {
                return Err(reject("BINDING_BLOCKED"));
            }
            if command.name == C::DownloadEntry {
                crate::management_files::download(worker, entry, op).await?;
            } else {
                let (_, client, _) = server.ok_or_else(|| reject("REAUTH_REQUIRED"))?;
                crate::management_files::release(worker, config, client, entry, op).await?;
            }
            result.operation_id = Some(op.to_string());
        }
        C::ExportRecovery => {
            let binding = binding.unwrap();
            let local = if let Some((worker, _)) = running.get(&binding) {
                worker.importer.local.clone()
            } else {
                let configured = config
                    .bindings
                    .iter()
                    .find(|b| b.id == binding.to_string())
                    .ok_or_else(|| reject("NOT_FOUND"))?;
                Arc::new(
                    crate::local::Local::open(
                        &configured.path,
                        &configured.cache,
                        db.clone(),
                        binding,
                    )
                    .await?,
                )
            };
            result.exported_files = Some(
                crate::management_export::export(local, config, p.destination.as_deref().unwrap())
                    .await?
                    .to_string(),
            );
        }
        C::ListActivity => {
            let (server, client, _) = server.ok_or_else(|| reject("REAUTH_REQUIRED"))?;
            return crate::management_activity::execute(db, config, &server.id, client, command)
                .await;
        }
        C::ListHistory | C::ListTrash | C::RestoreEntry => {
            let (_, client, _) = server.ok_or_else(|| reject("REAUTH_REQUIRED"))?;
            return crate::management_history::execute(db, config, client, command).await;
        }
        // Connection changes are applied by the runtime, which owns the Credential Manager scope.
        _ => return Err(reject("INVALID_ARGUMENT")),
    }
    Ok(result)
}
pub(crate) fn connection(server: &ServerConfig, session: Option<&AuthSession>) -> ConnectionInfo {
    ConnectionInfo {
        server_id: server.id.clone(),
        server_url: server.url.clone(),
        ca_file: server
            .ca_file
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        authenticated: session.is_some_and(|s| s.has_credentials() && !s.needs_reauth()),
        username: server.username.clone(),
    }
}
type ProblemRow = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    bool,
    Option<String>,
);
pub(crate) async fn problems(
    db: &Database,
    binding: Option<BindingId>,
) -> Result<Vec<Problem>, PortError> {
    let rows: Vec<ProblemRow> = sqlx::query_as(
        "WITH origins(binding_id,problem_id,entry_id,code,retryable,path) AS (
         SELECT p.binding_id,lower(hex(p.op_id)),p.entry_id,coalesce(p.last_error_code,'CANCELLED'),1,NULL FROM pending_ops p LEFT JOIN operation_schedule s ON s.op_id=p.op_id WHERE p.last_error_code IS NOT NULL OR coalesce(s.cancelled,0)=1 OR coalesce(s.halted,0)=1
         UNION ALL SELECT binding_id,'event:'||seq,NULL,last_error_code,0,NULL FROM received_changes WHERE state='blocked'
         UNION ALL SELECT binding_id,'intent:'||intent_id,entry_id,last_error_code,0,target_path FROM local_intents WHERE state='failed'
         UNION ALL SELECT t.binding_id,'transfer:'||lower(hex(t.version_id)),v.entry_id,t.last_error_code,0,NULL FROM transfers t LEFT JOIN agent_versions v ON v.binding_id=t.binding_id AND v.version_id=t.version_id WHERE t.state='failed'
         UNION ALL SELECT b.id,'notification:'||n.dedupe_key,n.entry_id,n.code,0,NULL FROM notifications n JOIN bindings b ON n.binding_id=b.id OR (n.binding_id IS NULL AND (n.dedupe_key LIKE 'server:'||lower(hex(b.server_id))||':vault:'||lower(hex(b.vault_id))||':%' OR (n.dedupe_key NOT LIKE 'vault:%' AND n.dedupe_key NOT LIKE 'server:%'))) WHERE n.state='active'
         UNION ALL SELECT a.binding_id,lower(hex(a.op_id)),NULL,'MANAGEMENT_PENDING',1,NULL FROM agent_actions a WHERE json_extract(a.specification,'$.kind')='management_restore' AND a.result IS NULL
        ) SELECT lower(hex(o.binding_id)),o.problem_id,CASE WHEN o.entry_id IS NOT NULL THEN lower(hex(o.entry_id)) END,o.code,l.dirty,l.residency,o.retryable,o.path FROM origins o LEFT JOIN local_entries l ON l.binding_id=o.binding_id AND l.entry_id=o.entry_id WHERE (? IS NULL OR o.binding_id=?) ORDER BY o.binding_id,o.problem_id")
        .bind(binding.map(|b|b.as_bytes().to_vec())).bind(binding.map(|b|b.as_bytes().to_vec())).fetch_all(db.pool()).await.map_err(error)?;
    let locations: Vec<(String,String,String)> = sqlx::query_as("WITH RECURSIVE paths(binding_id,entry_id,path,depth) AS (SELECT binding_id,entry_id,'',0 FROM baseline_entries WHERE parent_entry_id IS NULL UNION ALL SELECT b.binding_id,b.entry_id,CASE WHEN p.path='' THEN b.name ELSE p.path||'/'||b.name END,p.depth+1 FROM baseline_entries b JOIN paths p ON b.binding_id=p.binding_id AND b.parent_entry_id=p.entry_id WHERE p.depth<128) SELECT lower(hex(binding_id)),lower(hex(entry_id)),path FROM paths")
        .fetch_all(db.pool()).await.map_err(error)?;
    let mut seen = std::collections::HashSet::new();
    let mut result = vec![];
    for (binding_id, problem_id, entry_id, code, dirty, residency, retryable, path) in rows {
        if !seen.insert((binding_id.clone(), problem_id.clone())) {
            continue;
        }
        if vaultsyncd_domain::error::ErrorCode::from_code(&code).is_none()
            && !matches!(code.as_str(), "CANCELLED" | "MANAGEMENT_PENDING")
        {
            return Err(PortError::protocol("unknown persisted problem code"));
        }
        let path = path.or_else(|| {
            locations
                .iter()
                .find(|(b, e, _)| b == &binding_id && Some(e) == entry_id.as_ref())
                .map(|(_, _, p)| p.clone())
        });
        let reason = if problem_id.ends_with(":CONFLICT_COPY") {
            "双方修改已保留为冲突副本，请检查内容；关闭通知不会删除副本。".into()
        } else {
            format!("操作尚未完成（{code}）")
        };
        result.push(Problem {
            binding_id,
            problem_id,
            entry_id,
            path,
            reason,
            code,
            local_data: format!(
                "本地状态：{}；驻留：{}；未确认内容和队列保留",
                dirty.as_deref().unwrap_or("unknown"),
                residency.as_deref().unwrap_or("unknown")
            ),
            next_step: if retryable {
                "核对原因后立即重试；未确认内容可先导出恢复包"
            } else {
                "恢复权限/存储或重新连接后复核；不要删除本地恢复现场"
            }
            .into(),
            retryable,
        });
    }
    Ok(result)
}
