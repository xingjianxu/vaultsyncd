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
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tracing::warn;
use vaultsyncd_protocol::generated::{
    ErrorBody, ErrorCode, ErrorEnvelope, HealthResponse, HealthStatus,
};

use crate::lifecycle::{Lifecycle, Phase};

/// 健康探测与其他无需认证端点的 router。
pub fn router(lifecycle: Arc<Lifecycle>) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .with_state(lifecycle)
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

    fn service(lifecycle: Arc<Lifecycle>) -> Router {
        router(lifecycle)
    }

    async fn get_health(lifecycle: Arc<Lifecycle>) -> (StatusCode, String) {
        let response = service(lifecycle)
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
