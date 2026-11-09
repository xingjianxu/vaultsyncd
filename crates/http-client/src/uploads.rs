//! 上传会话端点：把契约 `uploads` 端点装配成 `crates/transfer` 的上传端口（design §13.1—13.2）。
//!
//! 本模块只做“路径 + 请求头 + 请求体 + 响应解码”和错误分类，不判断续传策略：缺哪些块、什么时候
//! finalize 由 [`vaultsyncd_transfer::upload`] 决定。因此这里的每个方法都对应一个真实端点，测试可以
//! 用 [`FakeTransport`](crate::testing::FakeTransport) 逐字核对接线形状。
//!
//! # 端点
//!
//! | 方法 | 端点 |
//! | --- | --- |
//! | [`UploadTarget::begin`] | `POST /v1/vaults/{vault}/uploads` |
//! | [`UploadTarget::status`] | `GET /v1/vaults/{vault}/uploads/{upload}` |
//! | [`UploadTarget::put_part`] | `PUT /v1/vaults/{vault}/uploads/{upload}/parts/{index}` |
//! | [`UploadTarget::finalize`] | `POST /v1/vaults/{vault}/uploads/{upload}/finalize` |
//!
//! # 不变量
//!
//! - **块摘要一定要发**：`PUT` 带 `x-part-sha256`（64 位小写十六进制）。服务端在写盘前核对它，
//!   所以“发出去的字节”和“声明的内容”不可能各说各话。
//! - **身份只接受 32 位小写十六进制**：Vault 与上传会话身份来自服务端；拼进 URL 之前先校验形态，
//!   避免把任意文本（例如 `../`）拼进路径。
//! - **错误分类保留语义**：可重试的暂时失败、服务端拒绝（含 `PART_CONFLICT`/`HASH_MISMATCH`）、
//!   需要重新登录与协议错误分成不同的 [`PortError`]，调度器据此决定退避、重新登录还是上报。
//! - **finalize 的结果不是版本**：端点返回的是可提交内容引用，条目 head 由上层用 `OpId` 按 CAS
//!   提交（design §15.2、I01）。

use vaultsyncd_protocol::generated::{
    FinalizedContent as WireFinalizedContent, UploadPartAck, UploadRequest, UploadSession,
    UploadState, UploadStatus,
};
use vaultsyncd_transfer::bitmap::PartRange;
use vaultsyncd_transfer::hash::{DIGEST_BYTES, from_hex, to_hex};
use vaultsyncd_transfer::ports::{
    FinalizedContent, PartAck, PortError, PortFuture, ReceivedPart, SessionSpec,
    UploadProgressInfo, UploadSessionInfo, UploadSessionState, UploadTarget,
};

use crate::client::{ApiClient, ApiRequest, AuthMode};
use crate::error::ClientError;
use crate::transport::Transport;

/// 块摘要请求头（服务端在写盘前核对它）。
pub const PART_DIGEST_HEADER: &str = "x-part-sha256";

/// 一个 Vault 下的上传会话端点集合。
///
/// 克隆共享同一个 [`ApiClient`]（同一个会话与连接池）。
pub struct UploadEndpoint<T: Transport> {
    client: ApiClient<T>,
    vault: String,
}

impl<T: Transport> Clone for UploadEndpoint<T> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            vault: self.vault.clone(),
        }
    }
}

impl<T: Transport> UploadEndpoint<T> {
    /// 绑定一个 Vault；身份必须是 32 位小写十六进制。
    ///
    /// # Errors
    ///
    /// Vault 身份形态不对时返回 [`ClientError::InvalidRequest`]（在发出任何请求之前就拒绝）。
    pub fn new(client: ApiClient<T>, vault: impl Into<String>) -> Result<Self, ClientError> {
        let vault = vault.into();
        if !is_identity(&vault) {
            return Err(ClientError::invalid_request(
                "Vault 身份必须是 32 位小写十六进制",
            ));
        }
        Ok(Self { client, vault })
    }

    /// 底层客户端（继续发别的请求时复用同一个会话）。
    pub fn client(&self) -> &ApiClient<T> {
        &self.client
    }

    /// 绑定的 Vault 身份。
    pub fn vault(&self) -> &str {
        &self.vault
    }

    fn uploads_path(&self) -> String {
        format!("/v1/vaults/{}/uploads", self.vault)
    }

