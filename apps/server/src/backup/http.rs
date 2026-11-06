//! owner备份API；wire DTO唯一源为OpenAPI，manifest保持独立artifact schema。
use super::jobs::Context;
use crate::{
    auth::AuthContext,
    lifecycle::{Lifecycle, Phase},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, RawQuery, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{io, sync::Arc};
use vaultsyncd_domain::ids::{BackupLeaseId, UserId};
use vaultsyncd_protocol::generated::{BackupPrepareRequest, ErrorCode};
#[derive(Clone)]
struct HttpState {
    auth: Arc<AuthContext>,
    context: Arc<Context>,
    lifecycle: Arc<Lifecycle>,
}
/// 与基础路由合并，不改变现有消费者的router参数。
pub fn router(auth: Arc<AuthContext>, context: Arc<Context>, lifecycle: Arc<Lifecycle>) -> Router {
    Router::new()
        .route("/v1/admin/backup-snapshots", get(list).post(prepare))
        .route("/v1/admin/backup-snapshots/{id}", get(status))
        .route("/v1/admin/backup-snapshots/{id}/manifest", get(manifest))
        .route("/v1/admin/backup-snapshots/{id}/export", get(export))
        .route("/v1/admin/backup-snapshots/{id}/files/{name}", get(file))
        .route(
            "/v1/admin/backup-snapshots/{id}/objects/{root}/{hash}",
            get(object),
        )
        .route("/v1/admin/backup-snapshots/{id}/verify", post(verify))
        .route("/v1/admin/backup-snapshots/{id}/release", post(release))
        .layer(DefaultBodyLimit::max(8192))
        .layer(axum::middleware::map_response(
            |mut r: Response| async move {
                r.headers_mut()
                    .insert("cache-control", "no-store".parse().expect("static header"));
                r
            },
        ))
        .with_state(HttpState {
            auth,
            context,
            lifecycle,
        })
}
fn failure(error: io::Error) -> Response {
    let (status, code) = match error.kind() {
        io::ErrorKind::PermissionDenied => (StatusCode::FORBIDDEN, ErrorCode::AccessDenied),
        io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, ErrorCode::UnsupportedEntry),
        io::ErrorKind::InvalidInput => (StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest),
        io::ErrorKind::InvalidData => (StatusCode::UNPROCESSABLE_ENTITY, ErrorCode::HashMismatch),
        _ => (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::NeedsRecovery),
    };
    crate::uploads::error_response(
        status,
        code,
        "backup",
        &error.to_string(),
        status == StatusCode::SERVICE_UNAVAILABLE,
        None,
    )
}
fn id(text: &str) -> Result<BackupLeaseId, Response> {
    BackupLeaseId::from_hex(text)
        .ok()
        .filter(|id| id.to_hex() == text)
        .ok_or_else(|| {
            failure(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid backup identity",
            ))
        })
}
async fn owner(state: &HttpState, headers: &HeaderMap) -> Result<UserId, Response> {
    let p = crate::auth::principal_from(&state.auth, headers).await?;
    state.context.authorize(p.user_id).await.map_err(failure)?;
    Ok(p.user_id)
}
async fn prepare(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Result<Json<BackupPrepareRequest>, JsonRejection>,
) -> Response {
    let owner = match owner(&state, &headers).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    if state.lifecycle.phase() != Phase::Ready {
        return failure(io::Error::other("service is not ready"));
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => {
            return failure(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid prepare request",
            ));
        }
    };
    let id = match id(&body.backup_id) {
        Ok(i) => i,
        Err(r) => return r,
    };
    match state.context.submit(owner, id, "prepare").await {
        Ok(s) => (StatusCode::ACCEPTED, Json(s)).into_response(),
        Err(e) => failure(e),
    }
}
async fn list(
    State(state): State<HttpState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let owner = match owner(&state, &headers).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let query = query.unwrap_or_default();
    let page = if query.is_empty() {
        0
    } else {
        match query
            .strip_prefix("page=")
            .and_then(|s| s.parse::<u32>().ok().filter(|n| n.to_string() == s))
        {
            Some(p) => p,
            None => {
                return failure(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid backup page",
                ));
            }
        }
    };
    match state.context.list(owner, page).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => failure(e),
    }
}
async fn status(
    State(state): State<HttpState>,
    Path(text): Path<String>,
    headers: HeaderMap,
) -> Response {
    let owner = match owner(&state, &headers).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let id = match id(&text) {
        Ok(i) => i,
        Err(r) => return r,
    };
    match state.context.status(owner, id).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => failure(e),
    }
}
async fn operation(state: HttpState, text: String, headers: HeaderMap, kind: &str) -> Response {
    let owner = match owner(&state, &headers).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let id = match id(&text) {
        Ok(i) => i,
        Err(r) => return r,
    };
    match state.context.submit(owner, id, kind).await {
        Ok(s) => (StatusCode::ACCEPTED, Json(s)).into_response(),
        Err(e) => failure(e),
    }
}
async fn verify(
    State(state): State<HttpState>,
    Path(text): Path<String>,
    headers: HeaderMap,
) -> Response {
    operation(state, text, headers, "verify").await
}
async fn release(
    State(state): State<HttpState>,
    Path(text): Path<String>,
    headers: HeaderMap,
) -> Response {
    operation(state, text, headers, "release").await
}
async fn manifest(
    State(state): State<HttpState>,
    Path(text): Path<String>,
    headers: HeaderMap,
) -> Response {
    let owner = match owner(&state, &headers).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let id = match id(&text) {
        Ok(i) => i,
        Err(r) => return r,
    };
    match state.context.manifest(owner, id).await {
        Ok((bytes, hash)) => Response::builder()
            .header("content-type", "application/octet-stream")
            .header(
                "x-manifest-sha256",
                vaultsyncd_server_core::objects::ContentDigest::from_bytes(hash).to_hex(),
            )
            .body(Body::from(bytes))
            .expect("valid response"),
        Err(e) => failure(e),
    }
}
async fn export(
    State(state): State<HttpState>,
    Path(text): Path<String>,
    headers: HeaderMap,
) -> Response {
    let owner = match owner(&state, &headers).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let id = match id(&text) {
        Ok(i) => i,
        Err(r) => return r,
    };
    match state.context.archive(owner, id).await {
        Ok(stream) => Response::builder()
            .header("content-type", "application/octet-stream")
            .header(
                "content-disposition",
                format!("attachment; filename=\"backup-{}.vsbk\"", id.to_hex()),
            )
            .body(Body::from_stream(stream))
            .expect("valid response"),
        Err(e) => failure(e),
    }
}
async fn file(
    State(state): State<HttpState>,
    Path((text, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    download(state, text, name, headers).await
}
async fn object(
    State(state): State<HttpState>,
    Path((text, root, hash)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    download(state, text, format!("objects/{root}/{hash}"), headers).await
}
async fn download(state: HttpState, text: String, path: String, headers: HeaderMap) -> Response {
    let owner = match owner(&state, &headers).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let id = match id(&text) {
        Ok(i) => i,
        Err(r) => return r,
    };
    match state.context.file(owner, id, &path).await {
        Ok(stream) => Response::builder()
            .header("content-type", "application/octet-stream")
            .body(Body::from_stream(stream))
            .expect("valid response"),
        Err(e) => failure(e),
    }
}
