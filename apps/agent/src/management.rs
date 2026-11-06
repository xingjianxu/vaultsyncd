//! Shared management executor. All user-facing operations resolve identities from durable state.
use crate::{
    config::{AgentConfig, BindingConfig},
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
    db::Database,
};
use vaultsyncd_domain::ids::{BindingId, EntryId, OpId};
use vaultsyncd_http_client::{ApiClient, ApiRequest, AuthSession, Method, ReqwestTransport};
use vaultsyncd_local_ipc::generated::{
    BindingInfo, Command, CommandName as C, ManagementResult, Problem, RemoteItem,
};
use vaultsyncd_platform_windows::files::RootHandle;
use vaultsyncd_transfer::ports::PortError;

pub(crate) type Running = HashMap<BindingId, (Worker, Arc<Scheduler<Dispatch>>)>;
fn reject(code: &str) -> PortError {
    PortError::rejected(code, "management command rejected")
}

pub(crate) async fn load_connection(
    db: &Database,
    config: &mut AgentConfig,
) -> Result<(), PortError> {
    let stored: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT server_id,server_url,ca_file FROM agent_connection WHERE singleton=1",
    )
    .fetch_optional(db.pool())
    .await
    .map_err(error)?;
    if let Some((id, url, ca_file)) = stored {
        config.server.id = id;
        config.server.url = url;
        config.server.ca_file = ca_file.map(Into::into);
        config.validate().map_err(error)?;
    }
    Ok(())
}
pub(crate) async fn proposed_connection(
    db: &Database,
    config: &AgentConfig,
    command: &Command,
) -> Result<AgentConfig, PortError> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM bindings")
        .fetch_one(db.pool())
        .await
        .map_err(error)?;
    let p = command
        .parameters
        .as_ref()
        .ok_or_else(|| reject("INVALID_ARGUMENT"))?;
    if (count != 0 || !config.bindings.is_empty())
        && p.server_id.as_deref() != Some(&config.server.id)
    {
        return Err(reject("BINDING_BLOCKED"));
    }
    let mut proposed = config.clone();
    proposed.server.id = p.server_id.clone().unwrap();
    proposed.server.url = p.server_url.clone().unwrap();
    proposed.server.ca_file = p.ca_file.as_ref().map(Into::into);
    proposed
        .validate()
        .map_err(|_| reject("INVALID_ARGUMENT"))?;
    Ok(proposed)
}
pub(crate) async fn save_connection(db: &Database, config: &AgentConfig) -> Result<(), PortError> {
    let mut tx = db.begin_write().await.map_err(error)?;
    sqlx::query("INSERT INTO agent_connection(singleton,server_id,server_url,ca_file) VALUES(1,?,?,?) ON CONFLICT(singleton) DO UPDATE SET server_id=excluded.server_id,server_url=excluded.server_url,ca_file=excluded.ca_file")
        .bind(&config.server.id).bind(&config.server.url).bind(config.server.ca_file.as_ref().map(|p|p.to_string_lossy().into_owned())).execute(&mut *tx).await.map_err(error)?;
    tx.commit().await.map_err(error)
}

pub(crate) async fn load_bindings(
    db: &Database,
    config: &mut AgentConfig,
) -> Result<(), PortError> {
    let rows: Vec<(String,String,String,String,String)> = sqlx::query_as("SELECT lower(hex(id)),lower(hex(root_id)),local_path,cache_path,mode FROM bindings WHERE server_id=? AND device_id=? ORDER BY id")
        .bind(vaultsyncd_domain::ids::ServerId::from_hex(&config.server.id).map_err(error)?.as_bytes().as_slice())
        .bind(vaultsyncd_domain::ids::DeviceId::from_hex(&config.device_id).map_err(error)?.as_bytes().as_slice())
        .fetch_all(db.pool()).await.map_err(error)?;
    for (id, root_id, path, cache, mode) in rows {
        if let Some(existing) = config.bindings.iter_mut().find(|b| b.id == id) {
            if existing.root_id != root_id
                || existing.path != Path::new(&path)
                || existing.cache != Path::new(&cache)
            {
                return Err(reject("CONFIGURATION_CHANGED"));
            }
            existing.mode = mode;
        } else {
            config.bindings.push(BindingConfig {
                id,
                root_id,
                path: path.into(),
                cache: cache.into(),
                mode,
            });
        }
    }
    Ok(())
}

