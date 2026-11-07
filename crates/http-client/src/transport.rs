//! 传输端口与其生产实现。
//!
//! [`Transport`] 是本 crate 唯一的网络边界：生产用 [`ReqwestTransport`]（rustls + 操作系统
//! 信任库），测试用 `crate::testing::FakeTransport`。认证刷新、超时、取消与错误分类都在端口
//! 之上，因此这些规则可以用替身完整验证，不需要真实网络。
//!
//! 生产传输固定三条策略：不跟随重定向（避免把 `Authorization` 带到别的主机）、不关闭证书
//! 校验、只信任系统根证书与显式追加的开发 CA。

use core::future::Future;
use core::pin::Pin;

use crate::config::{ConfigError, ConfigErrorKind, ValidatedConfig};
use crate::error::{CertificateProblem, TransportError, TransportErrorKind};

/// HTTP 方法；只覆盖契约用到的五种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `PATCH`
    Patch,
    /// `DELETE`
    Delete,
}

impl Method {
    /// 线缆上的方法名。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }

    fn to_reqwest(self) -> reqwest::Method {
        match self {
            Self::Get => reqwest::Method::GET,
            Self::Post => reqwest::Method::POST,
            Self::Put => reqwest::Method::PUT,
            Self::Patch => reqwest::Method::PATCH,
            Self::Delete => reqwest::Method::DELETE,
        }
    }
}

/// 一次已经装配好的传输请求：绝对 URL、完整请求头与可选请求体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportRequest {
    method: Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

impl TransportRequest {
    /// 构造；请求头按给定顺序发送。
    pub fn new(
        method: Method,
        url: impl Into<String>,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    ) -> Self {
        Self {
            method,
            url: url.into(),
            headers,
            body,
        }
    }

    /// HTTP 方法。
    pub const fn method(&self) -> Method {
        self.method
    }

    /// 绝对 URL。
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 请求头（名称保持调用方给出的大小写）。
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// 按名称取请求头（大小写不敏感，返回第一个匹配）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// 请求体。
    pub fn body(&self) -> Option<&[u8]> {
        self.body.as_deref()
    }
}

/// 传输层回来的原始响应；状态码与响应体都还不解释。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl TransportResponse {
    /// 构造。
    pub fn new(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        Self {
            status,
            headers,
            body,
        }
    }

    /// HTTP 状态码。
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// 响应头。
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// 按名称取响应头（大小写不敏感，返回第一个匹配）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// 响应体。
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// 追加一个响应头（测试与传输实现使用）。
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// 拆成状态码、响应头与响应体。
    pub fn into_parts(self) -> (u16, Vec<(String, String)>, Vec<u8>) {
        (self.status, self.headers, self.body)
    }
}

/// 传输的异步返回类型；用装箱 future 让 [`Transport`] 可以作为 trait object 注入。
pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 传输端口：发一个已装配的请求，拿回原始响应。
///
/// 实现方只负责“把字节送出去、把状态与响应体带回来”，不做重试、认证或语义判断。
pub trait Transport: Send + Sync + 'static {
    /// 发送请求。
    fn send(
        &self,
        request: TransportRequest,
    ) -> TransportFuture<'_, Result<TransportResponse, TransportError>>;
}

