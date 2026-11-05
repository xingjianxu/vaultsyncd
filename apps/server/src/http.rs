//! HTTP 路由：本卡只提供健康探测，readiness 直接来自 [`Lifecycle`]（design §15.1、§15.2）。
//!
//! 端点与 wire 形状取自 `api/openapi.yaml` 的生成 DTO，不手写 JSON。`GET /v1/health` 的语义：
//!
//! | 阶段 | HTTP | 响应体 |
//! | --- | --- | --- |
//! | [`Phase::Ready`] | 200 | `{"status":"ok"}` |
//! | [`Phase::Draining`] | 200 | `{"status":"degraded"}`（读可用，新的写入已被拒绝） |
//! | [`Phase::Starting`] | 503 | 错误信封 `NEEDS_RECOVERY`，`retryable=true`（重启尚未完成恢复） |
//! | [`Phase::NeedsRecovery`] | 503 | 错误信封 `NEEDS_RECOVERY`，`retryable=false`（需要人工核对） |
//!
//! 返回 503 时不用 200 冒充可用：客户端与运维脚本只需看状态码就能判断能否提交
//! （design §15.2 的 503 语义）。业务端点（认证、Root、上传等）由各自功能卡加入同一个 router。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use tracing::warn;
use vaultsyncd_protocol::generated::{
    ErrorBody, ErrorCode, ErrorEnvelope, HealthResponse, HealthStatus,
};

use crate::auth::AuthContext;
use crate::content::ContentContext;
use crate::lifecycle::{Lifecycle, Phase};
use crate::operations::OperationsContext;
use crate::roots::RootContext;
use crate::uploads::UploadsContext;

/// 全部路由：健康探测（无认证）、认证端点（S043）、Root 注册/清单（S044）、上传与内容（S045/S046）
/// 与操作命令（S047）。
///
/// 状态直接就是进程级状态对象，业务端点由后续功能卡加入同一个 router 并从它取数据库与授权检查。
pub fn router(
    lifecycle: Arc<Lifecycle>,
    auth: Arc<AuthContext>,
    uploads: Arc<UploadsContext>,
    roots: Arc<RootContext>,
    content: Arc<ContentContext>,
    operations: Arc<OperationsContext>,
) -> Router {
    let public = Router::new()
        .route("/v1/health", get(health))
        .with_state(lifecycle);
    let authenticated = Router::new()
        .route("/v1/auth/login", post(super::auth::login))
        .route("/v1/auth/refresh", post(super::auth::refresh))
        .route("/v1/auth/logout", post(super::auth::logout))
        .with_state(auth);
    let roots_router = Router::new()
        .route("/v1/roots", get(super::roots::list_roots))
        .route("/v1/admin/roots", post(super::roots::register_root))
        .with_state(roots);
    // 块请求体是二进制原样字节，大小上限由上传策略决定（`max_chunk_size + 1`，给“超一块”留一次
    // 拒绝机会）。Axum 默认请求体上限是 2 MiB，比默认块大小 4 MiB 还小：不在这里显式抬到策略值，
    // 按契约合法的块会在处理器之前就被 413 掉（客户端用默认块大小上传必然失败）。
    let upload_body_limit =
        usize::try_from(uploads.policy().max_chunk_size.saturating_add(1)).unwrap_or(usize::MAX);
    let uploads_router = Router::new()
        .route("/v1/roots/{root}/uploads", post(super::uploads::begin))
        .route(
            "/v1/roots/{root}/uploads/{upload}",
            get(super::uploads::status),
        )
        .route(
            "/v1/roots/{root}/uploads/{upload}/parts/{index}",
            put(super::uploads::put_part),
        )
        .layer(DefaultBodyLimit::max(upload_body_limit))
        .with_state(uploads);
    // finalize 与固定版本下载：只在 `Principal`/`authorize_root` 之后动作，处理器自己解析调用者。
    let content_router = Router::new()
        .route(
            "/v1/roots/{root}/uploads/{upload}/finalize",
            post(super::content::finalize),
        )
        .route(
            "/v1/roots/{root}/entries/{entry}/versions/{version}/content",
            get(super::content::download),
        )
        .with_state(content);
    // 操作命令与结果查询（S047）：幂等键与结果都来自 `operations` 表，`pending` 用 202。
    let operations_router = Router::new()
        .route(
            "/v1/roots/{root}/operations",
            post(super::operations::submit),
        )
        .route(
            "/v1/roots/{root}/operations/{op}",
            get(super::operations::status),
        )
        .with_state(operations);
    public
        .merge(authenticated)
        .merge(roots_router)
        .merge(uploads_router)
        .merge(content_router)
        .merge(operations_router)
}