/// Recheck physical ancestry, including drive aliases; neither a GUI check nor string prefixes suffice.
pub(crate) fn admit(config: &AgentConfig) -> Result<(), PortError> {
    config.validate().map_err(error)?;
    let state = RootHandle::open(&config.state_dir).map_err(error)?;
    let mut roots = vec![state];
    for b in &config.bindings {
        for path in [&b.path, &b.cache] {
            let root = RootHandle::open(path).map_err(error)?;
            if roots.iter().any(|old| old.overlaps(&root)) {
                return Err(reject("PATH_OVERLAP"));
            }
            roots.push(root);
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
        root_id: p.root_id.clone().unwrap(),
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
pub(crate) async fn binding_list(db: &Database) -> Result<Vec<BindingInfo>, PortError> {
    let rows: Vec<(String,String,String,String,String,bool,bool)> = sqlx::query_as("SELECT lower(hex(b.id)),lower(hex(b.root_id)),b.local_path,b.cache_path,b.mode,coalesce(c.paused,0),coalesce(c.detached,0) FROM bindings b LEFT JOIN binding_controls c ON c.binding_id=b.id ORDER BY b.id")
        .fetch_all(db.pool()).await.map_err(error)?;
    Ok(rows
        .into_iter()
        .map(
            |(binding_id, root_id, local_path, cache_path, mode, paused, detached)| BindingInfo {
                binding_id,
                root_id,
                local_path,
                cache_path,
                mode,
                paused,
                detached,
            },
        )
        .collect())
}

pub(crate) async fn execute(
    db: &Arc<Database>,
    config: &AgentConfig,
    client: &ApiClient<ReqwestTransport>,
    session: &AuthSession,
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
                    return crate::management_history::retry_restore(db, binding, op, client).await;
                }
            }
        }
        C::GetConnection => result.connection = Some(connection(config, session)),
        C::Login => {
            client
                .login(&vaultsyncd_protocol::generated::LoginRequest {
                    login: p.username.clone().unwrap(),
                    password: p.password.clone().unwrap(),
                    device_label: "Windows agent".into(),
                })
                .await
                .map_err(vaultsyncd_http_client::transfer_error)?;
            result.connection = Some(connection(config, session));
        }
        C::Logout => {
            client
                .logout()
                .await
                .map_err(vaultsyncd_http_client::transfer_error)?;
            result.connection = Some(connection(config, session));
        }
        C::ListRoots => {
            let roots: vaultsyncd_protocol::generated::RootList = client
                .send_json(
                    ApiRequest::new(Method::Get, "/v1/roots")
                        .with_auth(vaultsyncd_http_client::AuthMode::Bearer),
                )
                .await
                .map_err(vaultsyncd_http_client::transfer_error)?;
            result.items = Some(
                roots
                    .roots
                    .into_iter()
                    .map(|r| RemoteItem {
                        id: r.root_id.clone(),
                        name: r.root_id,
                        entry_id: None,
                        version_id: None,
                        revision: None,
                        parent_id: None,
                        size: None,
                        hash: None,
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
        C::ListHistory | C::ListTrash | C::RestoreEntry => {
            return crate::management_history::execute(db, config, client, command).await;
        }
        // Connection changes are applied by the runtime, which owns the Credential Manager scope.
        _ => return Err(reject("INVALID_ARGUMENT")),
    }
    Ok(result)
}
pub(crate) fn connection(
    config: &AgentConfig,
    session: &AuthSession,
) -> vaultsyncd_local_ipc::generated::ConnectionInfo {
    vaultsyncd_local_ipc::generated::ConnectionInfo {
        server_id: config.server.id.clone(),
        server_url: config.server.url.clone(),
        ca_file: config
            .server
            .ca_file
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        authenticated: session.has_credentials(),
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
async fn problems(db: &Database, binding: Option<BindingId>) -> Result<Vec<Problem>, PortError> {
    let rows: Vec<ProblemRow> = sqlx::query_as(
        "WITH origins(binding_id,problem_id,entry_id,code,retryable,path) AS (
         SELECT p.binding_id,lower(hex(p.op_id)),p.entry_id,coalesce(p.last_error_code,'CANCELLED'),1,NULL FROM pending_ops p LEFT JOIN operation_schedule s ON s.op_id=p.op_id WHERE p.last_error_code IS NOT NULL OR coalesce(s.cancelled,0)=1 OR coalesce(s.halted,0)=1
         UNION ALL SELECT binding_id,'event:'||seq,NULL,last_error_code,0,NULL FROM received_changes WHERE state='blocked'
         UNION ALL SELECT binding_id,'intent:'||intent_id,entry_id,last_error_code,0,target_path FROM local_intents WHERE state='failed'
         UNION ALL SELECT t.binding_id,'transfer:'||lower(hex(t.version_id)),v.entry_id,t.last_error_code,0,NULL FROM transfers t LEFT JOIN agent_versions v ON v.binding_id=t.binding_id AND v.version_id=t.version_id WHERE t.state='failed'
         UNION ALL SELECT b.id,'notification:'||n.dedupe_key,n.entry_id,n.code,0,NULL FROM notifications n JOIN bindings b ON n.binding_id=b.id OR (n.binding_id IS NULL AND (n.dedupe_key LIKE 'root:'||lower(hex(b.root_id))||':%' OR n.dedupe_key NOT LIKE 'root:%')) WHERE n.state='active'
         UNION ALL SELECT a.binding_id,lower(hex(a.op_id)),NULL,'MANAGEMENT_PENDING',1,NULL FROM agent_actions a WHERE json_extract(a.specification,'$.kind')='management_restore' AND a.result IS NULL
        ) SELECT lower(hex(o.binding_id)),o.problem_id,CASE WHEN o.entry_id IS NOT NULL THEN lower(hex(o.entry_id)) END,o.code,l.dirty,l.residency,o.retryable,o.path FROM origins o LEFT JOIN local_entries l ON l.binding_id=o.binding_id AND l.entry_id=o.entry_id WHERE (? IS NULL OR o.binding_id=?) ORDER BY o.binding_id,o.problem_id")
        .bind(binding.map(|b|b.as_bytes().to_vec())).bind(binding.map(|b|b.as_bytes().to_vec())).fetch_all(db.pool()).await.map_err(error)?;
    let locations: Vec<(String,String,String)> = sqlx::query_as("WITH RECURSIVE paths(binding_id,entry_id,path,depth) AS (SELECT binding_id,entry_id,'',0 FROM baseline_entries WHERE parent_entry_id IS NULL UNION ALL SELECT b.binding_id,b.entry_id,CASE WHEN p.path='' THEN b.name ELSE p.path||'/'||b.name END,p.depth+1 FROM baseline_entries b JOIN paths p ON b.binding_id=p.binding_id AND b.parent_entry_id=p.entry_id WHERE p.depth<128) SELECT lower(hex(binding_id)),lower(hex(entry_id)),path FROM paths")
        .fetch_all(db.pool()).await.map_err(error)?;
    let mut seen = std::collections::HashSet::new();
    let mut result = vec![];
    for (binding_id, problem_id, entry_id, code, dirty, residency, retryable, path) in rows {
        if !seen.insert(problem_id.clone()) {
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
        result.push(Problem {
            binding_id,
            problem_id,
            entry_id,
            path,
            reason: format!("操作尚未完成（{code}）"),
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
