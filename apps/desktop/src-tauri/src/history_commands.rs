use crate::commands::{BridgeError, manage};
use vaultsyncd_local_ipc::generated::{CommandName as C, CommandParameters as P, ManagementResult};
#[tauri::command]
pub(crate) async fn agent_list_activity(
    binding_id: String,
    cursor: Option<String>,
    direction: Option<String>,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::ListActivity,
        Some(P {
            binding_id: Some(binding_id),
            cursor,
            direction,
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_list_history(
    binding_id: String,
    entry_id: String,
    cursor: Option<String>,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::ListHistory,
        Some(P {
            binding_id: Some(binding_id),
            entry_id: Some(entry_id),
            cursor,
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_list_trash(
    binding_id: String,
    cursor: Option<String>,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::ListTrash,
        Some(P {
            binding_id: Some(binding_id),
            cursor,
            ..Default::default()
        }),
    )
    .await
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RestoreInput {
    binding_id: String,
    entry_id: String,
    operation_id: String,
    base_revision: String,
    version_id: Option<String>,
}
#[tauri::command]
pub(crate) async fn agent_restore_entry(
    input: RestoreInput,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::RestoreEntry,
        Some(P {
            binding_id: Some(input.binding_id),
            entry_id: Some(input.entry_id),
            operation_id: Some(input.operation_id),
            base_revision: Some(input.base_revision),
            version_id: input.version_id,
            ..Default::default()
        }),
    )
    .await
}
