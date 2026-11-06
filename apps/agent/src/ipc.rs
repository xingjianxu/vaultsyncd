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
    connection: watch::Receiver<ConnectionInfo>,
    problems: watch::Receiver<Vec<Problem>>,
}

pub(crate) fn start(
    db: Arc<Database>,
    commands: mpsc::Sender<CommandRequest>,
    connection: watch::Receiver<ConnectionInfo>,
    problems: watch::Receiver<Vec<Problem>>,
) -> io::Result<Server> {
    let (updates, _) = watch::channel(0);
    let mut nonce = [0; 16];
    OsRng.fill_bytes(&mut nonce);
    let hub = Arc::new(Hub {
        state: Mutex::new((0, None)),
        connection,
        problems,
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
                if request.command.name != CommandName::GetStatus && request.ipc_version.minor < 2 {
                    return Ok(failure(
                        &request,
                        LocalIpcErrorCode::UnsupportedVersion,
                        "此命令需要本机 IPC 1.2",
                        false,
                    ));
                }
                if let Err(error) = vaultsyncd_local_ipc::commands::validate(&request.command) {
                    return Ok(Response::error(request.request_id, error));
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
        let s = status::snapshot(db, binding).await?;
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
    let mut value = GetStatusResult {
        agent_version: env!("CARGO_PKG_VERSION").into(),
        ipc_protocol: version::supported(),
        bindings: Some(bindings),
        snapshot_version: None,
        management: Some(ManagementResult {
            bindings: Some(info),
            connection: Some(hub.connection.borrow().clone()),
            problems: Some(hub.problems.borrow().clone()),
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
            let source_code = if error.needs_reauth() {
                "REAUTH_REQUIRED"
            } else {
                error.code().unwrap_or("UNAVAILABLE")
            };
            let (code, retryable) = match source_code {
                "NOT_FOUND" => (LocalIpcErrorCode::NotFound, false),
                "INVALID_ARGUMENT" | "PATH_OVERLAP" => (LocalIpcErrorCode::InvalidArgument, false),
                "REAUTH_REQUIRED" | "UNAUTHORIZED" => (LocalIpcErrorCode::Unauthenticated, false),
                "ALREADY_EXISTS" | "OP_ID_REUSED" | "BASE_MISMATCH" | "SUBTREE_CHANGED" => {
                    (LocalIpcErrorCode::Conflict, false)
                }
                "ACCESS_DENIED"
                | "AUTH_EXPIRED"
                | "VERSION_GONE"
                | "INVALID_REQUEST"
                | "INVALID_NAME"
                | "NEEDS_RECOVERY"
                | "ROOT_UNAVAILABLE"
                | "INSUFFICIENT_STORAGE" => (LocalIpcErrorCode::Blocked, false),
                "NOT_READY"
                | "BINDING_BLOCKED"
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
                        | "LOCAL_IN_USE"
                        | "UNKNOWN_LOCAL_STATE"
                        | "PENDING"
                        | "INVALID_REQUEST"
                        | "SERVICE_NOT_READY"
                );
            let stable = if known { source_code } else { "UNAVAILABLE" };
            eprintln!(
                "event=management-failed command={:?} code={stable}",
                request.command.name
            );
            // Never Display/Debug HTTP/filesystem errors or requests; both can contain secrets.
            failure(
                request,
                code,
                &format!("操作未完成（{stable}）；本地内容、待办与恢复现场已保留"),
                retryable,
            )
        }
    }
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
