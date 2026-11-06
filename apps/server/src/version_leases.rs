//! Authenticated fixed-version leases for safe client dehydration (S069).
use crate::{
    content::ContentContext,
    uploads::{Caller, bad_request, error_response, internal_error, parse_identity},
};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use vaultsyncd_domain::ids::{EntryId, ObjectLeaseId, VersionId};
use vaultsyncd_protocol::generated::{ErrorCode, VersionLease, VersionLeaseRequest};
use vaultsyncd_server_core::objects::{ContentDigest, ObjectLayout};
use vaultsyncd_server_store::version_leases::{self as store, Error, Lease, Request};

/// Create (or safely replay) an exact fixed-version lease.
pub async fn acquire(
    State(c): State<Arc<ContentContext>>,
    Path((root, entry, version)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<VersionLeaseRequest>,
) -> Response {
    execute(
        c,
        (root, entry, version),
        None,
        headers,
        body,
        Action::Acquire,
    )
    .await
}
/// Renew an unexpired exact lease; expiry never silently reacquires it.
pub async fn renew(
    State(c): State<Arc<ContentContext>>,
    Path((root, entry, version, lease)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<VersionLeaseRequest>,
) -> Response {
    execute(
        c,
        (root, entry, version),
        Some(lease),
        headers,
        body,
        Action::Renew,
    )
    .await
}
/// Explicit idempotent release, retaining its immutable request identity.
pub async fn release(
    State(c): State<Arc<ContentContext>>,
    Path((root, entry, version, lease)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<VersionLeaseRequest>,
) -> Response {
    execute(
        c,
        (root, entry, version),
        Some(lease),
        headers,
        body,
        Action::Release,
    )
    .await
}
#[derive(Clone, Copy)]
enum Action {
    Acquire,
    Renew,
    Release,
}
async fn execute(
    c: Arc<ContentContext>,
    (root, entry, version): (String, String, String),
    lease: Option<String>,
    headers: HeaderMap,
    body: VersionLeaseRequest,
    action: Action,
) -> Response {
    let (caller, root) = match Caller::resolve(&c.auth, &headers, &root).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let parsed = (
        parse_identity::<EntryId>(&entry),
        parse_identity::<VersionId>(&version),
        ObjectLeaseId::from_hex(&body.lease_id).ok(),
        ContentDigest::from_hex(&body.sha256),
        body.size.parse::<u64>(),
    );
    let (Some(entry), Some(version), Some(id), Ok(digest), Ok(size)) = parsed else {
        return bad_request(&caller.request_id, "租约固定输入不合法");
    };
    if lease.as_ref().is_some_and(|p| p != &body.lease_id) {
        return bad_request(&caller.request_id, "路径和正文租约ID不一致");
    }
    let r = Request {
        id,
        root,
        entry,
        version,
        digest: *digest.as_bytes(),
        size,
    };
    let mut tx = match c.auth.database().begin_write().await {
        Ok(tx) => tx,
        Err(_) => {
            return internal_error(&caller.request_id, "租约事务不可用", Some(root.to_string()));
        }
    };
    let now = caller.now.to_db_integer();
    let result = match action {
        Action::Acquire => store::acquire(&mut tx, caller.principal.user_id, &r, now).await,
        Action::Renew => store::inspect(&mut tx, caller.principal.user_id, &r, now, true).await,
        Action::Release => store::release(&mut tx, caller.principal.user_id, &r).await,
    };
    let lease = match result {
        Ok(v) => v,
        Err(e) => return failure(&caller.request_id, &root.to_string(), e),
    };
    if tx.commit().await.is_err() {
        return internal_error(
            &caller.request_id,
            "租约未确认持久化",
            Some(root.to_string()),
        );
    }
    if !lease.closed {
        // Protection is already persistent before touching the object, closing the GC read/check race.
        let path = ObjectLayout::new(c.state_dir.join("objects")).object_path(&digest);
        let readable = std::fs::File::open(path)
            .and_then(|f| f.metadata())
            .is_ok_and(|m| m.is_file() && m.len() == size);
        if !readable {
            return internal_error(
                &caller.request_id,
                "固定版本内容不可读；不得释放本地副本",
                Some(root.to_string()),
            );
        }
    }
    (StatusCode::OK, Json(wire(lease, now))).into_response()
}
fn wire(l: Lease, now: i64) -> VersionLease {
    VersionLease {
        lease_id: l.request.id.to_string(),
        root_id: l.request.root.to_string(),
        entry_id: l.request.entry.to_string(),
        version_id: l.request.version.to_string(),
        sha256: ContentDigest::from_bytes(l.request.digest).to_hex(),
        size: l.request.size.to_string(),
        expires_at: l.expires_at.to_string(),
        ttl_seconds: if l.closed {
            0
        } else {
            l.expires_at
                .saturating_sub(now)
                .clamp(0, store::TTL_SECONDS)
        }
        .to_string(),
        closed: l.closed,
    }
}
fn failure(request: &str, root: &str, e: Error) -> Response {
    let (status, code, retry) = match e {
        Error::OutOfScope => (StatusCode::FORBIDDEN, ErrorCode::AccessDenied, false),
        Error::Reused => (StatusCode::CONFLICT, ErrorCode::OpIdReused, false),
        Error::Gone | Error::Expired => (StatusCode::GONE, ErrorCode::VersionGone, false),
        Error::Storage(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::NeedsRecovery,
            true,
        ),
    };
    error_response(
        status,
        code,
        request,
        &e.to_string(),
        retry,
        Some(root.to_owned()),
    )
}