async fn health(State(lifecycle): State<Arc<Lifecycle>>) -> Response {
    let phase = lifecycle.phase();
    let request_id = next_request_id();
    match phase {
        Phase::Ready => ok_response(HealthStatus::Ok),
        Phase::Draining => ok_response(HealthStatus::Degraded),
        Phase::Starting => {
            let message = "服务端启动恢复尚未完成，写入尚未开放";
            warn!(request_id = %request_id, phase = phase.as_str(), "{message}");
            unavailable(request_id, message, true)
        }
        Phase::NeedsRecovery => {
            let message = match lifecycle.recovery_detail() {
                Some(detail) => format!("服务端需要恢复：{detail}"),
                None => "服务端需要恢复：写入未开放".to_owned(),
            };
            warn!(request_id = %request_id, phase = phase.as_str(), "{message}");
            unavailable(request_id, &message, false)
        }
    }
}

fn ok_response(status: HealthStatus) -> Response {
    (StatusCode::OK, Json(HealthResponse { status })).into_response()
}

fn unavailable(request_id: String, message: &str, retryable: bool) -> Response {
    let envelope = ErrorEnvelope {
        error: ErrorBody {
            code: ErrorCode::NeedsRecovery,
            details: None,
            message: message.to_owned(),
            request_id,
            retryable,
            root_id: None,
        },
    };
    (StatusCode::SERVICE_UNAVAILABLE, Json(envelope)).into_response()
}

/// 进程内的请求标识：没有外部请求头可用时也要在日志与错误信封里给出可关联的编号。
fn next_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("srv-{}-{sequence}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn service(lifecycle: Arc<Lifecycle>) -> Router {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vaultsyncd-s043-http-{}-{sequence}.sqlite",
            std::process::id()
        ));
        let database = crate::auth::test_database(&path).await;
        let auth = Arc::new(crate::auth::AuthContext::new(database));
        let uploads = Arc::new(crate::uploads::UploadsContext::new(
            Arc::clone(&auth),
            &std::env::temp_dir(),
        ));
        let roots = Arc::new(RootContext::new(
            Arc::clone(&auth),
            None,
            std::env::temp_dir(),
            Default::default(),
        ));
        let content = Arc::new(ContentContext::new(
            Arc::clone(&auth),
            &std::env::temp_dir(),
        ));
        let operations = Arc::new(OperationsContext::new(Arc::clone(&auth)));
        router(lifecycle, auth, uploads, roots, content, operations)
    }

    async fn get_health(lifecycle: Arc<Lifecycle>) -> (StatusCode, String) {
        let response = service(lifecycle)
            .await
            .oneshot(
                Request::builder()
                    .uri("/v1/health")
                    .body(Body::empty())
                    .expect("构造请求"),
            )
            .await
            .expect("调用处理器");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("读响应体");
        (
            status,
            String::from_utf8(bytes.to_vec()).expect("响应体是 UTF-8"),
        )
    }

    #[tokio::test]
    async fn ready_reports_ok() {
        let lifecycle = Arc::new(Lifecycle::new());
        lifecycle.mark_ready();
        let (status, body) = get_health(lifecycle).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"status":"ok"}"#);
    }

    #[tokio::test]
    async fn starting_reports_unavailable_and_not_ok() {
        let lifecycle = Arc::new(Lifecycle::new());
        let (status, body) = get_health(lifecycle).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("\"NEEDS_RECOVERY\""), "{body}");
        assert!(body.contains("\"retryable\":true"), "{body}");
        assert!(
            !body.contains("\"status\":\"ok\""),
            "未就绪不得报 ok：{body}"
        );
    }

    #[tokio::test]
    async fn needs_recovery_reports_unavailable_and_not_retryable() {
        let lifecycle = Arc::new(Lifecycle::new());
        lifecycle.mark_needs_recovery("Root 00000000000000000000000000000001 还有未完成意图");
        let (status, body) = get_health(lifecycle).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("\"NEEDS_RECOVERY\""), "{body}");
        assert!(body.contains("\"retryable\":false"), "{body}");
        assert!(body.contains("未完成意图"), "{body}");
    }

    #[tokio::test]
    async fn draining_reports_degraded_without_claiming_writes_are_open() {
        let lifecycle = Arc::new(Lifecycle::new());
        lifecycle.mark_ready();
        lifecycle.begin_shutdown();
        let (status, body) = get_health(lifecycle).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"status":"degraded"}"#);
    }
}