    fn upload_path(&self, upload: &str) -> Result<String, PortError> {
        if !is_identity(upload) {
            return Err(PortError::protocol("上传会话身份必须是 32 位小写十六进制"));
        }
        Ok(format!("{}/{}", self.uploads_path(), upload))
    }
}

impl<T: Transport> UploadTarget for UploadEndpoint<T> {
    fn begin<'a>(
        &'a self,
        spec: &'a SessionSpec,
    ) -> PortFuture<'a, Result<UploadSessionInfo, PortError>> {
        Box::pin(async move {
            let body = UploadRequest {
                chunk_size: spec.chunk_size.to_string(),
                hash: to_hex(&spec.digest),
                total_size: spec.total_size.to_string(),
            };
            let request = ApiRequest::post(self.uploads_path())
                .with_auth(AuthMode::Bearer)
                .with_json(&body)
                .map_err(map_error)?;
            let session: UploadSession = self.client.send_json(request).await.map_err(map_error)?;
            session_info(session)
        })
    }

    fn status<'a>(
        &'a self,
        session: &'a str,
    ) -> PortFuture<'a, Result<UploadProgressInfo, PortError>> {
        Box::pin(async move {
            let path = self.upload_path(session)?;
            let request = ApiRequest::get(path).with_auth(AuthMode::Bearer);
            let status: UploadStatus = self.client.send_json(request).await.map_err(map_error)?;
            progress_info(status)
        })
    }

    fn put_part<'a>(
        &'a self,
        session: &'a str,
        index: u64,
        digest: &'a [u8; DIGEST_BYTES],
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<PartAck, PortError>> {
        Box::pin(async move {
            let path = format!("{}/parts/{index}", self.upload_path(session)?);
            let request = ApiRequest::new(crate::transport::Method::Put, path)
                .with_auth(AuthMode::Bearer)
                .with_header(PART_DIGEST_HEADER, to_hex(digest))
                .with_body("application/octet-stream", bytes.to_vec());
            let ack: UploadPartAck = self.client.send_json(request).await.map_err(map_error)?;
            let ack = PartAck {
                index: parse_u64(&ack.index, "块索引")?,
                length: parse_u64(&ack.length, "块长度")?,
                digest: parse_digest(&ack.hash)?,
                already_present: ack.already_present,
            };
            Ok(ack)
        })
    }

    fn finalize<'a>(
        &'a self,
        session: &'a str,
    ) -> PortFuture<'a, Result<FinalizedContent, PortError>> {
        Box::pin(async move {
            let path = format!("{}/finalize", self.upload_path(session)?);
            let request = ApiRequest::post(path).with_auth(AuthMode::Bearer);
            let finalized: WireFinalizedContent =
                self.client.send_json(request).await.map_err(map_error)?;
            Ok(FinalizedContent {
                digest: parse_digest(&finalized.hash)?,
                size: parse_u64(&finalized.size, "内容大小")?,
                part_count: parse_u64(&finalized.part_count, "块数")?,
                replayed: finalized.replayed,
                storage_path: finalized.storage_path,
            })
        })
    }
}

fn session_info(session: UploadSession) -> Result<UploadSessionInfo, PortError> {
    Ok(UploadSessionInfo {
        id: require_identity(&session.upload_id, "上传会话身份")?,
        chunk_size: parse_u64(&session.chunk_size, "块大小")?,
        part_count: parse_u64(&session.part_count, "块数")?,
        total_size: parse_u64(&session.total_size, "总大小")?,
        digest: parse_digest(&session.hash)?,
        state: session_state(session.state),
    })
}

fn progress_info(status: UploadStatus) -> Result<UploadProgressInfo, PortError> {
    let session = session_info(status.upload)?;
    let mut received = Vec::with_capacity(status.received.len());
    for part in status.received {
        received.push(ReceivedPart {
            index: parse_u64(&part.index, "块索引")?,
            length: parse_u64(&part.length, "块长度")?,
            digest: parse_digest(&part.hash)?,
        });
    }
    let mut missing = Vec::with_capacity(status.missing.len());
    for range in status.missing {
        let first_index = parse_u64(&range.first_index, "缺失区间起点")?;
        let count = parse_u64(&range.count, "缺失区间块数")?;
        let range = PartRange::new(first_index, count)
            .ok_or_else(|| PortError::protocol("服务端报了块数为 0 的缺失区间"))?;
        missing.push(range);
    }
    Ok(UploadProgressInfo {
        session,
        received,
        missing,
    })
}