/// 生产传输：reqwest + rustls，系统信任库加显式开发 CA。
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Same TLS/redirect policy as commands, but keep a streaming body rather than buffering it.
    pub(crate) async fn events(
        &self,
        url: &str,
        token: &str,
    ) -> Result<reqwest::Response, crate::ClientError> {
        self.client
            .get(url)
            .bearer_auth(token)
            .header("accept", "text/event-stream")
            .send()
            .await
            .map_err(|error| {
                crate::ClientError::invalid_request(format!("SSE connection failed: {error}"))
            })
    }

    /// 按已校验的配置建立连接池。
    ///
    /// # Errors
    ///
    /// 追加的 PEM 不是合法证书，或 reqwest 客户端构建失败时返回 [`ConfigErrorKind::Tls`]。
    pub(crate) fn new(config: &ValidatedConfig) -> Result<Self, ConfigError> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(config.timeouts().connect())
            // 不跟随重定向：跨主机的 3xx 会把 Bearer 带到别处；重定向按非 2xx 显式失败。
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("vaultsyncd-client/", env!("CARGO_PKG_VERSION")));
        for pem in config.trust().additional_roots() {
            let certificates = reqwest::Certificate::from_pem_bundle(pem).map_err(|error| {
                ConfigError::new(
                    ConfigErrorKind::Tls,
                    format!("开发 CA 不是合法的 PEM 证书：{error}"),
                )
            })?;
            if certificates.is_empty() {
                return Err(ConfigError::new(
                    ConfigErrorKind::Tls,
                    "开发 CA 的 PEM 里没有证书",
                ));
            }
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        let client = builder.build().map_err(|error| {
            ConfigError::new(
                ConfigErrorKind::Tls,
                format!("HTTPS 客户端构建失败：{error}"),
            )
        })?;
        Ok(Self { client })
    }
}

impl Transport for ReqwestTransport {
    fn send(
        &self,
        request: TransportRequest,
    ) -> TransportFuture<'_, Result<TransportResponse, TransportError>> {
        Box::pin(async move {
            let mut builder = self
                .client
                .request(request.method.to_reqwest(), request.url.as_str());
            for (name, value) in &request.headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let response = builder
                .send()
                .await
                .map_err(|error| classify_reqwest_error(&error))?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let body = response
                .bytes()
                .await
                .map_err(|error| classify_reqwest_error(&error))?
                .to_vec();
            Ok(TransportResponse::new(status, headers, body))
        })
    }
}

/// 把 reqwest 的错误翻成本 crate 的类别；证书问题必须能和“连不上”分开。
fn classify_reqwest_error(error: &reqwest::Error) -> TransportError {
    let kind = transport_error_kind(error);
    TransportError::new(kind, error.to_string())
}

fn transport_error_kind(error: &reqwest::Error) -> TransportErrorKind {
    if let Some(problem) = rustls_problem(error) {
        return problem.into_transport_kind();
    }
    if error.is_timeout() {
        TransportErrorKind::Timeout
    } else if error.is_connect() {
        TransportErrorKind::Connect
    } else if error.is_body() || error.is_decode() {
        TransportErrorKind::Protocol
    } else {
        TransportErrorKind::Io
    }
}

/// 错误链里出现过的 rustls 判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RustlsProblem {
    /// 证书校验失败，附具体原因。
    Certificate(CertificateProblem),
    /// 其它 TLS 失败（握手、版本、密码套件等）。
    Handshake,
}

impl RustlsProblem {
    const fn into_transport_kind(self) -> TransportErrorKind {
        match self {
            Self::Certificate(problem) => TransportErrorKind::Certificate(problem),
            Self::Handshake => TransportErrorKind::Tls,
        }
    }
}

/// 在错误链里找 rustls 的判定结果。
///
/// rustls 的错误常被 `tokio-rustls` 逐层包进 `io::Error`，而 `io::Error::source()` 返回的是内部
/// 错误自己的 source（通常是 `None`），所以除了沿 `source()` 走，还要递归看
/// `io::Error::get_ref()`。
fn rustls_problem(error: &reqwest::Error) -> Option<RustlsProblem> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if let Some(problem) = find_rustls_problem(current) {
            return Some(problem);
        }
        source = current.source();
    }
    None
}

