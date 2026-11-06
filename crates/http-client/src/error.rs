//! 客户端错误分类：把“重试可能有救”“必须重新登录”“本地数据别动”分成不同的值。
//!
//! 分类是设计 §13.3 的落点：普通网络故障可退避重试，401 只允许一次受控刷新，403 不盲目重试，
//! 证书错误既不可重试、也没有任何全局关闭校验的后门。调度与退避属于后续的 agent 执行器
//! （`crates/agent-core`），本 crate 只给结论与建议（[`ClientError::is_retryable`]、
//! [`ClientError::retry_after`]），不自己循环重试。

use core::fmt;
use std::time::Duration;

use vaultsyncd_protocol::generated::ErrorBody;

/// 证书校验失败的具体原因。
///
/// 这些取值来自 rustls 的判定，只用于让用户看到“证书不受信任 / 过期 / 名字不匹配”这类可行动
/// 结论；无论哪一种，都不是可重试错误，也不会触发凭据刷新。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateProblem {
    /// 找不到可信签发者：自签证书或未显式信任的开发 CA。
    UnknownIssuer,
    /// 证书已过期。
    Expired,
    /// 证书尚未生效。
    NotYetValid,
    /// 证书名称与请求的主机不匹配。
    NameMismatch,
    /// 证书已被撤销。
    Revoked,
    /// 其它证书问题（编码、用途等），细节在错误消息里。
    Other,
}

impl CertificateProblem {
    /// 稳定的短标签，供日志与状态展示使用。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownIssuer => "unknown_issuer",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::NameMismatch => "name_mismatch",
            Self::Revoked => "revoked",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for CertificateProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 传输失败的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportErrorKind {
    /// 连接没建立起来（DNS、拒绝连接、网络不可达等）。
    Connect,
    /// 连接或请求超时。
    Timeout,
    /// TLS 握手失败（不是证书判定问题）。
    Tls,
    /// 证书校验失败，附具体原因。
    Certificate(CertificateProblem),
    /// 连接读写失败（连接被重置、响应体中断等）。
    Io,
    /// HTTP 层协议错误（畸形响应头、响应体解码失败等）。
    Protocol,
}

impl TransportErrorKind {
    /// 稳定的短标签，供日志与状态展示使用。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Timeout => "timeout",
            Self::Tls => "tls",
            Self::Certificate(_) => "certificate",
            Self::Io => "io",
            Self::Protocol => "protocol",
        }
    }

    /// 同一请求稍后原样重试是否可能成功。
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Connect | Self::Timeout | Self::Io)
    }
}

impl fmt::Display for TransportErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Certificate(problem) => write!(f, "certificate({problem})"),
            other => f.write_str(other.as_str()),
        }
    }
}

/// 一次传输失败：类别加面向诊断的消息（不含令牌与密码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    kind: TransportErrorKind,
    message: String,
}

impl TransportError {
    /// 用类别与消息构造。
    pub fn new(kind: TransportErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// 失败类别。
    pub const fn kind(&self) -> TransportErrorKind {
        self.kind
    }

    /// 面向诊断的消息。
    pub fn message(&self) -> &str {
        &self.message
    }

    /// 证书问题（如果有）。
    pub const fn certificate_problem(&self) -> Option<CertificateProblem> {
        match self.kind {
            TransportErrorKind::Certificate(problem) => Some(problem),
            _ => None,
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "传输失败（{}）：{}", self.kind, self.message)
    }
}

impl std::error::Error for TransportError {}

/// 响应不符合契约：未知枚举、缺字段、非法十进制文本或不是 JSON。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    message: String,
}

impl ProtocolError {
    /// 用诊断消息构造。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 面向诊断的消息。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "响应不符合契约：{}", self.message)
    }
}

impl std::error::Error for ProtocolError {}

/// 超时发生在哪一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPhase {
    /// 业务请求本身。
    Request,
    /// 刷新凭据的请求。
    Refresh,
}

impl TimeoutPhase {
    /// 稳定的短标签，供日志与状态展示使用。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Refresh => "refresh",
        }
    }
}

impl fmt::Display for TimeoutPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 需要重新登录的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReauthReason {
    /// 刷新凭据被服务端拒绝（过期、已撤销或已被使用）；本地数据必须保留，等用户重新登录。
    RefreshRejected,
    /// 刷新成功后再带新凭据请求仍是 401：不再刷新，交回用户。
    RetriedUnauthorized,
}

impl ReauthReason {
    /// 稳定的短标签，供日志与状态展示使用。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RefreshRejected => "refresh_rejected",
            Self::RetriedUnauthorized => "retried_unauthorized",
        }
    }
}

