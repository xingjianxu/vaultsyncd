//! 客户端配置：服务端根地址、超时与 TLS 信任方式。
//!
//! TLS 只有两种形态：系统信任库，或“系统信任库 + 显式追加的开发 CA”。没有“忽略证书错误”
//! 的配置项、环境变量或构造器——开发环境的自签 CA 必须由调用方以 PEM 文件显式给出
//! （design §17.3：不因开发方便而降低传输安全）。
//!
//! 明文 `http://` 只用于本机或隔离测试：必须在配置里显式打开
//! [`ClientConfig::allow_plaintext_http`]，默认拒绝。

use core::fmt;
use std::path::Path;
use std::time::Duration;

/// 默认的单次请求超时。
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 默认的连接（TCP + TLS 握手）超时。
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 请求与连接超时。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    request: Duration,
    connect: Duration,
}

impl Timeouts {
    /// 用两个超时构造；两者都必须大于零（在 [`ClientConfig`] 校验时检查）。
    pub const fn new(request: Duration, connect: Duration) -> Self {
        Self { request, connect }
    }

    /// 工程默认值：[`DEFAULT_REQUEST_TIMEOUT`] 与 [`DEFAULT_CONNECT_TIMEOUT`]。
    pub const fn standard() -> Self {
        Self {
            request: DEFAULT_REQUEST_TIMEOUT,
            connect: DEFAULT_CONNECT_TIMEOUT,
        }
    }

    /// 单次请求（含读取响应体）的超时。
    pub const fn request(&self) -> Duration {
        self.request
    }

    /// 连接（TCP + TLS 握手）的超时。
    pub const fn connect(&self) -> Duration {
        self.connect
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Self::standard()
    }
}

/// TLS 信任来源。
///
/// [`Self::SystemRoots`] 是默认值：用操作系统信任库校验服务端证书。开发或自建部署的 CA
/// 只能用 [`Self::system_roots_with_pem`] / [`Self::system_roots_with_pem_file`] 显式追加，
/// 追加不会关闭对其它证书的校验。
#[derive(Clone, PartialEq, Eq)]
pub enum TlsTrust {
    /// 只用操作系统信任库。
    SystemRoots,
    /// 操作系统信任库，加上显式给出的 PEM 证书（可多段）。
    SystemRootsWithAdditionalRoots(Vec<Vec<u8>>),
}

impl TlsTrust {
    /// 只用操作系统信任库。
    pub const fn system_roots() -> Self {
        Self::SystemRoots
    }

    /// 追加一段 PEM 证书（可以是多段证书的 bundle）。内容在建立传输时校验。
    pub fn system_roots_with_pem(pem: impl Into<Vec<u8>>) -> Self {
        Self::SystemRootsWithAdditionalRoots(vec![pem.into()])
    }

    /// 从文件读入要追加的 PEM 证书。
    ///
    /// # Errors
    ///
    /// 文件读不出来时返回 [`ConfigErrorKind::Io`]。
    pub fn system_roots_with_pem_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let pem = std::fs::read(path).map_err(|error| {
            ConfigError::new(
                ConfigErrorKind::Io,
                format!("开发 CA 文件读不出来（{}）：{error}", path.display()),
            )
        })?;
        Ok(Self::SystemRootsWithAdditionalRoots(vec![pem]))
    }

    /// 显式追加的 PEM 证书；系统信任库形态返回空切片。
    pub fn additional_roots(&self) -> &[Vec<u8>] {
        match self {
            Self::SystemRoots => &[],
            Self::SystemRootsWithAdditionalRoots(roots) => roots,
        }
    }
}

impl fmt::Debug for TlsTrust {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SystemRoots => f.write_str("TlsTrust::SystemRoots"),
            Self::SystemRootsWithAdditionalRoots(roots) => f
                .debug_struct("TlsTrust::SystemRootsWithAdditionalRoots")
                .field("additional_pem_blobs", &roots.len())
                .finish(),
        }
    }
}

