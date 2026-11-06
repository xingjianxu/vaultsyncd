//! Real HTTP history/trash adapters. Restore uses server CAS/idempotency, never writes local bytes.
use crate::{config::AgentConfig, local::error};
use std::sync::Arc;
use vaultsyncd_client_store::{bindings, db::Database};
use vaultsyncd_domain::ids::{BindingId, OpId};
use vaultsyncd_http_client::{ApiClient, ApiRequest, AuthMode, ReqwestTransport};
use vaultsyncd_local_ipc::generated::{Command, CommandName as C, ManagementResult, RemoteItem};
use vaultsyncd_protocol::generated::{
    OperationKind, OperationRequest, OperationState, RecoveryPage,
};
use vaultsyncd_transfer::ports::PortError;

pub(crate) async fn execute(
    db: &Arc<Database>,
    config: &AgentConfig,
    client: &ApiClient<ReqwestTransport>,
    command: &Command,
) -> Result<ManagementResult, PortError> {
    let p = command
        .parameters
        .as_ref()
        .ok_or_else(|| error("missing parameters"))?;
    let binding = BindingId::from_hex(p.binding_id.as_deref().unwrap()).map_err(error)?;
    let mut conn = db.pool().acquire().await.map_err(error)?;
    let record = bindings::binding(&mut conn, binding)
        .await
        .map_err(error)?
        .ok_or_else(|| PortError::rejected("NOT_FOUND", "binding missing"))?;
    drop(conn);
    let mut result = ManagementResult::default();
    if matches!(command.name, C::ListHistory | C::ListTrash) {
        let path = if command.name == C::ListHistory {
            format!(
                "/v1/roots/{}/entries/{}/history",
                record.root,
                p.entry_id.as_deref().unwrap()
            )
        } else {
            format!("/v1/roots/{}/trash", record.root)
        };
        let mut request = ApiRequest::get(path).with_auth(AuthMode::Bearer);
        if let Some(cursor) = &p.cursor {
            request = request
                .with_query_param("after", cursor)
                .map_err(vaultsyncd_http_client::transfer_error)?;
        }
        let page: RecoveryPage = client
            .send_json(request)
            .await
            .map_err(vaultsyncd_http_client::transfer_error)?;
        result.items = Some(
            page.items
                .into_iter()
                .map(|i| RemoteItem {
                    id: i.id,
                    name: i.name,
                    entry_id: Some(i.entry_id),
                    version_id: i.version_id,
                    revision: Some(i.revision),
                    parent_id: i.parent_id,
                    size: i.size,
                    hash: i.hash,
                })
                .collect(),
        );
        result.next_cursor = page.next_cursor;
    } else {
        let op = OpId::from_hex(p.operation_id.as_deref().unwrap()).map_err(error)?;
        let request = OperationRequest {
            device_id: config.device_id.clone(),
            op_id: op.to_string(),
            server_epoch: record.server_epoch.to_string(),
            kind: OperationKind::Restore,
            entry_id: p.entry_id.clone(),
            base_revision: p.base_revision.clone(),
            version_id: p.version_id.clone(),
            name: None,
            parent_id: None,
            base_version_id: None,
            conflict_policy: None,
            expected_absent: None,
            local_generation: None,
            subtree_token: None,
            upload_id: None,
        };
        // Persist exact immutable inputs before HTTP; replay never silently changes a target/version.
        let text = serde_json::to_string(
            &serde_json::json!({"kind":"management_restore","request":request}),
        )
        .map_err(error)?;
        let mut tx = db.begin_write().await.map_err(error)?;
        let old: Option<(Vec<u8>, String)> =
            sqlx::query_as("SELECT binding_id,specification FROM agent_actions WHERE op_id=?")
                .bind(op.as_bytes().as_slice())
                .fetch_optional(&mut *tx)
                .await
                .map_err(error)?;
        if old
            .as_ref()
            .is_some_and(|(b, s)| b != binding.as_bytes() || s != &text)
        {
            return Err(PortError::rejected(
                "OP_ID_REUSED",
                "restore inputs changed",
            ));
        }
        sqlx::query(
            "INSERT OR IGNORE INTO agent_actions(op_id,binding_id,specification) VALUES(?,?,?)",
        )
        .bind(op.as_bytes().as_slice())
        .bind(binding.as_bytes().as_slice())
        .bind(text)
        .execute(&mut *tx)
        .await
        .map_err(error)?;
        tx.commit().await.map_err(error)?;
        return retry_restore(db, binding, op, client).await;
    }
    Ok(result)
}

pub(crate) async fn retry_restore(
    db: &Arc<Database>,
    binding: BindingId,
    op: OpId,
    client: &ApiClient<ReqwestTransport>,
) -> Result<ManagementResult, PortError> {
    let text:Option<String>=sqlx::query_scalar("SELECT specification FROM agent_actions WHERE op_id=? AND binding_id=? AND json_extract(specification,'$.kind')='management_restore'")
        .bind(op.as_bytes().as_slice()).bind(binding.as_bytes().as_slice()).fetch_optional(db.pool()).await.map_err(error)?;
    let text =
        text.ok_or_else(|| PortError::rejected("NOT_FOUND", "management operation missing"))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(error)?;
    let request: OperationRequest =
        serde_json::from_value(value["request"].clone()).map_err(error)?;
    let root: Vec<u8> = sqlx::query_scalar("SELECT root_id FROM bindings WHERE id=?")
        .bind(binding.as_bytes().as_slice())
        .fetch_one(db.pool())
        .await
        .map_err(error)?;
    let root = vaultsyncd_domain::ids::RootId::from_bytes(
        root.try_into()
            .map_err(|_| error("invalid root identity"))?,
    );
    let response = vaultsyncd_http_client::sync::submit(client, &root.to_string(), &request)
        .await
        .map_err(|e| match e {
            vaultsyncd_http_client::ClientError::Api(failure) => PortError::rejected(
                serde_json::to_value(failure.body().code)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "UNAVAILABLE".into()),
                "restore rejected by server",
            ),
            other => vaultsyncd_http_client::transfer_error(other),
        })?;
    if response.state != OperationState::Committed {
        return Err(PortError::rejected(
            "PENDING",
            "restore is not committed; retry same operation identity",
        ));
    }
    sqlx::query("UPDATE agent_actions SET result=? WHERE op_id=? AND binding_id=?")
        .bind(serde_json::to_string(&response).map_err(error)?)
        .bind(op.as_bytes().as_slice())
        .bind(binding.as_bytes().as_slice())
        .execute(db.pool())
        .await
        .map_err(error)?;
    Ok(ManagementResult {
        operation_id: Some(op.to_string()),
        ..Default::default()
    })
}
