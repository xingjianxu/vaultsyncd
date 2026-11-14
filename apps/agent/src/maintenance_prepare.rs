//! Quiesce admission and preserve every unconfirmed binding before a maintenance shutdown.
use crate::{
    config::AgentConfig,
    local::{Local, error},
    management::Running,
};
use std::{path::Path, sync::Arc};
use vaultsyncd_agent_core::bindings::control::BindingControls;
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::ids::BindingId;
use vaultsyncd_local_ipc::generated::ManagementResult;
use vaultsyncd_platform_windows::cfapi::adapter::Callbacks;
use vaultsyncd_transfer::ports::PortError;

pub(crate) async fn prepare(
    db: &Arc<Database>,
    config: &AgentConfig,
    running: Running,
    destination: &str,
) -> Result<ManagementResult, PortError> {
    let bindings = crate::management::binding_list(db).await?;
    pause_bindings(db).await?;
    // No further hydration is admitted; do not unregister, release caches or clear queues.
    for (worker, _) in running.values() {
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
            .wait_idle(std::time::Duration::from_secs(5))
            .await
            .map_err(error)?;
    }
    let mut count = 0;
    for binding in &bindings {
        let id = BindingId::from_hex(&binding.binding_id).map_err(error)?;
        let local = if let Some((worker, _)) = running.get(&id) {
            worker.importer.local.clone()
        } else {
            Arc::new(
                Local::open(
                    Path::new(&binding.local_path),
                    Path::new(&binding.cache_path),
                    db.clone(),
                    id,
                )
                .await?,
            )
        };
        let path = Path::new(destination).join(&binding.binding_id);
        count += crate::management_export::export(
            local,
            config,
            path.to_str().ok_or_else(|| error("invalid export path"))?,
        )
        .await?;
    }
    Ok(ManagementResult {
        exported_files: Some(count.to_string()),
        ..Default::default()
    })
}

/// Persist the existing stop policy without scanning or copying user content.
pub(crate) async fn pause_bindings(db: &Arc<Database>) -> Result<(), PortError> {
    let bindings = crate::management::binding_list(db).await?;
    let control = BindingControls::new(db.clone());
    for binding in bindings.iter().filter(|binding| !binding.detached) {
        control
            .set_paused(
                BindingId::from_hex(&binding.binding_id).map_err(error)?,
                true,
            )
            .await?;
    }
    Ok(())
}