/// 客户端配置；构造后由 [`crate::ApiClient::new`] 校验。
#[derive(Debug, Clone)]
pub struct ClientConfig {
    base_url: String,
    timeouts: Timeouts,
    trust: TlsTrust,
    plaintext_http: bool,
}

impl ClientConfig {
    /// 以服务端根地址建立配置（默认超时、系统信任库、拒绝明文 HTTP）。
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            timeouts: Timeouts::standard(),
            trust: TlsTrust::system_roots(),
            plaintext_http: false,
        }
    }

    /// 替换超时。
    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// 替换 TLS 信任来源（例如追加开发 CA）。
    pub fn with_trust(mut self, trust: TlsTrust) -> Self {
        self.trust = trust;
        self
    }

    /// 显式允许明文 `http://`。只用于本机或隔离测试，不要用于真实部署。
    pub fn allow_plaintext_http(mut self) -> Self {
        self.plaintext_http = true;
        self
    }

    /// 服务端根地址（未规范化的原始文本）。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 超时设置。
    pub const fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    /// TLS 信任来源。
    pub const fn trust(&self) -> &TlsTrust {
        &self.trust
    }

    /// 是否显式允许了明文 HTTP。
    pub const fn plaintext_http_allowed(&self) -> bool {
        self.plaintext_http
    }

    /// 校验并规范化配置；[`crate::ApiClient`] 在构造时调用。
    pub(crate) fn validated(&self) -> Result<ValidatedConfig, ConfigError> {
        let base_url = reqwest::Url::parse(self.base_url.trim()).map_err(|error| {
            ConfigError::new(ConfigErrorKind::Url, format!("服务端地址无法解析：{error}"))
        })?;
        if !base_url.username().is_empty() || base_url.password().is_some() {
            return Err(ConfigError::new(
                ConfigErrorKind::Url,
                "服务端地址不能包含用户名或密码",
            ));
        }
        if base_url.query().is_some() || base_url.fragment().is_some() {
            return Err(ConfigError::new(
                ConfigErrorKind::Url,
                "服务端地址不能带查询串或片段（请求路径各自携带）",
            ));
        }
        match base_url.scheme() {
            "https" => {}
            "http" if self.plaintext_http => {}
            "http" => {
                return Err(ConfigError::new(
                    ConfigErrorKind::PlaintextForbidden,
                    "明文 http 需要显式调用 ClientConfig::allow_plaintext_http（只用于本机或隔离测试）",
                ));
            }
            scheme => {
                return Err(ConfigError::new(
                    ConfigErrorKind::Scheme,
                    format!("不支持的 URL 方案：{scheme}（只接受 https）"),
                ));
            }
        }
        if base_url.host_str().is_none() {
            return Err(ConfigError::new(
                ConfigErrorKind::Url,
                "服务端地址缺少主机名",
            ));
        }
        if self.timeouts.request.is_zero() || self.timeouts.connect.is_zero() {
            return Err(ConfigError::new(
                ConfigErrorKind::Timeout,
                "请求与连接超时都必须大于零",
            ));
        }
        Ok(ValidatedConfig {
            base_url,
            timeouts: self.timeouts,
            trust: self.trust.clone(),
        })
    }
}

/// 已校验的配置：地址是可用的 [`reqwest::Url`]，超时非零。
#[derive(Debug, Clone)]
pub(crate) struct ValidatedConfig {
    base_url: reqwest::Url,
    timeouts: Timeouts,
    trust: TlsTrust,
}

impl ValidatedConfig {
    /// 服务端根地址（无查询串、无片段、无用户信息）。
    pub(crate) const fn base_url(&self) -> &reqwest::Url {
        &self.base_url
    }

    /// 超时设置。
    pub(crate) const fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    /// TLS 信任来源。
    pub(crate) const fn trust(&self) -> &TlsTrust {
        &self.trust
    }
}

