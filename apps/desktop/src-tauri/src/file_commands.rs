use crate::commands::{BridgeError, manage};
use vaultsyncd_local_ipc::generated::{CommandName as C, CommandParameters as P, ManagementResult};
#[tauri::command]
pub(crate) async fn agent_list_files(
    binding_id: String,
    cursor: Option<String>,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::ListFiles,
        Some(P {
            binding_id: Some(binding_id),
            cursor,
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_download_entry(
    binding_id: String,
    entry_id: String,
    operation_id: String,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::DownloadEntry,
        Some(P {
            binding_id: Some(binding_id),
            entry_id: Some(entry_id),
            operation_id: Some(operation_id),
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_release_entry(
    binding_id: String,
    entry_id: String,
    operation_id: String,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::ReleaseEntry,
        Some(P {
            binding_id: Some(binding_id),
            entry_id: Some(entry_id),
            operation_id: Some(operation_id),
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_list_problems(
    binding_id: Option<String>,
    search: Option<String>,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::ListProblems,
        Some(P {
            binding_id,
            search,
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_retry_problem(
    binding_id: String,
    operation_id: String,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::RetryProblem,
        Some(P {
            binding_id: Some(binding_id),
            operation_id: Some(operation_id),
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_set_mode(
    binding_id: String,
    mode: String,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::SetMode,
        Some(P {
            binding_id: Some(binding_id),
            mode: Some(mode),
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_detach_binding(
    binding_id: String,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::DetachBinding,
        Some(P {
            binding_id: Some(binding_id),
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_export_recovery(
    binding_id: String,
    destination: String,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::ExportRecovery,
        Some(P {
            binding_id: Some(binding_id),
            destination: Some(destination),
            ..Default::default()
        }),
    )
    .await
}
