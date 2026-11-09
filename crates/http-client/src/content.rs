//! 固定版本内容端点：按 `(entry, version)` 取字节，支持区间续传（design §13.1—13.2）。
//!
//! 只有一条路径：`GET /v1/vaults/{vault}/entries/{entry}/versions/{version}/content`，带
//! `Range: bytes=start-end`。请求里**没有**“当前内容”的位置，因此不可能把另一个版本的字节
//! 当成这个版本；服务端返回的 `Content-Range` 与 `x-content-sha256` 都会核对，对不上就报错，
//! 由下载执行器决定不安装。
//!
//! # 不变量
//!
//! - **版本固定**：路径只接受 32 位小写十六进制的条目与版本身份，杜绝把 `../` 之类拼进 URL。
//! - **长度必须一致**：返回的字节数必须等于请求的区间长度，短读不补零、不猜测。
//! - **区间与总长核对**：响应带 `Content-Range` 时，起点、终点与总长度都要与请求和固定版本一致；
//!   整份响应（200）只接受“从 0 开始且长度等于版本大小”的那种。
//! - **整份摘要不吻合立即失败**：响应带 `x-content-sha256` 且与固定版本摘要不同，说明服务端给了
//!   另一份内容，直接按协议错误拒绝，不用把整份内容下完才发现。
//! - **不做跨版本增量**：没有“复用旧版本块/按块寻址”的入口，只有固定版本 + 字节区间。

use vaultsyncd_transfer::ports::{PortError, PortFuture, VersionPin, VersionedContent};

use crate::client::{ApiClient, ApiRequest, AuthMode};
use crate::error::ClientError;
use crate::transport::Transport;
use crate::uploads::{is_identity, map_error, parse_digest, parse_u64};

/// 服务端在内容响应里回显的整份摘要头。
pub const CONTENT_DIGEST_HEADER: &str = "x-content-sha256";

/// 一个 Vault 下的固定版本内容端点。
pub struct ContentEndpoint<T: Transport> {
    client: ApiClient<T>,
    vault: String,
}

impl<T: Transport> Clone for ContentEndpoint<T> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            vault: self.vault.clone(),
        }
    }
}

impl<T: Transport> ContentEndpoint<T> {
    /// 绑定一个 Vault；身份必须是 32 位小写十六进制。
    ///
    /// # Errors
    ///
    /// Vault 身份形态不对时返回 [`ClientError::InvalidRequest`]。
    pub fn new(client: ApiClient<T>, vault: impl Into<String>) -> Result<Self, ClientError> {
        let vault = vault.into();
        if !is_identity(&vault) {
            return Err(ClientError::invalid_request(
                "Vault 身份必须是 32 位小写十六进制",
            ));
        }
        Ok(Self { client, vault })
    }

    /// 底层客户端。
    pub fn client(&self) -> &ApiClient<T> {
        &self.client
    }

    /// 绑定的 Vault 身份。
    pub fn vault(&self) -> &str {
        &self.vault
    }

    fn content_path(&self, pin: &VersionPin) -> Result<String, PortError> {
        if !is_identity(&pin.entry) {
            return Err(PortError::protocol("条目身份必须是 32 位小写十六进制"));
        }
        if !is_identity(&pin.version) {
            return Err(PortError::protocol("版本身份必须是 32 位小写十六进制"));
        }
        Ok(format!(
            "/v1/vaults/{}/entries/{}/versions/{}/content",
            self.vault, pin.entry, pin.version
        ))
    }
}

impl<T: Transport> VersionedContent for ContentEndpoint<T> {
    fn read_range<'a>(
        &'a self,
        pin: &'a VersionPin,
        offset: u64,
        length: u64,
    ) -> PortFuture<'a, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            if length == 0 {
                return Err(PortError::protocol("不允许请求 0 字节区间"));
            }
            let last = offset
                .checked_add(length - 1)
                .ok_or_else(|| PortError::protocol("区间终点溢出"))?;
            if last >= pin.size {
                return Err(PortError::protocol(format!(
                    "区间 {offset}-{last} 超出固定版本大小 {}",
                    pin.size
                )));
            }
            let path = self.content_path(pin)?;
            let request = ApiRequest::get(path)
                .with_auth(AuthMode::Bearer)
                .with_header("range", format!("bytes={offset}-{last}"));
            let response = self.client.send(request).await.map_err(map_error)?;
            let status = response.status();
            let body = response.body();

            if let Some(text) = response.header("content-range") {
                let range = parse_content_range(text)?;
                if range.first != offset || range.length() != length || range.total != pin.size {
                    return Err(PortError::protocol(format!(
                        "Content-Range 与请求不符：{text}（请求 {offset}+{length}，版本 {} 字节）",
                        pin.size
                    )));
                }
            } else if status == 200 {
                // 整份响应：只接受从 0 开始、长度等于版本大小的那一种。
                if offset != 0 || length != pin.size {
                    return Err(PortError::protocol(
                        "整份响应用来回答部分区间请求，长度与请求不符",
                    ));
                }
            } else {
                return Err(PortError::protocol("区间响应缺少 Content-Range"));
            }

            if let Some(text) = response.header(CONTENT_DIGEST_HEADER) {
                let digest = parse_digest(text)?;
                if digest != pin.digest {
                    return Err(PortError::protocol(format!(
                        "服务端内容摘要 {text} 与固定版本摘要不符"
                    )));
                }
            }
            if body.len() as u64 != length {
                return Err(PortError::protocol(format!(
                    "区间 {offset}+{length} 实际返回 {} 字节",
                    body.len()
                )));
            }
            Ok(body.to_vec())
        })
    }
}