fn session_state(state: UploadState) -> UploadSessionState {
    match state {
        UploadState::Open => UploadSessionState::Open,
        UploadState::Complete => UploadSessionState::Complete,
        UploadState::Finalized => UploadSessionState::Finalized,
        UploadState::Aborted => UploadSessionState::Aborted,
    }
}

/// 把客户端错误分类成端口错误：只有暂时性失败才可重试，403/冲突/协议错误不重试。
pub(crate) fn map_error(error: ClientError) -> PortError {
    match error {
        ClientError::Cancelled => PortError::Cancelled,
        ClientError::Timeout { .. } => PortError::unavailable("请求超时", None),
        ClientError::Transport(inner) => {
            if inner.kind().is_retryable() {
                PortError::unavailable(inner.message().to_owned(), None)
            } else {
                PortError::protocol(format!("传输失败（{}）：{}", inner.kind(), inner.message()))
            }
        }
        ClientError::UnexpectedResponse { status, message } => {
            if status >= 500 {
                PortError::unavailable(message, None)
            } else {
                PortError::protocol(format!("HTTP {status}：{message}"))
            }
        }
        ClientError::Api(failure) => {
            let detail = failure.body().message.clone();
            if failure.body().retryable {
                PortError::unavailable(detail, failure.retry_after())
            } else {
                PortError::rejected(code_text(&failure), detail)
            }
        }
        ClientError::ReauthRequired { reason, .. } => PortError::ReauthRequired {
            detail: format!("需要重新登录（{reason:?}）"),
        },
        ClientError::NotAuthenticated => PortError::ReauthRequired {
            detail: "本地没有可用的会话凭据".to_owned(),
        },
        ClientError::InvalidRequest { message } => PortError::protocol(message),
        ClientError::Protocol(inner) => PortError::protocol(inner.message().to_owned()),
    }
}

/// 契约里的错误码文本（`ErrorCode` 只有 serde 表示，没有 `as_str`）。
fn code_text(failure: &crate::error::ApiFailure) -> String {
    serde_json::to_value(failure.body().code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "UNKNOWN".to_owned())
}

/// 规范十进制长整数；不是规范十进制就是协议错误（不猜、不截断）。
pub(crate) fn parse_u64(text: &str, field: &str) -> Result<u64, PortError> {
    text.parse::<u64>()
        .map_err(|_| PortError::protocol(format!("{field}不是规范十进制整数：{text}")))
}

/// 64 位小写十六进制摘要。
pub(crate) fn parse_digest(text: &str) -> Result<[u8; DIGEST_BYTES], PortError> {
    from_hex(text).map_err(|error| PortError::protocol(format!("摘要不合法：{error}")))
}

