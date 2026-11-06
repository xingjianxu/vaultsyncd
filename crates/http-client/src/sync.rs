//! Snapshot and idempotent operation endpoints used by the Windows process assembler.
use crate::{ApiClient, ApiRequest, AuthMode, ClientError, transport::Transport};
use vaultsyncd_protocol::generated::{
    OperationRequest, OperationResult, OperationState, Snapshot, SnapshotPage,
};

fn id(value: &str) -> Result<(), ClientError> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ClientError::invalid_request("noncanonical identity"));
    }
    Ok(())
}
/// Create one immutable snapshot and return its authenticated HTTP Date in Unix seconds.
/// No client clock or inferred expiry timestamp is used as server time.
pub async fn snapshot<T: Transport>(
    client: &ApiClient<T>,
    root: &str,
) -> Result<(Snapshot, u64), ClientError> {
    id(root)?;
    let response = client
        .send(ApiRequest::post(format!("/v1/roots/{root}/snapshots")).with_auth(AuthMode::Bearer))
        .await?;
    let value: Snapshot = response.json()?;
    id(&value.snapshot_id)?;
    if value.root_id != root {
        return Err(ClientError::invalid_request("snapshot root mismatch"));
    }
    let date = response
        .header("date")
        .ok_or_else(|| ClientError::invalid_request("snapshot missing server Date"))?;
    let time = httpdate::parse_http_date(date)
        .map_err(|_| ClientError::invalid_request("malformed server Date"))?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ClientError::invalid_request("server Date before epoch"))?
        .as_secs();
    Ok((value, time))
}
/// Read only the specified fixed snapshot. Expiry errors are propagated without replacement.
pub async fn snapshot_page<T: Transport>(
    client: &ApiClient<T>,
    root: &str,
    snapshot: &str,
    cursor: Option<&str>,
) -> Result<SnapshotPage, ClientError> {
    id(root)?;
    id(snapshot)?;
    let mut request = ApiRequest::get(format!("/v1/roots/{root}/snapshots/{snapshot}/entries"))
        .with_auth(AuthMode::Bearer);
    if let Some(cursor) = cursor {
        vaultsyncd_protocol::decimal::validate(cursor)
            .map_err(|_| ClientError::invalid_request("invalid snapshot cursor"))?;
        request = request.with_query_param("cursor", cursor)?;
    }
    let page: SnapshotPage = client.send_json(request).await?;
    if page.snapshot_id != snapshot {
        return Err(ClientError::invalid_request(
            "snapshot page identity mismatch",
        ));
    }
    Ok(page)
}
/// Submit with the original OpId; caller retains the operation until committed.
pub async fn submit<T: Transport>(
    client: &ApiClient<T>,
    root: &str,
    request: &OperationRequest,
) -> Result<OperationResult, ClientError> {
    id(root)?;
    id(&request.op_id)?;
    let result = client
        .send_json(
            ApiRequest::post(format!("/v1/roots/{root}/operations"))
                .with_auth(AuthMode::Bearer)
                .with_json(request)?,
        )
        .await?;
    validate_result(&result, &request.op_id)?;
    Ok(result)
}
/// Query the same operation after a lost response in the authenticated Root scope.
pub async fn operation<T: Transport>(
    client: &ApiClient<T>,
    root: &str,
    op: &str,
) -> Result<OperationResult, ClientError> {
    id(root)?;
    id(op)?;
    let result = client
        .send_json(
            ApiRequest::get(format!("/v1/roots/{root}/operations/{op}"))
                .with_auth(AuthMode::Bearer),
        )
        .await?;
    validate_result(&result, op)?;
    Ok(result)
}
fn validate_result(value: &OperationResult, op: &str) -> Result<(), ClientError> {
    if value.op_id != op {
        return Err(ClientError::invalid_request("operation identity mismatch"));
    }
    if value.state == OperationState::Pending
        && (value.entry_id.is_some()
            || value.revision.is_some()
            || value.root_seq.is_some()
            || value.version_id.is_some())
    {
        return Err(ClientError::invalid_request(
            "pending contains committed facts",
        ));
    }
    if value.state == OperationState::Committed
        && (value.entry_id.is_none() || value.revision.is_none() || value.root_seq.is_none())
    {
        return Err(ClientError::invalid_request("committed lacks facts"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
