//! 当前账号的只读连接列表；认证/存储失败不伪装为空列表。
use super::*;
use axum::extract::RawQuery;
use vaultsyncd_domain::ids::SessionId;
use vaultsyncd_protocol::generated::{
    ClientConnection, ClientConnectionClientKind, ClientConnectionPage, ClientConnectionState,
};
use vaultsyncd_server_store::clients as store;

/// 列出请求账号的连接观测记录。
pub async fn list(
    State(context): State<Arc<AuthContext>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let principal = match principal_from(&context, &headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(now) = server_now() else {
        return internal_error("系统时钟不可用", "clients");
    };
    let mut conn = match context.database.pool().acquire().await {
        Ok(conn) => conn,
        Err(_) => return internal_error("读取客户端记录失败", "clients"),
    };
    let cursor = match query.as_deref() {
        None | Some("") => None,
        Some(raw) => match raw.strip_prefix("after=") {
            Some(value)
                if value.len() == 32
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
            {
                Some(value)
            }
            _ => return invalid_cursor(),
        },
    };
    let after = match cursor {
        Some(text) => match SessionId::from_hex(text) {
            Ok(id) => match store::cursor_exists(&mut conn, principal.user_id, id).await {
                Ok(true) => Some(id),
                Ok(false) => return invalid_cursor(),
                Err(_) => return internal_error("读取客户端记录失败", "clients"),
            },
            Err(_) => return invalid_cursor(),
        },
        None => None,
    };
    let mut rows =
        match store::list(&mut conn, principal.user_id, principal.session_id, after).await {
            Ok(rows) => rows,
            Err(_) => return internal_error("读取客户端记录失败", "clients"),
        };
    let more = rows.len() > store::PAGE_SIZE;
    rows.truncate(store::PAGE_SIZE);
    let next_cursor = more.then(|| rows.last().expect("非空分页").id.clone());
    let connections = rows
        .into_iter()
        .map(|row| summarize(row, now.as_unix_seconds()))
        .collect();
    let mut response = Json(ClientConnectionPage {
        connections,
        observed_at: now.as_unix_seconds().to_string(),
        next_cursor,
    })
    .into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn summarize(row: store::ClientConnection, now: i64) -> ClientConnection {
    let state = if row.revoked_at.is_some() {
        ClientConnectionState::Revoked
    } else if row.refresh_expires_at.is_none() {
        ClientConnectionState::Unknown
    } else if row.refresh_expires_at.is_some_and(|at| at <= now) {
        ClientConnectionState::Expired
    } else if row
        .last_seen_at
        .is_some_and(|at| at <= now && at >= now.saturating_sub(300))
    {
        ClientConnectionState::Recent
    } else {
        ClientConnectionState::Inactive
    };
    ClientConnection {
        connection_id: row.id,
        device_label: row.device_label,
        client_kind: match row.client_kind.as_str() {
            "browser" => ClientConnectionClientKind::Browser,
            "client" => ClientConnectionClientKind::Client,
            _ => ClientConnectionClientKind::Unknown,
        },
        login_ip: row.login_ip,
        created_at: row.created_at.to_string(),
        last_seen_at: row.last_seen_at.map(|at| at.to_string()),
        refresh_expires_at: row.refresh_expires_at.map(|at| at.to_string()),
        revoked_at: row.revoked_at.map(|at| at.to_string()),
        state,
        current: row.current,
    }
}

fn invalid_cursor() -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        ErrorCode::InvalidRequest,
        &next_request_id(),
        "客户端分页游标无效，请刷新列表",
        false,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(
        seen: Option<i64>,
        expiry: Option<i64>,
        revoked: Option<i64>,
    ) -> store::ClientConnection {
        store::ClientConnection {
            id: "11".repeat(16),
            device_label: "设备".into(),
            client_kind: "unknown".into(),
            login_ip: None,
            created_at: 100,
            last_seen_at: seen,
            refresh_expires_at: expiry,
            revoked_at: revoked,
            current: false,
        }
    }
    #[test]
    fn states_respect_expiry_revocation_activity_window_and_clock_rollback() {
        assert_eq!(
            summarize(record(Some(701), Some(2000), None), 1000).state,
            ClientConnectionState::Recent
        );
        assert_eq!(
            summarize(record(Some(700), Some(2000), None), 1000).state,
            ClientConnectionState::Recent
        );
        for seen in [None, Some(699), Some(1001)] {
            assert_eq!(
                summarize(record(seen, Some(2000), None), 1000).state,
                ClientConnectionState::Inactive
            );
        }
        assert_eq!(
            summarize(record(Some(999), Some(1000), None), 1000).state,
            ClientConnectionState::Expired
        );
        assert_eq!(
            summarize(record(Some(999), Some(1000), Some(998)), 1000).state,
            ClientConnectionState::Revoked
        );
        assert_eq!(
            summarize(record(Some(999), None, None), 1000).state,
            ClientConnectionState::Unknown
        );
    }
}
