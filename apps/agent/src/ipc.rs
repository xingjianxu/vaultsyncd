//! Read-only IPC assembly. Core/store remain the sole source of current sync state.
use std::{io, sync::Arc};
use vaultsyncd_agent_core::status;
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::ids::BindingId;
use vaultsyncd_local_ipc::{generated::BindingStatus, service::Service};
use vaultsyncd_platform_windows::ipc::{Endpoint, Server};

pub(crate) fn start(db: Arc<Database>, bindings: Vec<BindingId>) -> io::Result<Server> {
    let service = Service::new(env!("CARGO_PKG_VERSION"));
    Server::bind(Endpoint::current("vaultsyncd-agent")?, move |request| {
        let db = db.clone();
        let bindings = bindings.clone();
        let service = service.clone();
        async move {
            let mut response = service.handle(&request);
            if let Some(result) = &mut response.result {
                let mut snapshots = Vec::with_capacity(bindings.len());
                for binding in bindings {
                    // A failed read is a failed request, never a default green/empty snapshot.
                    let s = status::snapshot(&db, binding)
                        .await
                        .map_err(|_| io::Error::other("agent status snapshot unavailable"))?;
                    snapshots.push(BindingStatus {
                        binding_id: binding.to_string(),
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
                result.bindings = Some(snapshots);
            }
            Ok(response)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vaultsyncd_local_ipc::{
        generated::{Command, CommandName, Request},
        version,
    };

    #[tokio::test]
    async fn unavailable_snapshot_fails_without_fake_empty_status_and_version_is_checked_first() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("s071-status-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        let db = Arc::new(Database::open(path.join("client.sqlite")).await.unwrap());
        let server = start(db.clone(), vec![BindingId::from_bytes([17; 16])]).unwrap();
        let endpoint = Endpoint::current("vaultsyncd-agent").unwrap();
        let mut request = Request {
            request_id: "missing-durable-binding".into(),
            ipc_version: version::supported(),
            command: Command {
                name: CommandName::GetStatus,
            },
        };
        assert!(
            endpoint.request(&request).await.is_err(),
            "missing snapshot must close, not report empty success"
        );
        request.ipc_version.major += 1;
        assert!(endpoint.request(&request).await.unwrap().error.is_some());
        server.shutdown().await.unwrap();
        let server = start(db.clone(), Vec::new()).unwrap();
        request.ipc_version = version::supported();
        assert_eq!(
            endpoint
                .request(&request)
                .await
                .unwrap()
                .result
                .unwrap()
                .bindings,
            Some(Vec::new())
        );
        server.shutdown().await.unwrap();
        db.pool().close().await;
        drop(db);
        std::fs::remove_dir_all(path).unwrap();
    }
}
