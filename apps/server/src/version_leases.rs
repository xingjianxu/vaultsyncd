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
use vaultsyncd_server_core::objects::ContentDigest;
use vaultsyncd_server_store::version_leases::{self as store, Error, Lease, Request};

/// Create (or safely replay) an exact fixed-version lease.
pub async fn acquire(
    State(c): State<Arc<ContentContext>>,
    Path((vault, entry, version)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<VersionLeaseRequest>,
) -> Response {
    execute(
        c,
        (vault, entry, version),
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
    Path((vault, entry, version, lease)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<VersionLeaseRequest>,
) -> Response {
    execute(
        c,
        (vault, entry, version),
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
    Path((vault, entry, version, lease)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<VersionLeaseRequest>,
) -> Response {
    execute(
        c,
        (vault, entry, version),
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
    (vault, entry, version): (String, String, String),
    lease: Option<String>,
    headers: HeaderMap,
    body: VersionLeaseRequest,
    action: Action,
) -> Response {
    let (caller, vault) = match Caller::resolve(&c.auth, &headers, &vault).await {
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
        vault,
        entry,
        version,
        digest: *digest.as_bytes(),
        size,
    };
    // 布局解析只是本 Vault 的登记读取，放在租约事务之前：解析失败时租约尚未改变，
    // 响应与持久事实一致。对象可读性仍在提交后核对，避免 GC 在检查后删掉未受保护的对象。
    let layout = match crate::object_layout::for_digest(
        c.auth.database(),
        &c.state_dir,
        vault,
        digest,
    )
    .await
    {
        Ok(layout) => layout,
        Err(error) => {
            tracing::warn!(vault_id=%vault, error=%error, "固定版本租约对象布局不可读");
            return internal_error(
                &caller.request_id,
                "固定版本对象登记不可读；不得释放本地副本",
                Some(vault.to_string()),
            );
        }
    };
    let mut tx = match c.auth.database().begin_write().await {
        Ok(tx) => tx,
        Err(_) => {
            return internal_error(
                &caller.request_id,
                "租约事务不可用",
                Some(vault.to_string()),
            );
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
        Err(e) => return failure(&caller.request_id, &vault.to_string(), e),
    };
    if tx.commit().await.is_err() {
        return internal_error(
            &caller.request_id,
            "租约未确认持久化",
            Some(vault.to_string()),
        );
    }
    if !lease.closed {
        // Protection is already persistent before touching the object, closing the GC read/check race.
        // 布局失败已在上方返回；这里只剩真实对象不可读，租约已持久化，客户端应保留副本。
        let path = layout.object_path(&digest);
        let readable = std::fs::File::open(path)
            .and_then(|f| f.metadata())
            .is_ok_and(|m| m.is_file() && m.len() == size);
        if !readable {
            return internal_error(
                &caller.request_id,
                "固定版本内容不可读；不得释放本地副本",
                Some(vault.to_string()),
            );
        }
    }
    (StatusCode::OK, Json(wire(lease, now))).into_response()
}
fn wire(l: Lease, now: i64) -> VersionLease {
    VersionLease {
        lease_id: l.request.id.to_string(),
        vault_id: l.request.vault.to_string(),
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
fn failure(request: &str, vault: &str, e: Error) -> Response {
    if let Error::Storage(error) = &e {
        tracing::warn!(request_id=request, vault_id=vault, error=%error, "固定版本租约存储失败");
        return internal_error(request, "固定版本租约存储不可用", Some(vault.to_owned()));
    }
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
        Some(vault.to_owned()),
    )
}