impl<T: Transport> std::fmt::Debug for ContentEndpoint<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentEndpoint")
            .field("vault", &self.vault)
            .finish_non_exhaustive()
    }
}

/// `Content-Range: bytes start-end/total` 的解析结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentRange {
    /// 起始偏移（含）。
    pub first: u64,
    /// 结束偏移（含）。
    pub last: u64,
    /// 对象总字节数。
    pub total: u64,
}

impl ContentRange {
    /// 区间字节数。
    #[must_use]
    pub fn length(self) -> u64 {
        self.last.saturating_sub(self.first) + 1
    }
}

/// 解析 `bytes start-end/total`。
///
/// # Errors
///
/// 前缀不是 `bytes `、三段不是规范十进制、起点大于终点时返回协议错误。
pub fn parse_content_range(text: &str) -> Result<ContentRange, PortError> {
    let invalid = || PortError::protocol(format!("Content-Range 不合契约：{text}"));
    let rest = text.trim().strip_prefix("bytes ").ok_or_else(invalid)?;
    let (span, total) = rest.split_once('/').ok_or_else(invalid)?;
    let (first, last) = span.split_once('-').ok_or_else(invalid)?;
    let range = ContentRange {
        first: parse_u64(first, "区间起点")?,
        last: parse_u64(last, "区间终点")?,
        total: parse_u64(total, "对象总长度")?,
    };
    if range.first > range.last {
        return Err(invalid());
    }
    Ok(range)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ApiClient;
    use crate::config::ClientConfig;
    use crate::error::TransportError;
    use crate::testing::{FakeTransport, error_response, issued};
    use crate::transport::{TransportRequest, TransportResponse};
    use std::sync::Arc;
    use vaultsyncd_protocol::generated::ErrorCode;
    use vaultsyncd_transfer::hash::{digest_of, to_hex};

    const VAULT: &str = "11111111111111111111111111111111";
    const ENTRY: &str = "33333333333333333333333333333333";
    const VERSION: &str = "44444444444444444444444444444444";
    const CONTENT: &[u8] = b"0123456789";

    fn pin() -> VersionPin {
        VersionPin::new("binding", ENTRY, VERSION, digest_of(CONTENT), 10).expect("固定版本")
    }

    fn endpoint<F>(handler: F) -> (ContentEndpoint<FakeTransport>, Arc<FakeTransport>)
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
        let client = ApiClient::with_transport_for_unit_tests(
            ClientConfig::new("https://vault.example"),
            transport.clone(),
        )
        .expect("客户端配置");
        client
            .auth()
            .install(crate::Credentials::from_issued(&issued("a", "r")).expect("凭据"));
        (
            ContentEndpoint::new(client, VAULT).expect("Vault"),
            transport,
        )
    }

    fn range_response(first: u64, last: u64, total: u64, body: Vec<u8>) -> TransportResponse {
        TransportResponse::new(
            206,
            vec![
                (
                    "content-range".to_owned(),
                    format!("bytes {first}-{last}/{total}"),
                ),
                (
                    CONTENT_DIGEST_HEADER.to_owned(),
                    to_hex(&digest_of(CONTENT)),
                ),
            ],
            body,
        )
    }

    #[tokio::test]
    async fn a_range_request_is_bound_to_the_fixed_version() {
        let (endpoint, _transport) = endpoint(|request: TransportRequest| {
            Box::pin(async move {
                assert_eq!(request.method(), crate::transport::Method::Get);
                assert_eq!(
                    request.url(),
                    format!(
                        "https://vault.example/v1/vaults/{VAULT}/entries/{ENTRY}/versions/{VERSION}/content"
                    )
                );
                assert_eq!(request.header("range"), Some("bytes=2-5"));
                assert_eq!(request.header("authorization"), Some("Bearer a"));
                Ok(range_response(2, 5, 10, CONTENT[2..6].to_vec()))
            })
        });
        let bytes = endpoint.read_range(&pin(), 2, 4).await.expect("区间内容");
        assert_eq!(bytes, b"2345");
    }

    #[tokio::test]
    async fn a_full_200_response_is_accepted_only_for_the_whole_version() {
        let (endpoint, _transport) = endpoint(|_request: TransportRequest| {
            Box::pin(async move {
                Ok(TransportResponse::new(
                    200,
                    vec![(
                        CONTENT_DIGEST_HEADER.to_owned(),
                        to_hex(&digest_of(CONTENT)),
                    )],
                    CONTENT.to_vec(),
                ))
            })
        });
        let bytes = endpoint.read_range(&pin(), 0, 10).await.expect("整份内容");
        assert_eq!(bytes, CONTENT);
        let error = endpoint
            .read_range(&pin(), 2, 4)
            .await
            .expect_err("整份响应不能回答部分区间");
        assert!(matches!(error, PortError::Protocol { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_content_range_that_does_not_match_is_rejected() {
        let (endpoint, _transport) = endpoint(|_request: TransportRequest| {
            Box::pin(async move { Ok(range_response(4, 7, 10, CONTENT[4..8].to_vec())) })
        });
        let error = endpoint
            .read_range(&pin(), 2, 4)
            .await
            .expect_err("起点不符");
        assert!(matches!(error, PortError::Protocol { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_wrong_total_length_is_rejected() {
        let (endpoint, _transport) = endpoint(|_request: TransportRequest| {
            Box::pin(async move { Ok(range_response(2, 5, 11, CONTENT[2..6].to_vec())) })
        });
        let error = endpoint
            .read_range(&pin(), 2, 4)
            .await
            .expect_err("总长度不符");
        assert!(matches!(error, PortError::Protocol { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_short_body_is_an_error() {
        let (endpoint, _transport) = endpoint(|_request: TransportRequest| {
            Box::pin(async move { Ok(range_response(2, 5, 10, CONTENT[2..5].to_vec())) })
        });
        let error = endpoint.read_range(&pin(), 2, 4).await.expect_err("短读");
        assert!(matches!(error, PortError::Protocol { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn another_version_digest_is_refused_before_using_the_bytes() {
        let (endpoint, _transport) = endpoint(|_request: TransportRequest| {
            Box::pin(async move {
                Ok(TransportResponse::new(
                    206,
                    vec![
                        ("content-range".to_owned(), "bytes 0-9/10".to_owned()),
                        (
                            CONTENT_DIGEST_HEADER.to_owned(),
                            to_hex(&digest_of(b"xxxx")),
                        ),
                    ],
                    CONTENT.to_vec(),
                ))
            })
        });
        let error = endpoint
            .read_range(&pin(), 0, 10)
            .await
            .expect_err("另一个版本");
        assert!(matches!(error, PortError::Protocol { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_gone_version_is_reported_as_a_rejection() {
        let (endpoint, _transport) = endpoint(|_request: TransportRequest| {
            Box::pin(async move {
                Ok(error_response(
                    410,
                    ErrorCode::VersionGone,
                    "版本已清理",
                    false,
                ))
            })
        });
        let error = endpoint
            .read_range(&pin(), 0, 4)
            .await
            .expect_err("版本已清理");
        assert_eq!(error.code(), Some("VERSION_GONE"));
        assert!(!error.is_retryable());
    }

    #[tokio::test]
    async fn a_range_beyond_the_pinned_size_never_reaches_the_server() {
        let (endpoint, transport) =
            endpoint(|_request: TransportRequest| Box::pin(async move { panic!("不该发请求") }));
        assert!(endpoint.read_range(&pin(), 8, 4).await.is_err());
        assert!(endpoint.read_range(&pin(), 0, 0).await.is_err());
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn identities_must_be_lowercase_hex() {
        let (endpoint, transport) =
            endpoint(|_request: TransportRequest| Box::pin(async move { panic!("不该发请求") }));
        let bad = VersionPin::new("binding", "../etc", VERSION, digest_of(CONTENT), 10)
            .expect("形状由端点把关");
        assert!(endpoint.read_range(&bad, 0, 4).await.is_err());
        assert_eq!(transport.call_count(), 0);
    }

    #[test]
    fn content_range_parsing_is_strict() {
        assert_eq!(
            parse_content_range("bytes 0-9/10").expect("合法"),
            ContentRange {
                first: 0,
                last: 9,
                total: 10
            }
        );
        assert_eq!(
            parse_content_range("bytes 0-9/10").expect("合法").length(),
            10
        );
        assert!(parse_content_range("bytes 9-0/10").is_err());
        assert!(parse_content_range("items 0-9/10").is_err());
        assert!(parse_content_range("bytes 0-9").is_err());
        assert!(
            parse_content_range("bytes 00-09/10").is_ok(),
            "前导零是合法十进制"
        );
        assert!(parse_content_range("bytes -1-9/10").is_err());
    }
}
