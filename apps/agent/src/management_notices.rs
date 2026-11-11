//! The agent owns problem lifecycle and claims; the GUI cannot mutate a database.
use crate::local::error;
use vaultsyncd_client_store::{
    db::Database,
    notifications::{self, NewNotification, delivery},
};
use vaultsyncd_domain::{
    error::ErrorCode,
    ids::{ServerId, VaultId},
};
use vaultsyncd_local_ipc::generated::{ManagementResult, Notice};
use vaultsyncd_transfer::ports::PortError;

pub(crate) fn problem(
    server: ServerId,
    vault: VaultId,
    code: ErrorCode,
    cause: &str,
) -> NewNotification {
    NewNotification {
        dedupe_key: format!("server:{server}:vault:{vault}:{cause}"),
        binding: None,
        entry: None,
        code,
    }
}

pub(crate) async fn raise(
    db: &Database,
    server: ServerId,
    vault: VaultId,
    code: ErrorCode,
    cause: &str,
) -> Result<(), PortError> {
    let mut tx = db.begin_write().await.map_err(error)?;
    notifications::raise(&mut tx, &problem(server, vault, code, cause))
        .await
        .map_err(error)?;
    tx.commit().await.map_err(error)
}

pub(crate) async fn list(db: &Database) -> Result<Vec<Notice>, PortError> {
    let mut conn = db.pool().acquire().await.map_err(error)?;
    Ok(delivery::list(&mut conn)
        .await
        .map_err(error)?
        .into_iter()
        .map(|d| {
            let n = d.notification;
            let (server_id, vault_id, cause) = scope(&n.dedupe_key);
            Notice {
                server_id: server_id.map(|server| server.to_string()),
                vault_id: vault_id.map(|vault| vault.to_string()),
                cause: cause.unwrap_or(n.code.as_str()).into(),
                code: n.code.to_string(),
                state: n.state.as_text().into(),
                generation: d.generation.to_string(),
                critical: n.code.class().is_severe(),
                claimed: d.claimed,
                key: n.dedupe_key,
            }
        })
        .collect())
}
pub(crate) async fn claim(
    db: &Database,
    key: &str,
    generation: i64,
) -> Result<ManagementResult, PortError> {
    let mut tx = db.begin_write().await.map_err(error)?;
    let critical = notifications::notification(&mut tx, key)
        .await
        .map_err(error)?
        .is_some_and(|n| n.code.class().is_severe());
    let claimed = critical
        && delivery::claim(&mut tx, key, generation)
            .await
            .map_err(error)?;
    tx.commit().await.map_err(error)?;
    Ok(ManagementResult {
        notification_claimed: Some(claimed),
        ..Default::default()
    })
}

// Historical orphan records can remain vault-scoped. Their delivery claim and lifecycle remain
// readable, while application writers always supply the complete remote identity.
fn scope(key: &str) -> (Option<ServerId>, Option<VaultId>, Option<&str>) {
    let pieces: Vec<_> = key.split(':').collect();
    match pieces.as_slice() {
        ["server", server, "vault", vault, cause] => {
            let server_id = ServerId::from_hex(server).ok();
            let vault_id = VaultId::from_hex(vault).ok();
            if server_id.is_some_and(|id| id.to_string() == *server)
                && vault_id.is_some_and(|id| id.to_string() == *vault)
            {
                (server_id, vault_id, Some(cause))
            } else {
                (None, None, None)
            }
        }
        ["vault", vault, cause] => {
            let vault_id = VaultId::from_hex(vault)
                .ok()
                .filter(|id| id.to_string() == *vault);
            (None, vault_id, vault_id.map(|_| *cause))
        }
        _ => (None, None, None),
    }
}
