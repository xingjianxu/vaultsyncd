//! 远端 API 客户端：路径/请求头/请求体装配、认证、超时、取消与一次受控刷新。
//!
//! # 认证与刷新
//!
//! 携带 [`AuthMode::Bearer`] 的请求如果收到 401，会先用会话里的刷新凭据换一对新凭据，再带新
//! 访问凭据重试**一次**：
//!
//! - 同一会话的并发 401 通过 [`AuthSession::refresh`](crate::auth::AuthSession) 的单飞锁合并，
//!   只发一次刷新请求；
//! - 刷新被服务端拒绝（旧刷新凭据已被使用、过期或撤销）时返回
//!   [`ClientError::ReauthRequired`]；重试后仍 401 同样返回它，**不会刷新第二次**；
//! - 刷新遇到连接、超时或证书问题时原样返回该错误，凭据与本地队列都不改动，等下次重试。
//!
//! # 超时与取消
//!
//! 每次请求都有超时（默认 [`Timeouts::request`](crate::config::Timeouts::request)，可按请求
//! 覆盖）。取消通过 [`Cancellation`](crate::cancel::Cancellation) 表达：取消后不再等待传输，
//! 返回 [`ClientError::Cancelled`]，不产生任何失败状态。
//!
//! # 不做的事
//!
//! 不跟随重定向、不自动重试普通网络错误（退避策略属于 agent 执行器）、不实现端点级语义
//! （Vault、changes、上传/下载分别由后续卡在本 crate 上加模块）。

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use vaultsyncd_domain::ids::ServerId;
use vaultsyncd_protocol::generated::{
    ErrorEnvelope, LoginRequest, RefreshRequest, SessionIssued, SessionRevoked,
};

use crate::auth::{AuthSession, Credentials};
use crate::cancel::Cancellation;
use crate::config::{ClientConfig, ConfigError, Timeouts, ValidatedConfig};
use crate::error::{ApiFailure, ClientError, ProtocolError, ReauthReason, TimeoutPhase};
use crate::transport::{Method, ReqwestTransport, Transport, TransportRequest, TransportResponse};

/// 请求是否携带会话凭据，以及收到 401 时是否允许刷新。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// 不需要认证的端点（健康探测、登录、刷新）。
    None,
    /// 带 `Authorization: Bearer <访问凭据>`；401 时尝试一次受控刷新。
    Bearer,
}

/// 请求体。
#[derive(Debug, Clone)]
enum RequestBody {
    /// 已序列化的 JSON。
    Json(Vec<u8>),
    /// 原始字节（固定版本内容、上传块）。
    Bytes {
        content_type: String,
        bytes: Vec<u8>,
    },
}

/// 一次远端调用的输入。
///
/// 路径必须是绝对路径（以 `/` 开头）且不含查询串、片段或空白：查询参数属于后续端点模块，
/// 出现时再加显式的编码入口，避免把未编码的文本拼进 URL。
pub struct ApiRequest {
    method: Method,
    path: String,
    auth: AuthMode,
    body: Option<RequestBody>,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    timeout: Option<Duration>,
    cancellation: Option<Cancellation>,
}

impl ApiRequest {
    /// 用方法与绝对路径构造；默认不需要认证、无请求体。
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            auth: AuthMode::None,
            body: None,
            query: Vec::new(),
            headers: Vec::new(),
            timeout: None,
            cancellation: None,
        }
    }

    /// `GET` 请求。
    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::Get, path)
    }

    /// `POST` 请求。
    pub fn post(path: impl Into<String>) -> Self {
        Self::new(Method::Post, path)
    }

    /// 添加一个查询参数。参数名和值会按 RFC 3986 做百分号编码；调用方不能把查询串直接拼进路径。
    ///
    /// # Errors
    ///
    /// 空参数名或包含控制字符的参数会被拒绝。
    pub fn with_query_param(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, ClientError> {
        let name = name.into();
        let value = value.into();
        if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(ClientError::invalid_request(
                "查询参数名不能为空或包含控制字符",
            ));
        }
        if value.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(ClientError::invalid_request("查询参数值包含控制字符"));
        }
        self.query.push((name, value));
        Ok(self)
    }

    /// 设置认证方式。
    pub fn with_auth(mut self, auth: AuthMode) -> Self {
        self.auth = auth;
        self
    }

    /// 设置 JSON 请求体。
    ///
    /// # Errors
    ///
    /// 值无法序列化成 JSON 时返回 [`ClientError::InvalidRequest`]。
    pub fn with_json<T: Serialize>(mut self, value: &T) -> Result<Self, ClientError> {
        let bytes = serde_json::to_vec(value).map_err(|error| {
            ClientError::invalid_request(format!("请求体无法序列化成 JSON：{error}"))
        })?;
        self.body = Some(RequestBody::Json(bytes));
        Ok(self)
    }

    /// 设置原始字节请求体。
    pub fn with_body(mut self, content_type: impl Into<String>, bytes: Vec<u8>) -> Self {
        self.body = Some(RequestBody::Bytes {
            content_type: content_type.into(),
            bytes,
        });
        self
    }

    /// 追加一个请求头（`Authorization` 与 `Content-Type` 由客户端生成，不能手写）。
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// 覆盖本次请求的超时。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// 挂上取消开关。
    pub fn with_cancellation(mut self, cancellation: Cancellation) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    /// 请求方法。
    pub const fn method(&self) -> Method {
        self.method
    }

    /// 请求路径。
    pub fn path(&self) -> &str {
        &self.path
    }

    /// 认证方式。
    pub const fn auth(&self) -> AuthMode {
        self.auth
    }
}

