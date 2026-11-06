//! 现有Capabilities契约的实际入口；身份稳定持久化，不读取/公开服务器路径或认证材料。
use crate::lifecycle::{Lifecycle, Phase};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use std::{io, sync::Arc};
use vaultsyncd_domain::{ids::ServerId, version::ServerEpoch};
use vaultsyncd_protocol::{
    generated::{Capabilities, CapabilityLimits, ErrorCode, ProtocolVersion},
    negotiation,
};
use vaultsyncd_server_store::db::Database;
fn error(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
/// 在持状态锁的bootstrap里运行，必须早于对外监听。
pub async fn ensure_identity(db: &Database) -> io::Result<()> {
    let mut tx = db.begin_write().await.map_err(error)?;
    sqlx::query(
        "INSERT INTO store_metadata(key,value) VALUES('server_id',?) ON CONFLICT(key) DO NOTHING",
    )
    .bind(ServerId::from_bytes(crate::uploads::random_bytes()).to_hex())
    .execute(&mut *tx)
    .await
    .map_err(error)?;
    let id: String = sqlx::query_scalar("SELECT value FROM store_metadata WHERE key='server_id'")
        .fetch_one(&mut *tx)
        .await
        .map_err(error)?;
    if ServerId::from_hex(&id).map_err(error)?.to_hex() != id {
        return Err(error("noncanonical persisted ServerId"));
    }
    let epoch: Option<String> =
        sqlx::query_scalar("SELECT value FROM store_metadata WHERE key='restore_epoch'")
            .fetch_optional(&mut *tx)
            .await
            .map_err(error)?;
    if let Some(epoch) = epoch {
        ServerEpoch::from_decimal(&epoch).map_err(error)?;
    }
    tx.commit().await.map_err(error)
}
#[derive(Clone)]
struct Context {
    db: Database,
    lifecycle: Arc<Lifecycle>,
    max_part: u64,
}
/// 与真实服务路由合并；GET只读取启动已持久化的身份和恢复代际。
pub fn router(db: Database, lifecycle: Arc<Lifecycle>, max_part: u64) -> Router {
    Router::new()
        .route("/v1/capabilities", get(capabilities))
        .with_state(Context {
            db,
            lifecycle,
            max_part,
        })
}
async fn capabilities(State(context): State<Context>) -> Response {
    if context.lifecycle.phase() != Phase::Ready {
        return unavailable("server is not ready");
    }
    let result = async {
        let id: String =
            sqlx::query_scalar("SELECT value FROM store_metadata WHERE key='server_id'")
                .fetch_one(context.db.pool())
                .await
                .map_err(error)?;
        let epoch: Option<String> =
            sqlx::query_scalar("SELECT value FROM store_metadata WHERE key='restore_epoch'")
                .fetch_optional(context.db.pool())
                .await
                .map_err(error)?;
        let value = Capabilities {
            server_id: id,
            epoch: epoch.unwrap_or_else(|| "0".into()),
            protocol: ProtocolVersion {
                major: negotiation::MAJOR,
                minor: negotiation::MINOR,
            },
            features: negotiation::KNOWN_FEATURES
                .iter()
                .map(|f| (*f).to_owned())
                .collect(),
            limits: CapabilityLimits {
                max_page_size: vaultsyncd_server_core::api::snapshots::MAX_SNAPSHOT_PAGE,
                max_upload_part_bytes: context.max_part.to_string(),
            },
        };
        negotiation::negotiate(&value).map_err(error)?;
        Ok::<_, io::Error>(value)
    }
    .await;
    match result {
        Ok(value) => ([("cache-control", "no-store")], Json(value)).into_response(),
        Err(e) => {
            tracing::error!(error=%e,"capabilities identity unavailable");
            unavailable("server identity unavailable")
        }
    }
}
fn unavailable(message: &str) -> Response {
    crate::uploads::error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::NeedsRecovery,
        "capabilities",
        message,
        false,
        None,
    )
}
