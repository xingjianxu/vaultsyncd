//! Rebuild admission before restoring uploads, dispatching pending commands or local discovery.
//! A durable old-device fence is resumed even after process restart. No new first-import is used
//! to reinterpret old clean files, and a new epoch never reaches an old UploadActionHandler.
use crate::{local::error, remote::Remote, work::Worker};
use std::sync::Arc;
use vaultsyncd_agent_core::{
    engine::events::ChangeSource,
    recovery::resync::{Resync, ResyncReason},
};
use vaultsyncd_client_store::{bindings, db::Database};
use vaultsyncd_domain::ids::BindingId;
use vaultsyncd_http_client::{ApiRequest, AuthMode};
use vaultsyncd_protocol::generated::Capabilities;
use vaultsyncd_transfer::ports::PortError;

/// true means metadata/epoch was atomically rebuilt; discard this live assembler and reconnect.
/// An incomplete review is a persistent NEEDS_RECOVERY, never success or a retryable old command.
pub(crate) async fn admit(
    database: Arc<Database>,
    binding: BindingId,
    worker: &Worker,
    remote: Arc<Remote>,
) -> Result<bool, PortError> {
    let resync = Resync::new(
        database.clone(),
        worker.importer.clone(),
        worker.importer.local.clone(),
        remote.clone(),
    );
    if let Some(report) = resync.resume_pending(binding).await? {
        return finish(report);
    }
    let mut connection = database.pool().acquire().await.map_err(error)?;
    let bound = bindings::binding(&mut connection, binding)
        .await
        .map_err(error)?
        .ok_or_else(|| error("missing binding"))?;
    let has_baseline: i64 =
        sqlx::query_scalar("SELECT count(*) FROM baseline_entries WHERE binding_id=?")
            .bind(binding.as_bytes().as_slice())
            .fetch_one(&mut *connection)
            .await
            .map_err(error)?;
    let received: i64 = sqlx::query_scalar("SELECT received_seq FROM bindings WHERE id=?")
        .bind(binding.as_bytes().as_slice())
        .fetch_one(&mut *connection)
        .await
        .map_err(error)?;
    drop(connection);
    if has_baseline == 0 {
        return Ok(false);
    }
    let capabilities: Capabilities = remote
        .client
        .send_json(ApiRequest::get("/v1/capabilities").with_auth(AuthMode::Bearer))
        .await
        .map_err(vaultsyncd_http_client::transfer_error)?;
    let epoch = vaultsyncd_domain::version::ServerEpoch::from_decimal(&capabilities.epoch)
        .map_err(error)?;
    let reason = if epoch != bound.server_epoch {
        Some(ResyncReason::ServerRollback {
            new_epoch: epoch.get(),
        })
    } else {
        match remote
            .pull(
                bound.root,
                vaultsyncd_domain::version::RootSeq::new(received as u64).map_err(error)?,
                1,
            )
            .await
        {
            Ok(page) => {
                if page.server_epoch != bound.server_epoch.to_string() {
                    return Err(PortError::rejected(
                        "NEEDS_RECOVERY",
                        "Root and server epoch disagree",
                    ));
                }
                None
            }
            Err(e) if e.code() == Some("CURSOR_EXPIRED") => Some(ResyncReason::CursorExpired),
            Err(e) => return Err(e),
        }
    };
    match reason {
        None => Ok(false),
        Some(reason) => finish(resync.run(binding, reason).await?),
    }
}
fn finish(
    report: vaultsyncd_agent_core::recovery::resync::ResyncReport,
) -> Result<bool, PortError> {
    eprintln!(
        "event=old-device-rebuild resync={} prepared={} complete={} reason={:?}",
        report.resync_id, report.prepared, report.complete, report.reason
    );
    if report.complete {
        Ok(true)
    } else {
        Err(PortError::rejected(
            "NEEDS_RECOVERY",
            "old-device rebuild retained baseline, local bytes and quarantined operations; review persistent recovery candidates",
        ))
    }
}
