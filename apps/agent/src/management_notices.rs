//! The agent owns problem lifecycle and claims; the GUI cannot mutate a database.
use crate::local::error;
use vaultsyncd_client_store::{
    db::Database,
    notifications::{self, delivery},
};
use vaultsyncd_local_ipc::generated::{ManagementResult, Notice};
use vaultsyncd_transfer::ports::PortError;

pub(crate) async fn list(db: &Database) -> Result<Vec<Notice>, PortError> {
    let mut conn = db.pool().acquire().await.map_err(error)?;
    Ok(delivery::list(&mut conn)
        .await
        .map_err(error)?
        .into_iter()
        .map(|d| {
            let n = d.notification;
            let pieces: Vec<_> = n.dedupe_key.splitn(3, ':').collect();
            Notice {
                root_id: if pieces.len() == 3 && pieces[0] == "root" {
                    Some(pieces[1].into())
                } else {
                    None
                },
                cause: pieces.get(2).copied().unwrap_or(n.code.as_str()).into(),
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