/// 一次 2xx 响应：状态、响应头与原始响应体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl ApiResponse {
    fn from_transport(response: TransportResponse) -> Self {
        let (status, headers, body) = response.into_parts();
        Self {
            status,
            headers,
            body,
        }
    }

    /// HTTP 状态码（一定是 2xx）。
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// 响应头。
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// 按名称取响应头（大小写不敏感，返回第一个匹配）。
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }

    /// 原始响应体。
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// 服务端建议的等待时间（`Retry-After` 的秒数形式）。
    pub fn retry_after(&self) -> Option<Duration> {
        retry_after(&self.headers)
    }

    /// 按生成 DTO 解码响应体。
    ///
    /// # Errors
    ///
    /// 不是 JSON 或缺必填字段、出现未知枚举、长整数不是规范十进制文本时返回
    /// [`ClientError::Protocol`]。
    pub fn json<R: DeserializeOwned>(&self) -> Result<R, ClientError> {
        serde_json::from_slice(&self.body).map_err(|error| {
            ClientError::Protocol(ProtocolError::new(format!("响应体无法解码：{error}")))
        })
    }
}

/// 远端 API 客户端。
///
/// 克隆共享同一个会话（凭据代际与刷新合并对全部请求生效）与同一个连接池。
pub struct ApiClient<T: Transport> {
    base_url: reqwest::Url,
    timeouts: Timeouts,
    transport: Arc<T>,
    auth: AuthSession,
    protocol_negotiation: bool,
    expected_server: Option<ServerId>,
}

impl<T: Transport> Clone for ApiClient<T> {
    fn clone(&self) -> Self {
        Self {
            base_url: self.base_url.clone(),
            timeouts: self.timeouts,
            transport: Arc::clone(&self.transport),
            auth: self.auth.clone(),
            protocol_negotiation: self.protocol_negotiation,
            expected_server: self.expected_server,
        }
    }
}

impl<T: Transport> std::fmt::Debug for ApiClient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiClient")
            .field("base_url", &self.base_url.as_str())
            .field("timeouts", &self.timeouts)
            .field("auth", &self.auth)
            .finish()
    }
}

impl ApiClient<ReqwestTransport> {
    /// Receive optional SSE wakes using the same negotiated protocol, TLS trust and credentials.
    /// The caller must independently poll durable changes, reconnect on errors, and cancel this
    /// future on shutdown. A full wake channel coalesces hints rather than blocking the stream.
    ///
    /// # Errors
    /// Negotiation/authentication/stream/framing failure. No cursor or local state is changed.
    pub async fn receive_hints(
        &self,
        vault: &str,
        wake: &tokio::sync::mpsc::Sender<()>,
    ) -> Result<(), ClientError> {
        if vault.len() != 32
            || !vault
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ClientError::invalid_request("invalid SSE VaultId"));
        }
        let request =
            ApiRequest::get(format!("/v1/vaults/{vault}/events")).with_auth(AuthMode::Bearer);
        self.check_capabilities(&request).await?;
        let snapshot = self.auth.snapshot().ok_or(ClientError::NotAuthenticated)?;
        let url = self
            .base_url
            .join(request.path.trim_start_matches('/'))
            .map_err(|_| ClientError::invalid_request("SSE URL"))?;
        let mut response = tokio::time::timeout(
            self.timeouts.request(),
            self.transport.events(url.as_str(), snapshot.access_token()),
        )
        .await
        .map_err(|_| ClientError::Timeout {
            phase: TimeoutPhase::Request,
        })??;
        if response.status().as_u16() == 401 {
            self.auth
                .refresh(snapshot.generation(), |token| self.send_refresh(token))
                .await?;
            return Err(ClientError::NotAuthenticated); // reconnect with the refreshed snapshot
        }
        if response.status().as_u16() != 200
            || !response
                .headers()
                .get("content-type")
                .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"))
        {
            return Err(ClientError::invalid_request("invalid SSE response"));
        }
        let mut decoder = crate::hints::Decoder::default();
        loop {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(45), response.chunk())
                .await
                .map_err(|_| ClientError::Timeout {
                    phase: TimeoutPhase::Request,
                })?
                .map_err(|_| ClientError::invalid_request("SSE body interrupted"))?;
            let Some(chunk) = chunk else {
                return Ok(());
            };
            if decoder.push(&chunk, vault)? > 0 {
                let _ = wake.try_send(());
            }
        }
    }

    /// 按配置建立生产客户端（rustls + 操作系统信任库，可选追加开发 CA）。
    ///
    /// # Errors
    ///
    /// 地址不合规、超时为零、追加的 PEM 非法或 TLS 客户端构建失败时返回 [`ConfigError`]。
    pub fn new(config: ClientConfig) -> Result<Self, ConfigError> {
        let validated = config.validated()?;
        let transport = ReqwestTransport::new(&validated)?;
        Ok(Self::from_validated(&validated, Arc::new(transport)))
    }
}

