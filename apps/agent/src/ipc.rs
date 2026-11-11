//! Authenticated transport assembly, executor mailbox, and versioned full-state subscription.
#[cfg(test)]
#[path = "ipc_tests.rs"]
mod tests;
use rand_core::{OsRng, RngCore};
use std::{
    io,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use vaultsyncd_agent_core::status;
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::ids::BindingId;
use vaultsyncd_local_ipc::{generated::*, service::Service, version};
use vaultsyncd_platform_windows::ipc::{Endpoint, Server};
use vaultsyncd_transfer::ports::PortError;

pub(crate) struct CommandRequest {
    pub request: Request,
    pub reply: oneshot::Sender<Response>,
}
struct Hub {
    state: Mutex<(u64, Option<GetStatusResult>)>,
    updates: watch::Sender<u64>,
    instance: String,
    connections: watch::Receiver<Vec<ConnectionInfo>>,
    problems: watch::Receiver<Vec<Problem>>,
    offline: watch::Receiver<std::collections::HashSet<BindingId>>,
}

pub(crate) fn start(
    db: Arc<Database>,
    commands: mpsc::Sender<CommandRequest>,
    connections: watch::Receiver<Vec<ConnectionInfo>>,
    problems: watch::Receiver<Vec<Problem>>,
    offline: watch::Receiver<std::collections::HashSet<BindingId>>,
) -> io::Result<Server> {
    let (updates, _) = watch::channel(0);
    let mut nonce = [0; 16];
    OsRng.fill_bytes(&mut nonce);
    let hub = Arc::new(Hub {
        state: Mutex::new((0, None)),
        connections,
        problems,
        offline,
        updates,
        instance: nonce.iter().map(|b| format!("{b:02x}")).collect(),
    });
    let server = Server::bind(Endpoint::current("vaultsyncd-agent")?, {
        let db = db.clone();
        let hub = hub.clone();
        move |request| {
            let db = db.clone();
            let commands = commands.clone();
            let hub = hub.clone();
            async move {
                if !version::is_compatible(&request.ipc_version) {
                    return Ok(Service::new(env!("CARGO_PKG_VERSION")).handle(&request));
                }
                // IPC 3.0 includes all current commands. Old 2.x minor gates do not
                // carry across the incompatible extra-rules semantic boundary above.
                if let Err(error) = vaultsyncd_local_ipc::commands::validate(&request.command) {
                    return Ok(Response::error(request.request_id, error));
                }
                if !matches!(
                    request.command.name,
                    CommandName::GetStatus
                        | CommandName::SubscribeStatus
                        | CommandName::PrepareMaintenance
                ) {
                    let pending = db
                        .path()
                        .parent()
                        .ok_or_else(|| io::Error::other("state absent"))
                        .and_then(crate::maintenance::trial_pending);
                    if !matches!(pending, Ok(false)) {
                        return Ok(failure(
                            &request,
                            LocalIpcErrorCode::Unavailable,
                            "客户端更新尚未通过启动验证，请等待更新完成或恢复该事务",
                            true,
                        ));
                    }
                }
                if matches!(
                    request.command.name,
                    CommandName::GetStatus | CommandName::SubscribeStatus
                ) {
                    let mut receiver = hub.updates.subscribe();
                    let result = snapshot(&db, &hub).await;
                    let result = if request.command.name == CommandName::SubscribeStatus {
                        let after = request
                            .command
                            .parameters
                            .as_ref()
                            .and_then(|p| p.after_version.as_ref());
                        if result
                            .as_ref()
                            .is_ok_and(|s| s.snapshot_version.as_ref() == after)
                        {
                            let _ =
                                tokio::time::timeout(Duration::from_secs(3), receiver.changed())
                                    .await;
                            snapshot(&db, &hub).await
                        } else {
                            result
                        }
                    } else {
                        result
                    };
                    return Ok(match result {
                        Ok(s) => Response::ok(request.request_id, s),
                        Err(_) => failure(
                            &request,
                            LocalIpcErrorCode::Unavailable,
                            "无法读取完整状态，请重新查询",
                            true,
                        ),
                    });
                }
                let (reply, response) = oneshot::channel();
                if commands
                    .try_send(CommandRequest {
                        request: request.clone(),
                        reply,
                    })
                    .is_err()
                {
                    return Ok(failure(
                        &request,
                        LocalIpcErrorCode::Unavailable,
                        "管理队列繁忙，请稍后重试",
                        true,
                    ));
                }
                response
                    .await
                    .map_err(|_| io::Error::other("management executor stopped"))
            }
        }
    })?;
    tokio::spawn(refresh(db, Arc::downgrade(&hub)));
    Ok(server)
}
async fn refresh(db: Arc<Database>, hub: Weak<Hub>) {
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let Some(hub) = hub.upgrade() else { break };
        // A failed read never publishes a green replacement; next query receives the real failure.
        let _ = snapshot(&db, &hub).await;
    }
}
async fn snapshot(db: &Database, hub: &Hub) -> Result<GetStatusResult, PortError> {
    // Serialize refresh/query so an older observation cannot overwrite a newer published version.
    let mut state = hub.state.lock().await;
    let info = crate::management::binding_list(db).await?;
    let mut bindings = Vec::with_capacity(info.len());
    for b in &info {
        let binding = BindingId::from_hex(&b.binding_id).map_err(crate::local::error)?;
        let mut s = status::snapshot(db, binding).await?;
        if hub.offline.borrow().contains(&binding) {
            s.offline += 1;
        }
        bindings.push(BindingStatus {
            binding_id: b.binding_id.clone(),
            synchronized: s.synchronized(),
            dirty: s.dirty.to_string(),
            metadata_only: s.residency[0].to_string(),
            partial: s.residency[1].to_string(),
            materialized: s.residency[2].to_string(),
            pending: s.pending.to_string(),
            failed: s.failed.to_string(),
            offline: s.offline.to_string(),
            transfers: s.transfers.to_string(),
            intents: s.intents.to_string(),
            recovery: s.recovery.to_string(),
            event_lag: s.event_lag.to_string(),
            mode_pending: s.mode_pending,
            paused: s.paused,
            detached: s.detached,
            unknown: s.unknown.to_string(),
        });
    }
    let mut problems = crate::management::problems(db, None).await?;
    for problem in hub.problems.borrow().iter() {
        if !problems
            .iter()
            .any(|p| p.binding_id == problem.binding_id && p.problem_id == problem.problem_id)
        {
            problems.push(problem.clone());
        }
    }
    let notices = crate::management_notices::list(db).await?;
    let connections = hub.connections.borrow().clone();
    let connection = (connections.len() == 1).then(|| connections[0].clone());
    let mut value = GetStatusResult {
        agent_version: env!("CARGO_PKG_VERSION").into(),
        ipc_protocol: version::supported(),
        bindings: Some(bindings),
        snapshot_version: None,
        management: Some(ManagementResult {
            bindings: Some(info),
            connection,
            connections: Some(connections),
            problems: Some(problems),
            notices: Some(notices),
            ..Default::default()
        }),
    };
    if state.1.as_ref() != Some(&value) {
        state.0 = state
            .0
            .checked_add(1)
            .ok_or_else(|| PortError::protocol("status version exhausted"))?;
        state.1 = Some(value.clone());
        hub.updates.send_replace(state.0);
    }
    value.snapshot_version = Some(format!("{}:{}", hub.instance, state.0));
    Ok(value)
}
pub(crate) fn management_response(
    request: &Request,
    result: Result<ManagementResult, PortError>,
) -> Response {
    match result {
        Ok(result) => Response::ok(
            request.request_id.clone(),
            GetStatusResult {
                agent_version: env!("CARGO_PKG_VERSION").into(),
                ipc_protocol: version::supported(),
                bindings: None,
                snapshot_version: None,
                management: Some(result),
            },
        ),
        Err(error) => {
            let source_code = if error.is_no_space() {
                "INSUFFICIENT_STORAGE"
            } else if error.needs_reauth() {
                "REAUTH_REQUIRED"
            } else {
                error.code().unwrap_or("UNAVAILABLE")
            };
            let (code, retryable) = match source_code {
                "NOT_FOUND" => (LocalIpcErrorCode::NotFound, false),
                "INVALID_ARGUMENT"
                | "PATH_OVERLAP"
                | "LOCAL_PATH_UNAVAILABLE"
                | "SERVER_REQUIRED" => (LocalIpcErrorCode::InvalidArgument, false),
                "CERTIFICATE_INVALID" | "CONNECTION_SERVER_MISMATCH" => {
                    (LocalIpcErrorCode::Blocked, false)
                }
                "REAUTH_REQUIRED" | "UNAUTHORIZED" | "AUTH_EXPIRED" => {
                    (LocalIpcErrorCode::Unauthenticated, false)
                }
                "RESTORE_REJECTED" | "ALREADY_EXISTS" | "OP_ID_REUSED" | "BASE_MISMATCH"
                | "SUBTREE_CHANGED" => (LocalIpcErrorCode::Conflict, false),
                "ACCESS_DENIED"
                | "VERSION_GONE"
                | "INVALID_REQUEST"
                | "INVALID_NAME"
                | "NEEDS_RECOVERY"
                | "VAULT_UNAVAILABLE"
                | "INSUFFICIENT_STORAGE" => (LocalIpcErrorCode::Blocked, false),
                "NOT_READY"
                | "BINDING_BLOCKED"
                | "CONNECTION_IN_USE"
                | "BINDING_UNBOUND"
                | "MODE_REQUIRES_LOCAL_COPY"
                | "LOCAL_CHANGED"
                | "LOCAL_IN_USE"
                | "UNKNOWN_LOCAL_STATE" => (LocalIpcErrorCode::Blocked, false),
                _ => (LocalIpcErrorCode::Unavailable, true),
            };
            let known = vaultsyncd_domain::error::ErrorCode::from_code(source_code).is_some()
                || matches!(
                    source_code,
                    "NOT_FOUND"
                        | "INVALID_ARGUMENT"
                        | "PATH_OVERLAP"
                        | "REAUTH_REQUIRED"
                        | "ALREADY_EXISTS"
                        | "NOT_READY"
                        | "BINDING_BLOCKED"
                        | "CONNECTION_IN_USE"
                        | "BINDING_UNBOUND"
                        | "LOCAL_IN_USE"
                        | "UNKNOWN_LOCAL_STATE"
                        | "PENDING"
                        | "RESTORE_REJECTED"
                        | "INVALID_REQUEST"
                        | "SERVICE_NOT_READY"
                        | "CERTIFICATE_INVALID"
                        | "SERVER_REQUIRED"
                        | "CONNECTION_SERVER_MISMATCH"
                        | "LOCAL_PATH_UNAVAILABLE"
                );
            let stable = if known { source_code } else { "UNAVAILABLE" };
            eprintln!(
                "event=management-failed command={:?} code={stable}",
                request.command.name
            );
            // Never Display/Debug HTTP/filesystem errors or requests; both can contain secrets.
            let message = match stable {
                "RESTORE_REJECTED" => "服务器已拒绝此次恢复（RESTORE_REJECTED）；未完成覆盖，请重新读取清单和当前修订后选择操作。".to_owned(),
                "CERTIFICATE_INVALID" => "服务器证书验证失败（CERTIFICATE_INVALID）；请核对服务器地址与名称，或在连接设置中选择管理员提供的附加 CA。证书验证未被关闭。".to_owned(),
                "LOCAL_PATH_UNAVAILABLE" => "本地目录无法访问（LOCAL_PATH_UNAVAILABLE）；请选择现有本机 NTFS 目录，并检查路径、权限与卷状态。".to_owned(),
                "INSUFFICIENT_STORAGE" => "本地磁盘或状态库空间不足（INSUFFICIENT_STORAGE）；完整缓存、未确认内容与恢复现场保留，请腾出空间后重试。".to_owned(),
                "CONNECTION_SERVER_MISMATCH" => identity_mismatch_message(&error),
                "SERVER_REQUIRED" => "客户端有多个服务器连接，请明确选择目标服务器（SERVER_REQUIRED）。".to_owned(),
                "CONNECTION_IN_USE" => "该服务器仍有正在管理的本地绑定（CONNECTION_IN_USE）；请先在「同步目录」解除绑定并确认保留的数据，再移除服务器。".to_owned(),
                "PATH_OVERLAP" => "目录重叠（PATH_OVERLAP）；同步目录须与其他正在管理的绑定、保留缓存和 agent 状态目录分离，不能嵌套旧同步根。".to_owned(),
                "MODE_REQUIRES_LOCAL_COPY" => "目录仍有未完整下载或无法确认的内容（MODE_REQUIRES_LOCAL_COPY）；重新绑定前须先恢复并下载完整内容。旧文件、缓存与恢复记录均已保留。".to_owned(),
                _ => format!("操作未完成（{stable}）；本地内容、待办与恢复现场已保留"),
            };
            failure(request, code, &message, retryable)
        }
    }
}
fn identity_mismatch_message(error: &PortError) -> String {
    use vaultsyncd_domain::ids::ServerId;
    if let PortError::Rejected { detail, .. } = error
        && let Some((expected, actual)) = detail.split_once(':')
        && [expected, actual]
            .into_iter()
            .all(|id| ServerId::from_hex(id).is_ok_and(|server| server.to_string() == id))
    {
        return format!(
            "服务器返回的 ID 与确认值不同。确认值 {expected}，服务器返回 {actual}。请重新读取 Server ID。"
        );
    }
    "服务器返回的 ID 与确认值不同，请重新读取 Server ID。".into()
}

fn failure(request: &Request, code: LocalIpcErrorCode, message: &str, retryable: bool) -> Response {
    Response::error(
        request.request_id.clone(),
        ErrorBody {
            code,
            message: message.into(),
            retryable,
        },
    )
}