/// 32 位小写十六进制身份（Vault、会话、条目、版本）。
pub(crate) fn is_identity(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 身份形态不对时给出协议错误。
pub(crate) fn require_identity(text: &str, field: &str) -> Result<String, PortError> {
    if is_identity(text) {
        Ok(text.to_owned())
    } else {
        Err(PortError::protocol(format!(
            "{field}必须是 32 位小写十六进制：{text}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ApiClient;
    use crate::config::ClientConfig;
    use crate::error::CertificateProblem;
    use crate::error::{TransportError, TransportErrorKind};
    use crate::testing::{FakeTransport, error_response, issued, json_response};
    use crate::transport::{TransportRequest, TransportResponse};
    use std::sync::Arc;
    use vaultsyncd_protocol::generated::{
        ErrorCode, MissingRange, ObjectReuse, ReceivedPart as WireReceivedPart,
    };
    use vaultsyncd_transfer::hash::digest_of;

    const VAULT: &str = "11111111111111111111111111111111";
    const UPLOAD: &str = "22222222222222222222222222222222";

    fn endpoint<F>(handler: F) -> (UploadEndpoint<FakeTransport>, Arc<FakeTransport>)
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
            UploadEndpoint::new(client, VAULT).expect("Vault 身份"),
            transport,
        )
    }

    fn wire_session(state: UploadState) -> UploadSession {
        UploadSession {
            chunk_size: "4194304".to_owned(),
            expires_at: "1700000000".to_owned(),
            hash: to_hex(&digest_of(b"abc")),
            part_count: "1".to_owned(),
            vault_id: VAULT.to_owned(),
            state,
            total_size: "3".to_owned(),
            upload_id: UPLOAD.to_owned(),
        }
    }

    #[test]
    fn vault_identity_is_validated_before_any_request() {
        let transport = Arc::new(FakeTransport::new(|_request: TransportRequest| {
            Box::pin(async move { panic!("不该发请求") })
        }));
        let client = ApiClient::with_transport_for_unit_tests(
            ClientConfig::new("https://vault.example"),
            transport.clone(),
        )
        .expect("客户端配置");
        assert!(UploadEndpoint::new(client.clone(), "../../etc").is_err());
        assert!(UploadEndpoint::new(client.clone(), "ABCDEF").is_err());
        assert!(UploadEndpoint::new(client, VAULT).is_ok());
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn begin_posts_the_snapshot_facts_and_decodes_the_session() {
        let (endpoint, transport) = endpoint(|request: TransportRequest| {
            Box::pin(async move {
                assert_eq!(request.method(), crate::transport::Method::Post);
                assert_eq!(
                    request.url(),
                    format!("https://vault.example/v1/vaults/{VAULT}/uploads")
                );
                assert_eq!(request.header("authorization"), Some("Bearer a"));
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().expect("请求体")).expect("JSON");
                assert_eq!(body["total_size"], "3");
                assert_eq!(body["chunk_size"], "4194304");
                assert_eq!(body["hash"], to_hex(&digest_of(b"abc")));
                Ok(json_response(201, &wire_session(UploadState::Open)))
            })
        });
        let spec = SessionSpec {
            total_size: 3,
            digest: digest_of(b"abc"),
            chunk_size: 4 * 1024 * 1024,
        };
        let info = endpoint.begin(&spec).await.expect("会话");
        assert_eq!(info.id, UPLOAD);
        assert_eq!(info.chunk_size, 4 * 1024 * 1024);
        assert_eq!(info.part_count, 1);
        assert_eq!(info.state, UploadSessionState::Open);
        assert_eq!(transport.call_count(), 1);
    }

    #[tokio::test]
    async fn status_maps_received_parts_and_missing_ranges() {
        let (endpoint, _transport) = endpoint(|request: TransportRequest| {
            Box::pin(async move {
                assert_eq!(request.method(), crate::transport::Method::Get);
                assert_eq!(
                    request.url(),
                    format!("https://vault.example/v1/vaults/{VAULT}/uploads/{UPLOAD}")
                );
                Ok(json_response(
                    200,
                    &UploadStatus {
                        missing: vec![MissingRange {
                            count: "2".to_owned(),
                            first_index: "1".to_owned(),
                        }],
                        received: vec![WireReceivedPart {
                            hash: to_hex(&digest_of(b"abc")),
                            index: "0".to_owned(),
                            length: "3".to_owned(),
                        }],
                        upload: wire_session(UploadState::Open),
                    },
                ))
            })
        });
        let info = endpoint.status(UPLOAD).await.expect("会话状态");
        assert_eq!(info.received.len(), 1);
        assert_eq!(info.received[0].index, 0);
        assert_eq!(info.received[0].digest, digest_of(b"abc"));
        assert_eq!(
            info.missing,
            vec![PartRange {
                first_index: 1,
                count: 2
            }]
        );
    }

    #[tokio::test]
    async fn put_part_sends_the_block_digest_header_and_octets() {
        let (endpoint, _transport) = endpoint(|request: TransportRequest| {
            Box::pin(async move {
                assert_eq!(request.method(), crate::transport::Method::Put);
                assert!(
                    request
                        .url()
                        .ends_with(&format!("/v1/vaults/{VAULT}/uploads/{UPLOAD}/parts/7")),
                    "{}",
                    request.url()
                );
                assert_eq!(
                    request.header(PART_DIGEST_HEADER),
                    Some(to_hex(&digest_of(b"hello")).as_str())
                );
                assert_eq!(
                    request.header("content-type"),
                    Some("application/octet-stream")
                );
                assert_eq!(request.body(), Some(&b"hello"[..]));
                Ok(json_response(
                    200,
                    &UploadPartAck {
                        already_present: true,
                        hash: to_hex(&digest_of(b"hello")),
                        index: "7".to_owned(),
                        length: "5".to_owned(),
                        state: UploadState::Open,
                        upload_id: UPLOAD.to_owned(),
                    },
                ))
            })
        });
        let digest = digest_of(b"hello");
        let ack = endpoint
            .put_part(UPLOAD, 7, &digest, b"hello")
            .await
            .expect("块登记");
        assert_eq!(ack.index, 7);
        assert_eq!(ack.length, 5);
        assert!(ack.already_present);
        assert_eq!(ack.digest, digest);
    }

    #[tokio::test]
    async fn finalize_returns_a_content_reference_not_a_commit() {
        let (endpoint, _transport) = endpoint(|request: TransportRequest| {
            Box::pin(async move {
                assert_eq!(request.method(), crate::transport::Method::Post);
                assert!(request.url().ends_with("/finalize"), "{}", request.url());
                assert!(request.body().is_none(), "finalize 没有请求体");
                Ok(json_response(
                    200,
                    &WireFinalizedContent {
                        hash: to_hex(&digest_of(b"abc")),
                        part_count: "1".to_owned(),
                        replayed: false,
                        reuse: ObjectReuse::FreshlyPublished,
                        vault_id: VAULT.to_owned(),
                        size: "3".to_owned(),
                        storage_path: "objects/ab/cd".to_owned(),
                        upload_id: UPLOAD.to_owned(),
                    },
                ))
            })
        });
        let finalized = endpoint.finalize(UPLOAD).await.expect("finalize");
        assert_eq!(finalized.digest, digest_of(b"abc"));
        assert_eq!(finalized.size, 3);
        assert_eq!(finalized.part_count, 1);
        assert!(!finalized.replayed);
        assert_eq!(finalized.storage_path, "objects/ab/cd");
    }

    #[tokio::test]
    async fn server_rejections_are_not_retryable_but_throttling_is() {
        let (endpoint, _transport) = endpoint(|request: TransportRequest| {
            Box::pin(async move {
                if request.url().contains("/parts/1") {
                    Ok(error_response(
                        409,
                        ErrorCode::PartConflict,
                        "同一索引不同字节",
                        false,
                    ))
                } else {
                    Ok(error_response(429, ErrorCode::RateLimited, "太频繁", true)
                        .with_header("retry-after", "3"))
                }
            })
        });
        let digest = digest_of(b"x");
        let conflict = endpoint
            .put_part(UPLOAD, 1, &digest, b"x")
            .await
            .expect_err("冲突");
        assert_eq!(conflict.code(), Some("PART_CONFLICT"));
        assert!(!conflict.is_retryable());

        let throttled = endpoint
            .put_part(UPLOAD, 2, &digest, b"x")
            .await
            .expect_err("限流");
        assert!(throttled.is_retryable());
        match throttled {
            PortError::Unavailable { retry_after, .. } => {
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(3)));
            }
            other => panic!("期望 Unavailable，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn certificate_failures_are_not_retryable() {
        let (endpoint, _transport) = endpoint(|_request: TransportRequest| {
            Box::pin(async move {
                Err(TransportError::new(
                    TransportErrorKind::Certificate(CertificateProblem::UnknownIssuer),
                    "未知签发者",
                ))
            })
        });
        let error = endpoint.status(UPLOAD).await.expect_err("证书不受信任");
        assert!(!error.is_retryable());
        assert!(matches!(error, PortError::Protocol { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn malformed_decimal_text_is_a_protocol_error() {
        // 生成的 DTO 在序列化时就拒绝非规范十进制，因此这里直接给原始 JSON 响应体。
        let body = format!(
            r#"{{"chunk_size":"4","expires_at":"1700000000","hash":"{}","part_count":"1.0","vault_id":"{VAULT}","state":"open","total_size":"3","upload_id":"{UPLOAD}"}}"#,
            to_hex(&digest_of(b"abc"))
        );
        let (endpoint, _transport) = endpoint(move |_request: TransportRequest| {
            let body = body.clone();
            Box::pin(async move {
                Ok(TransportResponse::new(
                    201,
                    vec![("content-type".to_owned(), "application/json".to_owned())],
                    body.into_bytes(),
                ))
            })
        });
        let spec = SessionSpec {
            total_size: 3,
            digest: digest_of(b"abc"),
            chunk_size: 4,
        };
        let error = endpoint.begin(&spec).await.expect_err("非规范十进制");
        assert!(matches!(error, PortError::Protocol { .. }), "{error:?}");
        assert!(!error.is_retryable());
    }

    #[tokio::test]
    async fn an_invalid_upload_identity_never_reaches_the_transport() {
        let (endpoint, transport) =
            endpoint(|_request: TransportRequest| Box::pin(async move { panic!("不该发请求") }));
        assert!(endpoint.status("not-hex").await.is_err());
        assert_eq!(transport.call_count(), 0);
    }
}
