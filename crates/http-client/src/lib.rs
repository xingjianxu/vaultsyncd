//! 共用远端 HTTPS 客户端：会话认证与刷新合并、超时、取消与错误分类。
//!
//! 设计依据 design §5.4（`crates/http-client/` 是客户端唯一的远端 HTTP 适配层）、§13.3
//! （普通网络故障退避、401 一次受控刷新、403 不盲目重试）、§17.3（不因开发方便降低传输安全）。
//!
//! # 边界
//!
//! - **只依赖 wire 契约**：请求/响应类型来自 [`vaultsyncd_protocol::generated`]（`api/openapi.yaml`
//!   生成），不定义第二套 DTO，也不手改生成代码。
//! - **不碰本地状态**：不写数据库、不碰队列、不读时钟、不取随机数。刷新失败时它只报告
//!   “需要重新登录”，本地未确认内容与上传队列由上层原样保留。
//! - **网络只有一条出口**：[`transport::Transport`] 端口；生产实现是 rustls + 操作系统信任库，
//!   测试注入 [`testing::FakeTransport`]（仅 `testing` feature）。认证刷新、超时、取消与错误分类
//!   都在端口之上，因此这些规则用替身即可完整验证。
//! - **TLS 不可降级**：只有系统信任库或“系统信任库 + 显式追加的开发 CA”，没有关闭校验的配置项。
//!   证书问题单独分类（[`error::CertificateProblem`]），不可重试，也不会触发刷新。
//!
//! # 典型用法
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use vaultsyncd_http_client::{ApiClient, ApiRequest, AuthMode, ClientConfig};
//!
//! let client = ApiClient::new(ClientConfig::new("https://vault.example:8443"))?;
//! // 凭据由登录或 OS 凭据库恢复后装入会话。
//! let roots = client
//!     .send_json::<vaultsyncd_protocol::generated::RootList>(
//!         ApiRequest::get("/v1/roots").with_auth(AuthMode::Bearer),
//!     )
//!     .await?;
//! # let _ = roots;
//! # Ok(())
//! # }
//! ```
//!
//! # 尚未实现（后续卡）
//!
//! 本卡只做传输骨架与认证刷新；Root、events/changes 拉取、上传会话与固定版本内容下载分别由
//! 后续卡在本 crate 内新增模块（design §5.4 列出的 `roots.rs`、`changes.rs`、`uploads.rs`、
//! `content.rs`）。查询串编码也留给需要它的端点一并引入。

#![forbid(unsafe_code)]

pub mod auth;
pub mod cancel;
pub mod client;
pub mod config;
pub mod error;
pub mod transport;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use auth::{AuthSession, CredentialSink, CredentialSnapshot, Credentials};
pub use cancel::Cancellation;
pub use client::{ApiClient, ApiRequest, ApiResponse, AuthMode};
pub use config::{
    ClientConfig, ConfigError, ConfigErrorKind, DEFAULT_CONNECT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT,
    Timeouts, TlsTrust,
};
pub use error::{
    ApiFailure, CertificateProblem, ClientError, ProtocolError, ReauthReason, TimeoutPhase,
    TransportError, TransportErrorKind,
};
pub use transport::{Method, ReqwestTransport, Transport, TransportRequest, TransportResponse};
