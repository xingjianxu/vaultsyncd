//! 授权的文件活动查询与传输事实记录。
use crate::{
    auth::AuthContext,
    uploads::{Caller, bad_request, internal_error},
};
use axum::{
    Json,
    extract::{Path, RawQuery, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use vaultsyncd_domain::ids::{UploadId, VaultId};
use vaultsyncd_protocol::generated::{ActivityPage, ActivityRecord};
use vaultsyncd_server_store::{activity as store, db::Database};

/// 50条倒序键集分页，只接受规范游标与方向。
pub async fn list(
    State(context): State<Arc<AuthContext>>,
    Path(vault): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let (caller, vault) = match Caller::resolve(&context, &headers, &vault).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let (before, direction) = match parse_query(query.as_deref()) {
        Ok(value) => value,
        Err(()) => return bad_request(&caller.request_id, "无效的记录分页游标或方向"),
    };
    let mut conn = match context.database().pool().acquire().await {
        Ok(value) => value,
        Err(_) => {
            return internal_error(
                &caller.request_id,
                "读取文件记录失败",
                Some(vault.to_string()),
            );
        }
    };
    let mut rows = match store::list(
        &mut conn,
        caller.principal.user_id,
        vault,
        before,
        direction,
    )
    .await
    {
        Ok(rows) => rows,
        Err(_) => {
            return internal_error(
                &caller.request_id,
                "读取文件记录失败",
                Some(vault.to_string()),
            );
        }
    };
    let more = rows.len() > 50;
    rows.truncate(50);
    let next_cursor = more.then(|| rows.last().expect("非空记录页").id.to_string());
    let items = rows
        .into_iter()
        .map(|row| ActivityRecord {
            id: row.id.to_string(),
            occurred_at: row.occurred_at.to_string(),
            client_id: row.client_id,
            client_label: row.client_label,
            client_hostname: row.client_hostname,
            client_os: row.client_os,
            client_ip: row.client_ip,
            client_kind: row.client_kind,
            device_id: row.device_id,
            direction: row.direction,
            action: row.action,
            status: row.status,
            entry_id: row.entry_id,
            version_id: row.version_id,
            path: row.path,
            size: row.size.map(|n| n.to_string()),
            bytes: row.bytes.map(|n| n.to_string()),
            offset: row.offset.map(|n| n.to_string()),
            operation_id: row.operation_id,
            upload_id: row.upload_id,
            error_code: row.error_code,
        })
        .collect();
    let mut response = Json(ActivityPage { items, next_cursor }).into_response();
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().expect("静态头"));
    response
}

fn parse_query(query: Option<&str>) -> Result<(Option<i64>, Option<&str>), ()> {
    let mut before = None;
    let mut direction = None;
    for pair in query
        .filter(|v| !v.is_empty())
        .into_iter()
        .flat_map(|v| v.split('&'))
    {
        let (key, value) = pair.split_once('=').ok_or(())?;
        match key {
            "before"
                if before.is_none()
                    && !value.starts_with('0')
                    && !value.is_empty()
                    && value.bytes().all(|c| c.is_ascii_digit()) =>
            {
                before = Some(value.parse::<i64>().ok().filter(|v| *v > 0).ok_or(())?)
            }
            "direction" if direction.is_none() && matches!(value, "upload" | "download") => {
                direction = Some(value)
            }
            _ => return Err(()),
        }
    }
    Ok((before, direction))
}

/// 单个已授权内容请求的观测事实。失败时向调用者返回存储故障，不伪造已记账。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn transfer(
    database: &Database,
    caller: &Caller,
    vault: VaultId,
    action: &str,
    status: &str,
    entry: Option<&str>,
    version: Option<&str>,
    size: Option<u64>,
    bytes: Option<u64>,
    offset: Option<u64>,
    upload: Option<UploadId>,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    let mut conn = database.pool().acquire().await?;
    let client = store::client(
        &mut conn,
        caller.principal.user_id,
        caller.principal.session_id,
    )
    .await?;
    let path = match entry {
        Some(entry) => store::path(&mut conn, vault, entry).await?,
        None => None,
    };
    store::record(
        &mut conn,
        &store::Transfer {
            vault,
            at: crate::uploads::server_now(),
            client: &client,
            action,
            status,
            entry,
            version,
            path: path.as_deref(),
            size,
            bytes,
            offset,
            upload,
            error,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_is_strict_and_bounded() {
        assert_eq!(parse_query(None), Ok((None, None)));
        assert_eq!(
            parse_query(Some("direction=download&before=55")),
            Ok((Some(55), Some("download")))
        );
        for query in [
            "before=0",
            "before=01",
            "before=-1",
            "before=+1",
            "before=9223372036854775808",
            "before=1&before=2",
            "direction=other",
            "direction=upload&direction=download",
            "unknown=1",
            "before=%31",
            "before=",
        ] {
            assert!(parse_query(Some(query)).is_err(), "{query}");
        }
    }
}