impl fmt::Display for ReauthReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 服务端按错误信封回复的一次失败（design §15.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiFailure {
    status: u16,
    body: ErrorBody,
    retry_after: Option<Duration>,
}

impl ApiFailure {
    /// 用 HTTP 状态、已解码的错误主体与服务端的 `Retry-After` 构造。
    pub fn new(status: u16, body: ErrorBody, retry_after: Option<Duration>) -> Self {
        Self {
            status,
            body,
            retry_after,
        }
    }

    /// HTTP 状态码。
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// 已解码的错误主体。
    pub const fn body(&self) -> &ErrorBody {
        &self.body
    }

    /// 服务端给出的 `Retry-After`（只解析秒数形式）。
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
}

impl fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 生成枚举没有稳定的 `as_str`；这里用 Debug 变体名，wire 文本由 serde 输出，
        // 诊断只需要能一眼看出是哪个码。
        write!(
            f,
            "服务端拒绝（HTTP {}，{:?}）：{}",
            self.status, self.body.code, self.body.message
        )
    }
}

/// 一次请求失败的全部形态。
///
/// 每个取值只说明“为什么失败”，不改变任何本地状态：调用方据此决定退避重试、重新登录还是
/// 把条目保留在本地队列里。证书失败与非 2xx 响应都不会被自动重试，也不会被“关闭校验”绕过。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// 请求无法构造（路径、请求头、请求体或刷新凭据形态不合规），修正代码后才可能成功。
    InvalidRequest {
        /// 面向诊断的消息。
        message: String,
    },
    /// 传输层失败；是否可重试见 [`TransportErrorKind::is_retryable`]。
    Transport(TransportError),
    /// 请求或刷新超时；同一请求稍后重试可能成功。
    Timeout {
        /// 超时发生在哪一步。
        phase: TimeoutPhase,
    },
    /// 调用方取消；不是失败，也不再重试。
    Cancelled,
    /// 本地没有可用会话凭据。
    NotAuthenticated,
    /// 需要用户重新登录；本地队列与未确认内容必须原样保留。
    ReauthRequired {
        /// 需要重新登录的原因。
        reason: ReauthReason,
        /// 触发结论的原始失败（例如刷新接口的 401），供诊断。
        cause: Option<Box<ClientError>>,
    },
    /// 服务端按错误信封回复的非 2xx 响应（装箱以缩小 `Result` 的错误分支）。
    Api(Box<ApiFailure>),
    /// 非 2xx 响应不是合法错误信封（反向代理、网关或协议漂移）。
    UnexpectedResponse {
        /// HTTP 状态码。
        status: u16,
        /// 面向诊断的消息。
        message: String,
    },
    /// 2xx 响应不是契约允许的形状。
    Protocol(ProtocolError),
}

impl ClientError {
    /// 用诊断消息构造 [`ClientError::InvalidRequest`]。
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest {
            message: message.into(),
        }
    }

    /// 同一请求稍后原样重试是否可能成功。
    ///
    /// 证书问题、协议错误、请求构造错误与“需要重新登录”都是 `false`：重试改变不了结论。
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(error) => error.kind().is_retryable(),
            Self::Timeout { .. } => true,
            Self::UnexpectedResponse { status, .. } => *status >= 500,
            Self::Api(failure) => failure.body().retryable,
            Self::Cancelled
            | Self::InvalidRequest { .. }
            | Self::NotAuthenticated
            | Self::ReauthRequired { .. }
            | Self::Protocol(_) => false,
        }
    }

    /// 服务端建议的等待时间（`Retry-After`）。
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api(failure) => failure.retry_after(),
            _ => None,
        }
    }

    /// 证书问题（如果有）。
    pub fn certificate_problem(&self) -> Option<CertificateProblem> {
        match self {
            Self::Transport(error) => error.certificate_problem(),
            _ => None,
        }
    }

    /// 服务端错误主体（如果这次失败来自合法错误信封）。
    pub fn api_failure(&self) -> Option<&ApiFailure> {
        match self {
            Self::Api(failure) => Some(failure.as_ref()),
            _ => None,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest { message } => write!(f, "请求不合规：{message}"),
            Self::Transport(error) => write!(f, "{error}"),
            Self::Timeout { phase } => write!(f, "请求超时（{phase}）"),
            Self::Cancelled => f.write_str("请求已取消"),
            Self::NotAuthenticated => f.write_str("本地没有可用会话凭据"),
            Self::ReauthRequired { reason, .. } => write!(f, "需要重新登录（{reason}）"),
            Self::Api(failure) => write!(f, "{failure}"),
            Self::UnexpectedResponse { status, message } => {
                write!(f, "非 2xx 响应不是错误信封（HTTP {status}）：{message}")
            }
            Self::Protocol(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ClientError {}