/// 配置错误的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    /// 地址无法解析或形态不合规。
    Url,
    /// URL 方案不是 https/http。
    Scheme,
    /// 明文 HTTP 未显式允许。
    PlaintextForbidden,
    /// 超时为零。
    Timeout,
    /// TLS 信任配置不可用（PEM 非法、客户端构建失败）。
    Tls,
    /// 读开发 CA 文件失败。
    Io,
}

impl ConfigErrorKind {
    /// 稳定的短标签，供日志与状态展示使用。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Url => "url",
            Self::Scheme => "scheme",
            Self::PlaintextForbidden => "plaintext_forbidden",
            Self::Timeout => "timeout",
            Self::Tls => "tls",
            Self::Io => "io",
        }
    }
}

/// 客户端配置错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    message: String,
}

impl ConfigError {
    /// 用类别与消息构造。
    pub fn new(kind: ConfigErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// 错误类别。
    pub const fn kind(&self) -> ConfigErrorKind {
        self.kind
    }

    /// 面向诊断的消息。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "客户端配置错误（{}）：{}",
            self.kind.as_str(),
            self.message
        )
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn validated(base_url: &str) -> Result<ValidatedConfig, ConfigError> {
        ClientConfig::new(base_url).validated()
    }

    #[test]
    fn https_is_the_default_and_plaintext_needs_explicit_opt_in() {
        assert!(validated("https://vault.example:8443").is_ok());

        let error = validated("http://127.0.0.1:8080").expect_err("默认拒绝明文 http");
        assert_eq!(error.kind(), ConfigErrorKind::PlaintextForbidden);

        let allowed = ClientConfig::new("http://127.0.0.1:8080")
            .allow_plaintext_http()
            .validated()
            .expect("显式允许后可用");
        assert_eq!(allowed.base_url().scheme(), "http");
    }

    #[test]
    fn only_https_and_http_are_accepted() {
        assert_eq!(
            validated("vault.example").expect_err("没有方案").kind(),
            ConfigErrorKind::Url
        );
        for base_url in ["ftp://vault.example", "file:///tmp/vault"] {
            let error = validated(base_url).expect_err("方案不合规");
            assert_eq!(error.kind(), ConfigErrorKind::Scheme);
        }
    }

    #[test]
    fn userinfo_and_query_are_rejected() {
        for base_url in [
            "https://owner:secret@vault.example",
            "https://vault.example/?root=1",
            "https://vault.example/#fragment",
        ] {
            let error = validated(base_url).expect_err("地址形态不合规");
            assert_eq!(error.kind(), ConfigErrorKind::Url);
        }
    }

    #[test]
    fn zero_timeouts_are_rejected() {
        let config = ClientConfig::new("https://vault.example")
            .with_timeouts(Timeouts::new(Duration::ZERO, DEFAULT_CONNECT_TIMEOUT));
        let error = config.validated().expect_err("零超时没有意义");
        assert_eq!(error.kind(), ConfigErrorKind::Timeout);
    }

    #[test]
    fn base_url_path_prefix_is_preserved() {
        let config = ClientConfig::new("https://vault.example:8443/vault/")
            .validated()
            .expect("带前缀的地址可用");
        assert_eq!(config.base_url().path(), "/vault/");
        assert_eq!(
            config.base_url().as_str(),
            "https://vault.example:8443/vault/"
        );
    }

    #[test]
    fn tls_trust_defaults_to_system_roots_and_never_disables_verification() {
        let config = ClientConfig::new("https://vault.example");
        assert_eq!(config.trust(), &TlsTrust::system_roots());
        assert!(config.trust().additional_roots().is_empty());

        let with_ca =
            ClientConfig::new("https://vault.example").with_trust(TlsTrust::system_roots_with_pem(
                "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
            ));
        assert_eq!(with_ca.trust().additional_roots().len(), 1);
        // Debug 只报告数量，不打印 PEM 内容。
        assert!(!format!("{:?}", with_ca.trust()).contains("BEGIN CERTIFICATE"));
    }
}
