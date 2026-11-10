//! Read the selected binding's authorized Vault activity through the authenticated client.
use crate::{config::AgentConfig, local::error};
use std::sync::Arc;
use vaultsyncd_client_store::{bindings, db::Database};
use vaultsyncd_domain::ids::BindingId;
use vaultsyncd_http_client::{ApiClient, ApiRequest, AuthMode, ReqwestTransport};
use vaultsyncd_local_ipc::generated::{ActivityRecord, Command, ManagementResult};
use vaultsyncd_protocol::generated::ActivityPage;
use vaultsyncd_transfer::ports::PortError;

pub(crate) async fn execute(
    db: &Arc<Database>,
    config: &AgentConfig,
    client: &ApiClient<ReqwestTransport>,
    command: &Command,
) -> Result<ManagementResult, PortError> {
    let parameters = command
        .parameters
        .as_ref()
        .ok_or_else(|| PortError::rejected("INVALID_ARGUMENT", "missing parameters"))?;
    let binding = BindingId::from_hex(
        parameters
            .binding_id
            .as_deref()
            .ok_or_else(|| PortError::rejected("INVALID_ARGUMENT", "missing binding"))?,
    )
    .map_err(error)?;
    let mut connection = db.pool().acquire().await.map_err(error)?;
    let record = bindings::binding(&mut connection, binding)
        .await
        .map_err(error)?
        .ok_or_else(|| PortError::rejected("NOT_FOUND", "binding missing"))?;
    drop(connection);
    if record.server.to_string() != config.server.id
        || record.device.to_string() != config.device_id
    {
        return Err(PortError::rejected(
            "BINDING_BLOCKED",
            "binding identity differs",
        ));
    }
    let mut request = ApiRequest::get(format!("/v1/vaults/{}/activity", record.vault))
        .with_auth(AuthMode::Bearer);
    for (name, value) in [
        ("before", parameters.cursor.as_ref()),
        ("direction", parameters.direction.as_ref()),
    ] {
        if let Some(value) = value {
            request = request
                .with_query_param(name, value)
                .map_err(vaultsyncd_http_client::transfer_error)?;
        }
    }
    let page: ActivityPage = client
        .send_json(request)
        .await
        .map_err(vaultsyncd_http_client::transfer_error)?;
    Ok(ManagementResult {
        activity: Some(
            page.items
                .into_iter()
                .map(|item| ActivityRecord {
                    id: item.id,
                    occurred_at: item.occurred_at,
                    client_label: item.client_label,
                    client_kind: item.client_kind,
                    direction: item.direction,
                    action: item.action,
                    status: item.status,
                    client_id: item.client_id,
                    device_id: item.device_id,
                    entry_id: item.entry_id,
                    version_id: item.version_id,
                    path: item.path,
                    size: item.size,
                    bytes: item.bytes,
                    offset: item.offset,
                    operation_id: item.operation_id,
                    upload_id: item.upload_id,
                    error_code: item.error_code,
                })
                .collect(),
        ),
        next_cursor: page.next_cursor,
        ..Default::default()
    })
}