fn find_rustls_problem(error: &(dyn std::error::Error + 'static)) -> Option<RustlsProblem> {
    if let Some(rustls_error) = error.downcast_ref::<rustls::Error>() {
        return Some(classify_rustls_error(rustls_error));
    }
    if let Some(io_error) = error.downcast_ref::<std::io::Error>() {
        if let Some(inner) = io_error.get_ref() {
            return find_rustls_problem(inner);
        }
    }
    None
}

fn classify_rustls_error(error: &rustls::Error) -> RustlsProblem {
    match error {
        rustls::Error::InvalidCertificate(problem) => {
            RustlsProblem::Certificate(certificate_problem_of(problem))
        }
        _ => RustlsProblem::Handshake,
    }
}

fn certificate_problem_of(error: &rustls::CertificateError) -> CertificateProblem {
    use rustls::CertificateError;
    match error {
        CertificateError::UnknownIssuer => CertificateProblem::UnknownIssuer,
        CertificateError::Expired => CertificateProblem::Expired,
        CertificateError::NotValidYet => CertificateProblem::NotYetValid,
        CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
            CertificateProblem::NameMismatch
        }
        CertificateError::Revoked => CertificateProblem::Revoked,
        _ => CertificateProblem::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::time::Duration;

    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use crate::client::{ApiClient, ApiRequest};
    use crate::config::{ClientConfig, TlsTrust};

    /// 一段测试用 CA 与其私钥。
    struct TestCa {
        certificate: rcgen::Certificate,
        key: KeyPair,
    }

    impl TestCa {
        fn new(common_name: &str) -> Self {
            let mut params = CertificateParams::new(Vec::new()).expect("CA 参数");
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params
                .distinguished_name
                .push(DnType::CommonName, common_name);
            let key = KeyPair::generate().expect("CA 私钥");
            let certificate = params.self_signed(&key).expect("CA 证书");
            Self { certificate, key }
        }

        fn pem(&self) -> String {
            self.certificate.pem()
        }

        /// 为 `127.0.0.1` 签一张服务端证书。
        fn server_certificate(&self) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
            let mut params =
                CertificateParams::new(vec!["127.0.0.1".to_owned()]).expect("服务端证书参数");
            params
                .distinguished_name
                .push(DnType::CommonName, "vaultsyncd s053 test");
            let key = KeyPair::generate().expect("服务端私钥");
            let certificate = params
                .signed_by(&key, &self.certificate, &self.key)
                .expect("签发服务端证书");
            (
                certificate.der().clone(),
                PrivateKeyDer::Pkcs8(key.serialize_der().into()),
            )
        }
    }

    /// 起一个只回一份固定 JSON 的 HTTPS 服务；返回根地址。
    async fn spawn_https_server(
        certificate: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
    ) -> String {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], key)
            .expect("服务端 TLS 配置");
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定端口");
        let address = listener.local_addr().expect("本地地址");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let mut head = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    loop {
                        match stream.read(&mut chunk).await {
                            Ok(0) => return,
                            Ok(read) => head.extend_from_slice(&chunk[..read]),
                            Err(_) => return,
                        }
                        if head.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let body = br#"{"ok":true}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                    let _ = stream.flush().await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        format!("127.0.0.1:{}", address.port())
    }

    async fn get(config: ClientConfig) -> Result<Vec<u8>, crate::error::ClientError> {
        let client = ApiClient::new(config).expect("客户端配置");
        client
            .send(ApiRequest::get("/v1/health"))
            .await
            .map(|response| response.body().to_vec())
    }

    #[tokio::test]
    async fn an_untrusted_certificate_is_reported_as_a_certificate_error() {
        let ca = TestCa::new("vaultsyncd s053 untrusted CA");
        let (certificate, key) = ca.server_certificate();
        let address = spawn_https_server(certificate, key).await;

        let error = get(ClientConfig::new(format!("https://{address}")))
            .await
            .expect_err("系统信任库不认识这张自签证书");
        assert_eq!(
            error.certificate_problem(),
            Some(CertificateProblem::UnknownIssuer),
            "证书失败必须能与“连不上”区分：{error}"
        );
        assert!(!error.is_retryable(), "证书错误不可重试");
    }

    #[tokio::test]
    async fn an_explicitly_trusted_development_ca_is_accepted() {
        let ca = TestCa::new("vaultsyncd s053 trusted CA");
        let (certificate, key) = ca.server_certificate();
        let address = spawn_https_server(certificate, key).await;

        let config = ClientConfig::new(format!("https://{address}"))
            .with_trust(TlsTrust::system_roots_with_pem(ca.pem()));
        let body = get(config).await.expect("显式信任的开发 CA 可用");
        assert_eq!(body, br#"{"ok":true}"#.to_vec());
    }

    #[tokio::test]
    async fn trusting_one_development_ca_does_not_disable_verification() {
        // 只追加“另一张” CA：目标证书依旧不受信任，说明追加根证书不是关闭校验的开关。
        let server_ca = TestCa::new("vaultsyncd s053 server CA");
        let (certificate, key) = server_ca.server_certificate();
        let address = spawn_https_server(certificate, key).await;

        let other_ca = TestCa::new("vaultsyncd s053 unrelated CA");
        let config = ClientConfig::new(format!("https://{address}"))
            .with_trust(TlsTrust::system_roots_with_pem(other_ca.pem()));
        let error = get(config).await.expect_err("无关 CA 不应让请求通过");
        assert_eq!(
            error.certificate_problem(),
            Some(CertificateProblem::UnknownIssuer)
        );
    }

    #[tokio::test]
    async fn an_invalid_pem_is_rejected_when_building_the_transport() {
        let config = ClientConfig::new("https://127.0.0.1:1")
            .with_trust(TlsTrust::system_roots_with_pem(b"not a pem".to_vec()));
        let error = ApiClient::new(config).expect_err("非法 PEM 必须报配置错误");
        assert_eq!(error.kind(), ConfigErrorKind::Tls);
    }

    #[test]
    fn rustls_problems_are_classified_as_certificate_or_handshake() {
        assert_eq!(
            classify_rustls_error(&rustls::Error::InvalidCertificate(
                rustls::CertificateError::Expired
            )),
            RustlsProblem::Certificate(CertificateProblem::Expired)
        );
        assert_eq!(
            classify_rustls_error(&rustls::Error::NoCertificatesPresented),
            RustlsProblem::Handshake
        );
        assert_eq!(
            RustlsProblem::Handshake.into_transport_kind(),
            TransportErrorKind::Tls
        );
    }

    #[test]
    fn certificate_problem_mapping_covers_the_common_cases() {
        use rustls::CertificateError;
        assert_eq!(
            certificate_problem_of(&CertificateError::Expired),
            CertificateProblem::Expired
        );
        assert_eq!(
            certificate_problem_of(&CertificateError::NotValidYet),
            CertificateProblem::NotYetValid
        );
        assert_eq!(
            certificate_problem_of(&CertificateError::UnknownIssuer),
            CertificateProblem::UnknownIssuer
        );
        assert_eq!(
            certificate_problem_of(&CertificateError::Revoked),
            CertificateProblem::Revoked
        );
        assert_eq!(
            certificate_problem_of(&CertificateError::BadEncoding),
            CertificateProblem::Other
        );
    }

    #[tokio::test]
    async fn connection_refused_is_a_connect_error() {
        // 127.0.0.1:1 通常没人监听；用 10 ms 连接超时避免慢网络把用例拖住。
        let config = ClientConfig::new("https://127.0.0.1:1").with_timeouts(
            crate::config::Timeouts::new(Duration::from_millis(500), Duration::from_millis(10)),
        );
        let error = get(config).await.expect_err("端口没人监听");
        match error {
            crate::error::ClientError::Transport(transport) => {
                assert_eq!(transport.kind(), TransportErrorKind::Connect);
                assert!(transport.kind().is_retryable());
            }
            other => panic!("期望连接失败，得到 {other:?}"),
        }
    }
}