impl<T: Transport> ApiClient<T> {
    /// 用已校验的配置与注入的传输建立客户端（测试替身或自定义传输）。
    ///
    /// # Errors
    ///
    /// 配置不合规时返回 [`ConfigError`]。
    pub fn with_transport(config: ClientConfig, transport: Arc<T>) -> Result<Self, ConfigError> {
        let validated = config.validated()?;
        Ok(Self::from_validated(&validated, transport))
    }

    /// 仅隔离的认证/传输单元测试使用；默认构造器在testing构建中仍始终真实协商。
    #[cfg(any(test, feature = "testing"))]
    pub fn with_transport_for_unit_tests(
        config: ClientConfig,
        transport: Arc<T>,
    ) -> Result<Self, ConfigError> {
        let mut client = Self::with_transport(config, transport)?;
        client.protocol_negotiation = false;
        Ok(client)
    }

    /// 替换会话（例如注入带 OS 凭据库端口、或已从凭据库恢复凭据的会话）。
    pub fn with_auth_session(mut self, auth: AuthSession) -> Self {
        self.auth = auth;
        self
    }

    /// Pin the server identity checked by the public probe before sending business requests.
    pub fn with_expected_server(mut self, server: ServerId) -> Self {
        self.expected_server = Some(server);
        self
    }

    /// 服务端根地址。
    pub fn base_url(&self) -> &reqwest::Url {
        &self.base_url
    }

    /// 会话状态。
    pub const fn auth(&self) -> &AuthSession {
        &self.auth
    }

    /// 注入的传输（测试里用来核对实际发出的请求）。
    pub fn transport(&self) -> &T {
        self.transport.as_ref()
    }

    /// 发一次请求并返回 2xx 响应；非 2xx 一律转成 [`ClientError`]。
    ///
    /// # Errors
    ///
    /// 见 [`ClientError`] 的各个取值：请求不合规、传输失败、超时、取消、需要重新登录、
    /// 服务端错误信封、非信封的非 2xx 响应或响应体不合契约。
    pub async fn send(&self, request: ApiRequest) -> Result<ApiResponse, ClientError> {
        if let Some(cancellation) = &request.cancellation {
            if cancellation.is_cancelled() {
                return Err(ClientError::Cancelled);
            }
        }
        if request.auth == AuthMode::Bearer && !self.auth.has_credentials() {
            return Err(ClientError::NotAuthenticated);
        }
        self.check_capabilities(&request).await?;
        match request.auth {
            AuthMode::None => {
                let response = self.dispatch(&request, None, TimeoutPhase::Request).await?;
                classify(response)
            }
            AuthMode::Bearer => {
                let snapshot = self.auth.snapshot().ok_or(ClientError::NotAuthenticated)?;
                let response = self
                    .dispatch(
                        &request,
                        Some(snapshot.access_token()),
                        TimeoutPhase::Request,
                    )
                    .await?;
                if response.status() != 401 {
                    return classify(response);
                }
                let refreshed = self
                    .auth
                    .refresh(snapshot.generation(), |refresh_token| {
                        self.send_refresh(refresh_token)
                    })
                    .await?;
                self.check_capabilities(&request).await?;
                let retried = self
                    .dispatch(
                        &request,
                        Some(refreshed.access_token()),
                        TimeoutPhase::Request,
                    )
                    .await?;
                if retried.status() == 401 {
                    self.auth.mark_reauth_required();
                    // 第二次 401 不再刷新：交给用户重新登录，本地队列保持不动。
                    return Err(ClientError::ReauthRequired {
                        reason: ReauthReason::RetriedUnauthorized,
                        cause: Some(Box::new(error_for(&retried))),
                    });
                }
                classify(retried)
            }
        }
    }

    /// 发一次请求并把 2xx 响应体按生成 DTO 解码。
    ///
    /// # Errors
    ///
    /// 同 [`Self::send`]，外加响应体不合契约时的 [`ClientError::Protocol`]。
    pub async fn send_json<R: DeserializeOwned>(
        &self,
        request: ApiRequest,
    ) -> Result<R, ClientError> {
        self.send(request).await?.json::<R>()
    }

    /// 登录：`POST /v1/auth/login`，成功后把凭据装进会话并通知凭据保管端口。
    ///
    /// # Errors
    ///
    /// 凭据无效（401）、限流（429）、传输失败或响应不合契约。
    pub async fn login(&self, request: &LoginRequest) -> Result<SessionIssued, ClientError> {
        let response = self
            .send(ApiRequest::post("/v1/auth/login").with_json(request)?)
            .await?;
        let issued: SessionIssued = response.json()?;
        self.auth.install(Credentials::from_issued(&issued)?);
        Ok(issued)
    }

    /// 注销当前会话：`POST /v1/auth/logout`，成功后清除本地凭据（不删除任何同步数据）。
    ///
    /// # Errors
    ///
    /// 没有凭据、凭据失效（需要重新登录）、传输失败或响应不合契约。
    pub async fn logout(&self) -> Result<SessionRevoked, ClientError> {
        let response = self
            .send(ApiRequest::post("/v1/auth/logout").with_auth(AuthMode::Bearer))
            .await?;
        let revoked: SessionRevoked = response.json()?;
        self.auth.clear();
        Ok(revoked)
    }

