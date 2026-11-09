//! 传输测试替身：只注入传输端口，不改变产品代码路径。
//!
//! 只在 `#[cfg(test)]` 或 `testing` feature 下编译；产品构建里不存在这个模块。

use std::sync::{Arc, Mutex};

use serde::Serialize;
use vaultsyncd_protocol::generated::{ErrorBody, ErrorCode, ErrorEnvelope, SessionIssued};

use crate::error::TransportError;
use crate::transport::{Transport, TransportFuture, TransportRequest, TransportResponse};

/// 测试替身的处理函数：拿到一次请求，决定回什么（可以异步、可以失败）。
pub type FakeHandler = dyn Fn(TransportRequest) -> TransportFuture<'static, Result<TransportResponse, TransportError>>
    + Send
    + Sync;

/// 可编程传输：按处理函数回应，并记下每次实际发出的请求。
pub struct FakeTransport {
    handler: Arc<FakeHandler>,
    calls: Arc<Mutex<Vec<TransportRequest>>>,
}

impl FakeTransport {
    /// 用处理函数构造。
    pub fn new<F>(handler: F) -> Self
    where
        F: Fn(
                TransportRequest,
            ) -> TransportFuture<'static, Result<TransportResponse, TransportError>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            handler: Arc::new(handler),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 已经发出的请求（按调用顺序）。
    pub fn calls(&self) -> Vec<TransportRequest> {
        self.calls.lock().expect("调用记录锁").clone()
    }

    /// 已经发出的请求数。
    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("调用记录锁").len()
    }
}

impl Transport for FakeTransport {
    fn send(
        &self,
        request: TransportRequest,
    ) -> TransportFuture<'_, Result<TransportResponse, TransportError>> {
        self.calls.lock().expect("调用记录锁").push(request.clone());
        (self.handler)(request)
    }
}

/// 一份 JSON 响应。
pub fn json_response(status: u16, value: &impl Serialize) -> TransportResponse {
    let body = serde_json::to_vec(value).expect("测试数据必须可序列化");
    TransportResponse::new(
        status,
        vec![("content-type".to_owned(), "application/json".to_owned())],
        body,
    )
}

/// 一份错误信封响应（design §15.4）。
pub fn error_response(
    status: u16,
    code: ErrorCode,
    message: &str,
    retryable: bool,
) -> TransportResponse {
    json_response(
        status,
        &ErrorEnvelope {
            error: ErrorBody {
                code,
                details: None,
                message: message.to_owned(),
                request_id: "test-request".to_owned(),
                retryable,
                vault_id: None,
            },
        },
    )
}

/// 一份合法的登录/刷新响应。
pub fn issued(access_token: &str, refresh_token: &str) -> SessionIssued {
    SessionIssued {
        access_expires_at: "1000".to_owned(),
        access_token: access_token.to_owned(),
        refresh_expires_at: "2000".to_owned(),
        refresh_token: refresh_token.to_owned(),
        session_id: "0123456789abcdef0123456789abcdef".to_owned(),
    }
}