    async fn check_capabilities(&self, request: &ApiRequest) -> Result<(), ClientError> {
        if (!self.protocol_negotiation && self.expected_server.is_none())
            || (request.auth == AuthMode::None
                && (request.path == crate::capabilities::PATH || request.path == "/v1/health"))
        {
            return Ok(());
        }
        let mut probe = ApiRequest::get(crate::capabilities::PATH);
        probe.timeout = request.timeout;
        probe.cancellation = request.cancellation.clone();
        let response = self.dispatch(&probe, None, TimeoutPhase::Request).await?;
        let value: vaultsyncd_protocol::generated::Capabilities = classify(response)?.json()?;
        if self
            .expected_server
            .is_some_and(|id| value.server_id != id.to_string())
        {
            return Err(ClientError::ServerIdentityMismatch);
        }
        crate::capabilities::admit(&value, &request.path)?;
        if request.path.contains("/parts/")
            && let Some(RequestBody::Bytes { bytes, .. }) = &request.body
            && bytes.len() as u64
                > value
                    .limits
                    .max_upload_part_bytes
                    .parse::<u64>()
                    .map_err(|_| ClientError::invalid_request("invalid negotiated upload limit"))?
        {
            return Err(ClientError::invalid_request(
                "part exceeds negotiated upload limit",
            ));
        }
        if let Some((_, limit)) = request.query.iter().find(|(name, _)| name == "limit")
            && limit
                .parse::<u32>()
                .ok()
                .is_some_and(|n| n > value.limits.max_page_size)
        {
            return Err(ClientError::invalid_request(
                "page exceeds negotiated limit",
            ));
        }
        Ok(())
    }

    fn from_validated(validated: &ValidatedConfig, transport: Arc<T>) -> Self {
        Self {
            base_url: validated.base_url().clone(),
            timeouts: validated.timeouts(),
            transport,
            auth: AuthSession::new(),
            protocol_negotiation: true,
            expected_server: None,
        }
    }

    /// 发一次刷新请求；401 是“刷新凭据被拒”，其它非 2xx 仍是普通服务端错误。
    async fn send_refresh(&self, refresh_token: String) -> Result<SessionIssued, ClientError> {
        let body = RefreshRequest { refresh_token };
        let request = ApiRequest::post("/v1/auth/refresh").with_json(&body)?;
        self.check_capabilities(&request).await?;
        let response = self.dispatch(&request, None, TimeoutPhase::Refresh).await?;
        if response.status() == 401 {
            return Err(ClientError::ReauthRequired {
                reason: ReauthReason::RefreshRejected,
                cause: Some(Box::new(error_for(&response))),
            });
        }
        classify(response)?.json::<SessionIssued>()
    }

    /// 装配并发送一次传输请求，套上超时与取消；状态码留给调用方解释。
    async fn dispatch(
        &self,
        request: &ApiRequest,
        token: Option<&str>,
        phase: TimeoutPhase,
    ) -> Result<ApiResponse, ClientError> {
        let transport_request = self.transport_request(request, token)?;
        let timeout = request.timeout.unwrap_or(self.timeouts.request());
        let send = self.transport.send(transport_request);
        let outcome = match request.cancellation.as_ref() {
            Some(cancellation) => {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(ClientError::Cancelled),
                    outcome = tokio::time::timeout(timeout, send) => outcome,
                }
            }
            None => tokio::time::timeout(timeout, send).await,
        };
        match outcome {
            Ok(Ok(response)) => Ok(ApiResponse::from_transport(response)),
            Ok(Err(error)) => Err(ClientError::Transport(error)),
            Err(_) => Err(ClientError::Timeout { phase }),
        }
    }

    fn transport_request(
        &self,
        request: &ApiRequest,
        token: Option<&str>,
    ) -> Result<TransportRequest, ClientError> {
        let url = self.resolve_path(&request.path, &request.query)?;
        let mut headers = Vec::with_capacity(request.headers.len() + 2);
        for (name, value) in &request.headers {
            if name.eq_ignore_ascii_case("authorization") {
                return Err(ClientError::invalid_request(
                    "Authorization 头由会话凭据生成，不能手写",
                ));
            }
            if request.body.is_some()
                && (name.eq_ignore_ascii_case("content-type")
                    || name.eq_ignore_ascii_case("content-length"))
            {
                return Err(ClientError::invalid_request(format!(
                    "{name} 由请求体自动生成，不能手写"
                )));
            }
            validate_header(name, value)?;
            headers.push((name.clone(), value.clone()));
        }
        if let Some(token) = token {
            headers.push(("authorization".to_owned(), format!("Bearer {token}")));
        }
        let body = match &request.body {
            None => None,
            Some(RequestBody::Json(bytes)) => {
                headers.push(("content-type".to_owned(), "application/json".to_owned()));
                Some(bytes.clone())
            }
            Some(RequestBody::Bytes {
                content_type,
                bytes,
            }) => {
                validate_header("content-type", content_type)?;
                headers.push(("content-type".to_owned(), content_type.clone()));
                Some(bytes.clone())
            }
        };
        Ok(TransportRequest::new(request.method, url, headers, body))
    }

    fn resolve_path(&self, path: &str, query: &[(String, String)]) -> Result<String, ClientError> {
        if !path.starts_with('/') {
            return Err(ClientError::invalid_request(format!(
                "请求路径必须以 / 开头：{path}"
            )));
        }
        if path.contains("://")
            || path.contains('#')
            || path.contains('?')
            || path.chars().any(char::is_whitespace)
        {
            return Err(ClientError::invalid_request(format!(
                "请求路径不能包含方案、查询串、片段或空白：{path}"
            )));
        }
        let mut resolved = format!("{}{}", self.base_url.as_str().trim_end_matches('/'), path);
        if !query.is_empty() {
            resolved.push('?');
            for (index, (name, value)) in query.iter().enumerate() {
                if index != 0 {
                    resolved.push('&');
                }
                resolved.push_str(&encode_query(name));
                resolved.push('=');
                resolved.push_str(&encode_query(value));
            }
        }
        Ok(resolved)
    }
}

/// 2xx 原样返回，其它状态码转成错误。
fn classify(response: ApiResponse) -> Result<ApiResponse, ClientError> {
    if (200..=299).contains(&response.status) {
        return Ok(response);
    }
    Err(error_for(&response))
}

/// 非 2xx 响应转成错误：合法错误信封是 [`ClientError::Api`]，否则是
/// [`ClientError::UnexpectedResponse`]。
fn error_for(response: &ApiResponse) -> ClientError {
    let status = response.status;
    match response.json::<ErrorEnvelope>() {
        Ok(envelope) => ClientError::Api(Box::new(ApiFailure::new(
            status,
            envelope.error,
            response.retry_after(),
        ))),
        Err(_) => ClientError::UnexpectedResponse {
            status,
            message: format!("非 2xx 响应不是合法错误信封（HTTP {status}）"),
        },
    }
}

fn encode_query(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(b"0123456789ABCDEF"[(byte >> 4) as usize]));
            encoded.push(char::from(b"0123456789ABCDEF"[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn retry_after(headers: &[(String, String)]) -> Option<Duration> {
    let value = find_header(headers, "retry-after")?.trim();
    // 只解析秒数形式；HTTP-date 形式返回 None，由调度器按指数退避处理。
    value.parse::<u64>().ok().map(Duration::from_secs)
}

fn validate_header(name: &str, value: &str) -> Result<(), ClientError> {
    let name_ok = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if !name_ok {
        return Err(ClientError::invalid_request(format!(
            "请求头名称不合规：{name}"
        )));
    }
    if value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(ClientError::invalid_request(format!(
            "请求头 {name} 的值包含控制字符"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use vaultsyncd_protocol::generated::{ErrorCode, VaultList};

    use crate::auth::CredentialSink;
    use crate::error::{TransportError, TransportErrorKind};
    use crate::testing::{FakeTransport, error_response, issued, json_response};

    const SESSION_ID: &str = "0123456789abcdef0123456789abcdef";

    type Fake = Arc<FakeTransport>;

    fn config() -> ClientConfig {
        ClientConfig::new("https://vault.example")
    }

    fn bearer_client<F>(handler: F) -> (ApiClient<FakeTransport>, Fake)
    where
        F: Fn(
                TransportRequest,
            ) -> crate::transport::TransportFuture<
                'static,
                Result<TransportResponse, TransportError>,
            > + Send
            + Sync
            + 'static,
    {
        let transport = Arc::new(FakeTransport::new(handler));
        let client = ApiClient::with_transport_for_unit_tests(config(), transport.clone())
            .expect("客户端配置");
        (client, transport)
    }

    fn install(client: &ApiClient<FakeTransport>, access: &str, refresh: &str) {
        client
            .auth()
            .install(Credentials::from_issued(&issued(access, refresh)).expect("合法凭据"));
    }

    fn vaults_request() -> ApiRequest {
        ApiRequest::get("/v1/vaults").with_auth(AuthMode::Bearer)
    }

    #[tokio::test]
    async fn a_401_triggers_exactly_one_refresh_and_one_retry() {
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let (client, transport) = bearer_client({
            let refresh_calls = refresh_calls.clone();
            move |request: TransportRequest| {
                let refresh_calls = refresh_calls.clone();
                Box::pin(async move {
                    if request.url().ends_with("/v1/auth/refresh") {
                        refresh_calls.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(
                            request.header("authorization"),
                            None,
                            "刷新请求不带访问凭据"
                        );
                        let body: RefreshRequest =
                            serde_json::from_slice(request.body().expect("刷新请求体"))
                                .expect("刷新请求体是 JSON");
                        assert_eq!(body.refresh_token, "refresh-1");
                        return Ok(json_response(200, &issued("access-2", "refresh-2")));
                    }
                    match request.header("authorization") {
                        Some("Bearer access-1") => Ok(error_response(
                            401,
                            ErrorCode::AuthExpired,
                            "访问凭据已过期",
                            false,
                        )),
                        Some("Bearer access-2") => {
                            Ok(json_response(200, &VaultList { vaults: Vec::new() }))
                        }
                        other => panic!("意外的凭据：{other:?}"),
                    }
                })
            }
        });
        install(&client, "access-1", "refresh-1");

        let vaults: VaultList = client
            .send_json(vaults_request())
            .await
            .expect("刷新后重试成功");
        assert!(vaults.vaults.is_empty());
        assert_eq!(transport.call_count(), 3, "业务请求 + 刷新 + 重试");
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            client.auth().snapshot().expect("凭据").access_token(),
            "access-2"
        );
        assert!(!client.auth().needs_reauth());
    }

    #[tokio::test]
    async fn a_second_401_after_refresh_requires_reauth_without_another_refresh() {
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let (client, transport) = bearer_client({
            let refresh_calls = refresh_calls.clone();
            move |request: TransportRequest| {
                let refresh_calls = refresh_calls.clone();
                Box::pin(async move {
                    if request.url().ends_with("/v1/auth/refresh") {
                        refresh_calls.fetch_add(1, Ordering::SeqCst);
                        return Ok(json_response(200, &issued("access-2", "refresh-2")));
                    }
                    Ok(error_response(
                        401,
                        ErrorCode::AuthExpired,
                        "访问凭据已过期",
                        false,
                    ))
                })
            }
        });
        install(&client, "access-1", "refresh-1");

        let error = client
            .send(vaults_request())
            .await
            .expect_err("刷新后仍 401");
        assert!(
            matches!(
                error,
                ClientError::ReauthRequired {
                    reason: ReauthReason::RetriedUnauthorized,
                    ..
                }
            ),
            "得到 {error:?}"
        );
        assert!(!error.is_retryable());
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 1, "不允许第二次刷新");
        assert_eq!(transport.call_count(), 3);
        assert!(client.auth().needs_reauth());
    }

    #[tokio::test]
    async fn concurrent_401s_share_one_refresh() {
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let (client, transport) = bearer_client({
            let refresh_calls = refresh_calls.clone();
            move |request: TransportRequest| {
                let refresh_calls = refresh_calls.clone();
                Box::pin(async move {
                    if request.url().ends_with("/v1/auth/refresh") {
                        refresh_calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        return Ok(json_response(200, &issued("access-2", "refresh-2")));
                    }
                    match request.header("authorization") {
                        Some("Bearer access-1") => Ok(error_response(
                            401,
                            ErrorCode::AuthExpired,
                            "访问凭据已过期",
                            false,
                        )),
                        Some("Bearer access-2") => {
                            Ok(json_response(200, &VaultList { vaults: Vec::new() }))
                        }
                        other => panic!("意外的凭据：{other:?}"),
                    }
                })
            }
        });
        install(&client, "access-1", "refresh-1");

        let mut tasks = Vec::new();
        for _ in 0..6 {
            let client = client.clone();
            tasks.push(tokio::spawn(async move {
                client.send_json::<VaultList>(vaults_request()).await
            }));
        }
        for task in tasks {
            task.await.expect("请求任务").expect("最终成功");
        }
        assert_eq!(
            refresh_calls.load(Ordering::SeqCst),
            1,
            "并发 401 只刷新一次"
        );
        // 6 次业务请求 + 1 次刷新；重试次数取决于竞争，但刷新恒为 1。
        assert!(transport.call_count() >= 7);
    }

    #[tokio::test]
    async fn a_rejected_refresh_requires_reauth_and_keeps_the_credentials() {
        let (client, transport) = bearer_client(|request: TransportRequest| {
            Box::pin(async move {
                if request.url().ends_with("/v1/auth/refresh") {
                    return Ok(error_response(
                        401,
                        ErrorCode::AuthExpired,
                        "刷新凭据已失效",
                        false,
                    ));
                }
                Ok(error_response(
                    401,
                    ErrorCode::AuthExpired,
                    "访问凭据已过期",
                    false,
                ))
            })
        });
        install(&client, "access-1", "refresh-1");

        let error = client.send(vaults_request()).await.expect_err("刷新被拒");
        assert!(
            matches!(
                error,
                ClientError::ReauthRequired {
                    reason: ReauthReason::RefreshRejected,
                    ..
                }
            ),
            "得到 {error:?}"
        );
        assert_eq!(transport.call_count(), 2, "业务请求 + 失败的刷新，不再重试");
        assert!(client.auth().needs_reauth());
        let kept = client
            .auth()
            .snapshot()
            .expect("本地凭据不因刷新被拒而丢弃");
        assert_eq!(kept.access_token(), "access-1");
    }

    #[tokio::test]
    async fn a_transient_refresh_failure_is_retryable_and_changes_nothing() {
        let (client, transport) = bearer_client(|request: TransportRequest| {
            Box::pin(async move {
                if request.url().ends_with("/v1/auth/refresh") {
                    return Err(TransportError::new(
                        TransportErrorKind::Connect,
                        "连接被重置",
                    ));
                }
                Ok(error_response(
                    401,
                    ErrorCode::AuthExpired,
                    "访问凭据已过期",
                    false,
                ))
            })
        });
        install(&client, "access-1", "refresh-1");

        let error = client
            .send(vaults_request())
            .await
            .expect_err("刷新临时失败");
        match &error {
            ClientError::Transport(transport) => {
                assert_eq!(transport.kind(), TransportErrorKind::Connect);
            }
            other => panic!("期望连接失败，得到 {other:?}"),
        }
        assert!(error.is_retryable());
        assert!(!client.auth().needs_reauth());
        assert_eq!(
            client.auth().snapshot().expect("凭据保留").access_token(),
            "access-1"
        );
        assert_eq!(transport.call_count(), 2);
    }

    #[tokio::test]
    async fn a_slow_transport_hits_the_per_request_timeout() {
        let (client, _transport) = bearer_client(|_request: TransportRequest| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok(json_response(200, &VaultList { vaults: Vec::new() }))
            })
        });
        let error = client
            .send(ApiRequest::get("/v1/health").with_timeout(Duration::from_millis(20)))
            .await
            .expect_err("超时");
        assert!(
            matches!(
                error,
                ClientError::Timeout {
                    phase: TimeoutPhase::Request
                }
            ),
            "得到 {error:?}"
        );
        assert!(error.is_retryable());
    }

    #[tokio::test]
    async fn a_slow_refresh_reports_the_refresh_phase() {
        let (client, _transport) = {
            let transport = Arc::new(FakeTransport::new(|request: TransportRequest| {
                Box::pin(async move {
                    if request.url().ends_with("/v1/auth/refresh") {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        return Ok(json_response(200, &issued("access-2", "refresh-2")));
                    }
                    Ok(error_response(
                        401,
                        ErrorCode::AuthExpired,
                        "访问凭据已过期",
                        false,
                    ))
                })
            }));
            let client = ApiClient::with_transport_for_unit_tests(
                config().with_timeouts(Timeouts::new(
                    Duration::from_millis(20),
                    Duration::from_secs(1),
                )),
                transport.clone(),
            )
            .expect("客户端配置");
            (client, transport)
        };
        install(&client, "access-1", "refresh-1");

        let error = client.send(vaults_request()).await.expect_err("刷新超时");
        assert!(
            matches!(
                error,
                ClientError::Timeout {
                    phase: TimeoutPhase::Refresh
                }
            ),
            "得到 {error:?}"
        );
    }

    #[tokio::test]
    async fn cancellation_before_sending_skips_the_transport() {
        let (client, transport) = bearer_client(|_request: TransportRequest| {
            Box::pin(async move { Ok(json_response(200, &VaultList { vaults: Vec::new() })) })
        });
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let error = client
            .send(ApiRequest::get("/v1/health").with_cancellation(cancellation))
            .await
            .expect_err("已取消");
        assert!(matches!(error, ClientError::Cancelled));
        assert_eq!(transport.call_count(), 0);
        assert!(!error.is_retryable());
    }

    #[tokio::test]
    async fn cancellation_while_waiting_returns_cancelled() {
        let (client, _transport) = bearer_client(|_request: TransportRequest| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok(json_response(200, &VaultList { vaults: Vec::new() }))
            })
        });
        let cancellation = Cancellation::new();
        let request = ApiRequest::get("/v1/health")
            .with_cancellation(cancellation.clone())
            .with_timeout(Duration::from_secs(5));
        let task = tokio::spawn({
            let client = client.clone();
            async move { client.send(request).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancellation.cancel();
        let error = task.await.expect("请求任务").expect_err("取消");
        assert!(matches!(error, ClientError::Cancelled));
    }

    #[tokio::test]
    async fn server_errors_are_classified_by_status_and_envelope() {
        // 403：不盲目重试。
        let (client, _transport) = bearer_client(|_request: TransportRequest| {
            Box::pin(async move {
                Ok(error_response(
                    403,
                    ErrorCode::AccessDenied,
                    "没有权限",
                    false,
                ))
            })
        });
        install(&client, "access-1", "refresh-1");
        let error = client
            .send(ApiRequest::get("/v1/vaults").with_auth(AuthMode::Bearer))
            .await
            .expect_err("越权");
        let failure = error.api_failure().expect("是服务端错误信封");
        assert_eq!(failure.status(), 403);
        assert_eq!(failure.body().code, ErrorCode::AccessDenied);
        assert!(!error.is_retryable());

        // 429 + Retry-After：可重试，并带上服务端建议的等待时间。
        let (client, _transport) = bearer_client(|_request: TransportRequest| {
            Box::pin(async move {
                Ok(error_response(429, ErrorCode::RateLimited, "太频繁", true)
                    .with_header("retry-after", "7"))
            })
        });
        let error = client
            .send(ApiRequest::get("/v1/health"))
            .await
            .expect_err("限流");
        assert!(error.is_retryable());
        assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));

        // 网关错误不是错误信封：仍可重试。
        let (client, _transport) = bearer_client(|_request: TransportRequest| {
            Box::pin(async move {
                Ok(TransportResponse::new(
                    502,
                    vec![("content-type".to_owned(), "text/html".to_owned())],
                    b"<html>bad gateway</html>".to_vec(),
                ))
            })
        });
        let error = client
            .send(ApiRequest::get("/v1/health"))
            .await
            .expect_err("网关错误");
        assert!(
            matches!(error, ClientError::UnexpectedResponse { status: 502, .. }),
            "得到 {error:?}"
        );
        assert!(error.is_retryable());

        // 2xx 但响应体不合契约：协议错误，不是重试能解决的。
        let (client, _transport) = bearer_client(|_request: TransportRequest| {
            Box::pin(async move {
                Ok(TransportResponse::new(
                    200,
                    Vec::new(),
                    br#"{"vaults": 1}"#.to_vec(),
                ))
            })
        });
        let error = client
            .send_json::<VaultList>(ApiRequest::get("/v1/vaults"))
            .await
            .expect_err("畸形响应");
        assert!(matches!(error, ClientError::Protocol(_)), "得到 {error:?}");
        assert!(!error.is_retryable());
    }

    #[tokio::test]
    async fn login_installs_credentials_and_logout_clears_them() {
        #[derive(Default)]
        struct RecordingSink {
            stored: StdMutex<Vec<String>>,
            erased: StdMutex<u32>,
        }

        impl CredentialSink for RecordingSink {
            fn credentials_stored(&self, credentials: &Credentials) {
                self.stored
                    .lock()
                    .expect("记录锁")
                    .push(credentials.session_id().to_owned());
            }

            fn credentials_erased(&self) {
                *self.erased.lock().expect("记录锁") += 1;
            }
        }

        let sink = Arc::new(RecordingSink::default());
        let session = AuthSession::with_sink(sink.clone());
        let transport = Arc::new(FakeTransport::new(|request: TransportRequest| {
            Box::pin(async move {
                match request.url() {
                    url if url.ends_with("/v1/auth/login") => {
                        let body: LoginRequest =
                            serde_json::from_slice(request.body().expect("登录请求体"))
                                .expect("登录请求体是 JSON");
                        assert_eq!(body.login, "owner");
                        assert_eq!(body.device_label, "test-device");
                        Ok(json_response(200, &issued("access-1", "refresh-1")))
                    }
                    url if url.ends_with("/v1/auth/logout") => {
                        assert_eq!(request.header("authorization"), Some("Bearer access-1"));
                        Ok(json_response(
                            200,
                            &SessionRevoked {
                                revoked_at: "1500".to_owned(),
                                session_id: SESSION_ID.to_owned(),
                            },
                        ))
                    }
                    other => panic!("意外端点：{other}"),
                }
            })
        }));
        let client = ApiClient::with_transport_for_unit_tests(config(), transport)
            .expect("客户端配置")
            .with_auth_session(session);

        let issued_session = client
            .login(&LoginRequest {
                hostname: None,
                operating_system: None,
                device_label: "test-device".to_owned(),
                login: "owner".to_owned(),
                password: "correct horse battery staple".to_owned(),
            })
            .await
            .expect("登录成功");
        assert_eq!(issued_session.access_token, "access-1");
        assert_eq!(
            client.auth().snapshot().expect("已装凭据").access_token(),
            "access-1"
        );
        {
            let stored = sink.stored.lock().expect("记录锁");
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0], SESSION_ID);
        }

        let revoked = client.logout().await.expect("注销成功");
        assert_eq!(revoked.session_id, SESSION_ID);
        assert!(client.auth().snapshot().is_none());
        assert_eq!(*sink.erased.lock().expect("记录锁"), 1);
    }

    #[tokio::test]
    async fn requests_without_credentials_fail_before_touching_the_transport() {
        let (client, transport) = bearer_client(|_request: TransportRequest| {
            Box::pin(async move { Ok(json_response(200, &VaultList { vaults: Vec::new() })) })
        });
        let error = client.send(vaults_request()).await.expect_err("没有凭据");
        assert!(matches!(error, ClientError::NotAuthenticated));
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn malformed_requests_are_rejected_before_the_transport() {
        let (client, transport) = bearer_client(|_request: TransportRequest| {
            Box::pin(async move { Ok(json_response(200, &VaultList { vaults: Vec::new() })) })
        });
        for request in [
            ApiRequest::get("v1/vaults"),
            ApiRequest::get("/v1/vaults?after=1"),
            ApiRequest::get("/v1/vaults#frag"),
            ApiRequest::get("/v1/vaults").with_header("authorization", "Bearer hand-written"),
            ApiRequest::get("/v1/health").with_header("bad header", "x"),
            ApiRequest::get("/v1/health").with_header("x-request-id", "line\r\nbreak"),
        ] {
            let error = client.send(request).await.expect_err("请求不合规");
            assert!(
                matches!(error, ClientError::InvalidRequest { .. }),
                "得到 {error:?}"
            );
        }
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn a_base_url_path_prefix_is_used_for_every_request() {
        let transport = Arc::new(FakeTransport::new(|_request: TransportRequest| {
            Box::pin(async move { Ok(json_response(200, &VaultList { vaults: Vec::new() })) })
        }));
        let client = Arc::new(
            ApiClient::with_transport_for_unit_tests(
                ClientConfig::new("https://vault.example/vault/"),
                transport.clone(),
            )
            .expect("客户端配置"),
        );
        client
            .send_json::<VaultList>(ApiRequest::get("/v1/vaults"))
            .await
            .expect("请求成功");
        let calls = transport.calls();
        assert_eq!(calls[0].url(), "https://vault.example/vault/v1/vaults");
    }
}
