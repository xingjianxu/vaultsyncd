//! 端到端接线测试：用真实端点模块 + 传输替身跑完一次上传与一次下载（含断线续传）。
//!
//! 这里不引入 HTTP 服务器：`FakeTransport` 就是线上字节的入口，因此被测的仍是**产品代码路径**
//! （`uploads`/`content` 端点 + `transfer` 的执行器），只是把 socket 换成了内存中的契约实现。
//! 它回答两个问题：
//!
//! 1. 客户端发给服务端的请求形状是否足以让服务端区分“缺哪些块”“哪些块不合”；
//! 2. 断线后重跑是否只补缺失块，且下载始终绑在固定 VersionId 上。

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use vaultsyncd_protocol::generated::{
    FinalizedContent, MissingRange, ObjectReuse, ReceivedPart, UploadPartAck, UploadRequest,
    UploadSession, UploadState, UploadStatus,
};
use vaultsyncd_transfer::bitmap::PartBitmap;
use vaultsyncd_transfer::chunk::ChunkGeometry;
use vaultsyncd_transfer::download::{self, DownloadOptions};
use vaultsyncd_transfer::hash::{DIGEST_BYTES, digest_of, to_hex};
use vaultsyncd_transfer::ports::{
    DownloadKey, DownloadProgressStore, LocalErrorKind, PortError, PortFuture, SessionSpec,
    SnapshotSource, StoredProgress, TempContent, VersionPin,
};
use vaultsyncd_transfer::upload::{self, UploadOptions};

use crate::client::ApiClient;
use crate::config::ClientConfig;
use crate::content::ContentEndpoint;
use crate::testing::{FakeTransport, error_response, issued, json_response};
use crate::transport::{TransportRequest, TransportResponse};
use crate::uploads::UploadEndpoint;

const ROOT: &str = "11111111111111111111111111111111";
const ENTRY: &str = "33333333333333333333333333333333";
const VERSION: &str = "44444444444444444444444444444444";

/// 内存服务端：只实现本次流程用到的端点，响应体是契约 DTO。
#[derive(Default)]
struct Server {
    inner: Mutex<ServerState>,
}

#[derive(Default)]
struct ServerState {
    sessions: BTreeMap<String, SessionState>,
    next_session: u64,
    /// 该索引的上传第一次返回“连接被重置”。
    drop_put_once_at: Option<u64>,
    /// 该索引的回执第一次给错摘要（服务端收下了别的字节）。
    wrong_ack_once_at: Option<u64>,
    puts: Vec<(String, u64)>,
    content_reads: Vec<(String, u64, u64)>,
    finalized: usize,
}

struct SessionState {
    spec: SessionSpec,
    parts: BTreeMap<u64, Vec<u8>>,
    state: UploadState,
}

impl Server {
    fn drop_put_once(&self, index: u64) {
        self.inner.lock().expect("服务端状态").drop_put_once_at = Some(index);
    }

    fn wrong_ack_once(&self, index: u64) {
        self.inner.lock().expect("服务端状态").wrong_ack_once_at = Some(index);
    }

    fn puts(&self) -> Vec<(String, u64)> {
        self.inner.lock().expect("服务端状态").puts.clone()
    }

    fn content_reads(&self) -> Vec<(String, u64, u64)> {
        self.inner.lock().expect("服务端状态").content_reads.clone()
    }

    fn finalized(&self) -> usize {
        self.inner.lock().expect("服务端状态").finalized
    }

    fn handle(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, crate::TransportError> {
        let url = request.url().to_owned();
        let body = request.body().map(<[u8]>::to_vec).unwrap_or_default();
        if url.ends_with("/uploads") && request.method() == crate::Method::Post {
            return self.begin(&body);
        }
        if let Some(index) = part_index(&url) {
            return self.put_part(&url, index, &body, request.header("x-part-sha256"));
        }
        if url.ends_with("/finalize") {
            return self.finalize(&url);
        }
        if url.contains("/uploads/") && request.method() == crate::Method::Get {
            return self.status(&url);
        }
        if url.contains("/versions/") && url.ends_with("/content") {
            let range = request.header("range").unwrap_or("").to_owned();
            return self.content(&url, &range);
        }
        panic!("意外端点：{} {}", request.method().as_str(), url);
    }

    fn begin(&self, body: &[u8]) -> Result<TransportResponse, crate::TransportError> {
        let request: UploadRequest = decode(body);
        let spec = SessionSpec {
            total_size: request.total_size.parse().expect("总大小"),
            digest: vaultsyncd_transfer::hash::from_hex(&request.hash).expect("摘要"),
            chunk_size: request.chunk_size.parse().expect("块大小"),
        };
        let mut state = self.inner.lock().expect("服务端状态");
        state.next_session += 1;
        let id = format!("{:032x}", state.next_session);
        let geometry = ChunkGeometry::new(spec.total_size, spec.chunk_size).expect("几何");
        state.sessions.insert(
            id.clone(),
            SessionState {
                spec,
                parts: BTreeMap::new(),
                state: UploadState::Open,
            },
        );
        Ok(json_response(
            201,
            &UploadSession {
                chunk_size: spec.chunk_size.to_string(),
                expires_at: "1700000000".to_owned(),
                hash: to_hex(&spec.digest),
                part_count: geometry.part_count().to_string(),
                root_id: ROOT.to_owned(),
                state: UploadState::Open,
                total_size: spec.total_size.to_string(),
                upload_id: id,
            },
        ))
    }

    fn status(&self, url: &str) -> Result<TransportResponse, crate::TransportError> {
        let id = url.rsplit('/').next().expect("会话身份").to_owned();
        let state = self.inner.lock().expect("服务端状态");
        let Some(session) = state.sessions.get(&id) else {
            return Ok(error_response(
                404,
                vaultsyncd_protocol::generated::ErrorCode::UnsupportedEntry,
                "会话不存在",
                false,
            ));
        };
        let geometry =
            ChunkGeometry::new(session.spec.total_size, session.spec.chunk_size).expect("几何");
        let mut bitmap = PartBitmap::empty(geometry);
        let mut received = Vec::new();
        for (index, bytes) in &session.parts {
            bitmap.mark(*index).expect("块索引");
            received.push(ReceivedPart {
                hash: to_hex(&digest_of(bytes)),
                index: index.to_string(),
                length: (bytes.len() as u64).to_string(),
            });
        }
        let missing: Vec<MissingRange> = bitmap
            .missing()
            .into_iter()
            .map(|range| MissingRange {
                count: range.count.to_string(),
                first_index: range.first_index.to_string(),
            })
            .collect();
        Ok(json_response(
            200,
            &UploadStatus {
                missing,
                received,
                upload: UploadSession {
                    chunk_size: session.spec.chunk_size.to_string(),
                    expires_at: "1700000000".to_owned(),
                    hash: to_hex(&session.spec.digest),
                    part_count: geometry.part_count().to_string(),
                    root_id: ROOT.to_owned(),
                    state: session.state,
                    total_size: session.spec.total_size.to_string(),
                    upload_id: id,
                },
            },
        ))
    }

    fn put_part(
        &self,
        url: &str,
        index: u64,
        body: &[u8],
        declared: Option<&str>,
    ) -> Result<TransportResponse, crate::TransportError> {
        let id = url
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .trim_end_matches("/parts/")
            .rsplit('/')
            .next()
            .expect("会话身份")
            .to_owned();
        let mut state = self.inner.lock().expect("服务端状态");
        state.puts.push((id.clone(), index));
        if state.drop_put_once_at == Some(index) {
            state.drop_put_once_at = None;
            return Ok(error_response(
                503,
                vaultsyncd_protocol::generated::ErrorCode::NeedsRecovery,
                "连接被重置",
                true,
            ));
        }
        let geometry = {
            let session = state.sessions.get(&id).expect("会话");
            ChunkGeometry::new(session.spec.total_size, session.spec.chunk_size).expect("几何")
        };
        let expected_length = geometry.part_length(index).expect("块索引");
        if body.len() as u64 != expected_length {
            return Ok(error_response(
                400,
                vaultsyncd_protocol::generated::ErrorCode::InvalidRequest,
                "块长度不符",
                false,
            ));
        }
        let actual = digest_of(body);
        if declared != Some(to_hex(&actual).as_str()) {
            return Ok(error_response(
                422,
                vaultsyncd_protocol::generated::ErrorCode::HashMismatch,
                "块摘要不符",
                false,
            ));
        }
        let wrong = state.wrong_ack_once_at == Some(index);
        if wrong {
            state.wrong_ack_once_at = None;
        }
        let session = state.sessions.get_mut(&id).expect("会话");
        session.parts.insert(index, body.to_vec());
        Ok(json_response(
            200,
            &UploadPartAck {
                already_present: false,
                hash: if wrong {
                    to_hex(&[0xff; DIGEST_BYTES])
                } else {
                    to_hex(&actual)
                },
                index: index.to_string(),
                length: expected_length.to_string(),
                state: session.state,
                upload_id: id,
            },
        ))
    }

    fn finalize(&self, url: &str) -> Result<TransportResponse, crate::TransportError> {
        let id = url
            .trim_end_matches("/finalize")
            .rsplit('/')
            .next()
            .expect("会话身份")
            .to_owned();
        let mut state = self.inner.lock().expect("服务端状态");
        state.finalized += 1;
        let geometry = {
            let session = state.sessions.get(&id).expect("会话");
            ChunkGeometry::new(session.spec.total_size, session.spec.chunk_size).expect("几何")
        };
        let session = state.sessions.get_mut(&id).expect("会话");
        if session.parts.len() as u64 != geometry.part_count() {
            return Ok(error_response(
                400,
                vaultsyncd_protocol::generated::ErrorCode::InvalidRequest,
                "块未收齐",
                false,
            ));
        }
        let mut content = Vec::new();
        for index in 0..geometry.part_count() {
            content.extend_from_slice(session.parts.get(&index).expect("块"));
        }
        assert_eq!(digest_of(&content), session.spec.digest, "服务端拼装摘要");
        session.state = UploadState::Finalized;
        Ok(json_response(
            200,
            &FinalizedContent {
                hash: to_hex(&session.spec.digest),
                part_count: geometry.part_count().to_string(),
                replayed: false,
                reuse: ObjectReuse::FreshlyPublished,
                root_id: ROOT.to_owned(),
                size: session.spec.total_size.to_string(),
                storage_path: format!("objects/{id}"),
                upload_id: id,
            },
        ))
    }

    fn content(&self, url: &str, range: &str) -> Result<TransportResponse, crate::TransportError> {
        // 固定版本内容就是那一份，服务端不认识别的版本。
        let bytes = content_bytes();
        let text = range.trim().strip_prefix("bytes=").expect("Range 头");
        let (first, last) = text.split_once('-').expect("区间");
        let first: u64 = first.parse().expect("起点");
        let last: u64 = last.parse().expect("终点");
        {
            let mut state = self.inner.lock().expect("服务端状态");
            state
                .content_reads
                .push((VERSION.to_owned(), first, last - first + 1));
        }
        assert!(
            url.contains(&format!("/versions/{VERSION}/content")),
            "{url}"
        );
        let slice = &bytes[first as usize..=last as usize];
        Ok(TransportResponse::new(
            206,
            vec![
                (
                    "content-range".to_owned(),
                    format!("bytes {first}-{last}/{}", bytes.len()),
                ),
                ("x-content-sha256".to_owned(), to_hex(&digest_of(&bytes))),
            ],
            slice.to_vec(),
        ))
    }
}

fn part_index(url: &str) -> Option<u64> {
    let (_, index) = url.split_once("/parts/")?;
    index.parse().ok()
}

fn decode<T: DeserializeOwned>(body: &[u8]) -> T {
    serde_json::from_slice(body).expect("请求体是契约 DTO")
}

fn content_bytes() -> Vec<u8> {
    (0..20_u8).map(|index| index.wrapping_mul(3)).collect()
}

/// 固定快照：一份内存字节。
struct SliceSource {
    bytes: Vec<u8>,
}

impl SnapshotSource for SliceSource {
    fn identity(&self) -> &str {
        "generation-1"
    }

    fn total_size(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            let start = usize::try_from(offset).expect("偏移");
            let end = start + usize::try_from(length).expect("长度");
            self.bytes
                .get(start..end)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| PortError::local(LocalErrorKind::Read, "越界读取"))
        })
    }
}

/// 内存临时文件。
#[derive(Default)]
struct MemoryTemp {
    bytes: Mutex<Vec<u8>>,
    discards: Mutex<usize>,
}

impl TempContent for MemoryTemp {
    fn path(&self) -> &str {
        "temp/version"
    }

    fn ensure_capacity(&self, _needed: u64) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }

    fn write_at<'a>(
        &'a self,
        offset: u64,
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let mut target = self.bytes.lock().expect("临时内容");
            let start = usize::try_from(offset).expect("偏移");
            let end = start + bytes.len();
            if target.len() < end {
                target.resize(end, 0);
            }
            target[start..end].copy_from_slice(bytes);
            Ok(())
        })
    }

    fn read_at(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            let target = self.bytes.lock().expect("临时内容");
            let start = usize::try_from(offset).expect("偏移");
            let end = start + usize::try_from(length).expect("长度");
            target
                .get(start..end)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| PortError::local(LocalErrorKind::Read, "越界读取"))
        })
    }

    fn sync(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }

    fn discard(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            self.bytes.lock().expect("临时内容").clear();
            *self.discards.lock().expect("丢弃计数") += 1;
            Ok(())
        })
    }
}

/// 内存下载进度。
#[derive(Default)]
struct MemoryProgress {
    inner: Mutex<BTreeMap<String, StoredProgress>>,
}

impl DownloadProgressStore for MemoryProgress {
    fn load<'a>(
        &'a self,
        key: &'a DownloadKey,
    ) -> PortFuture<'a, Result<Option<StoredProgress>, PortError>> {
        Box::pin(async move { Ok(self.inner.lock().expect("进度").get(&key.as_key()).cloned()) })
    }

    fn save<'a>(
        &'a self,
        key: &'a DownloadKey,
        progress: &'a StoredProgress,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            self.inner
                .lock()
                .expect("进度")
                .insert(key.as_key(), progress.clone());
            Ok(())
        })
    }

    fn discard<'a>(&'a self, key: &'a DownloadKey) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            self.inner.lock().expect("进度").remove(&key.as_key());
            Ok(())
        })
    }
}

fn client(server: &std::sync::Arc<Server>) -> ApiClient<FakeTransport> {
    let server = std::sync::Arc::clone(server);
    let transport = std::sync::Arc::new(FakeTransport::new(move |request: TransportRequest| {
        let server = std::sync::Arc::clone(&server);
        Box::pin(async move { server.handle(request) })
    }));
    let client = ApiClient::with_transport(ClientConfig::new("https://vault.example"), transport)
        .expect("客户端配置");
    client
        .auth()
        .install(crate::Credentials::from_issued(&issued("a", "r")).expect("凭据"));
    client
}

#[tokio::test]
async fn a_disconnect_through_the_real_endpoints_only_resends_missing_parts() {
    let bytes = content_bytes();
    let server = std::sync::Arc::new(Server::default());
    server.drop_put_once(2);
    let endpoint = UploadEndpoint::new(client(&server), ROOT).expect("上传端点");
    let source = SliceSource {
        bytes: bytes.clone(),
    };
    let options = UploadOptions::new(4).with_part_concurrency(1);

    let failure = upload::run(&source, &endpoint, None, &options)
        .await
        .expect_err("块 2 断线");
    assert!(failure.kind().is_retryable(), "{failure}");
    let resume = failure.resume().expect("可续传").clone();
    let puts_before = server.puts();

    let outcome = upload::run(&source, &endpoint, Some(&resume), &options)
        .await
        .expect("续传成功");
    assert_eq!(outcome.content.digest, digest_of(&bytes));
    assert_eq!(outcome.uploaded_parts + outcome.reused_parts, 5);
    assert_eq!(server.finalized(), 1);

    let mut resent: Vec<u64> = server.puts()[puts_before.len()..]
        .iter()
        .map(|(_, index)| *index)
        .collect();
    resent.sort_unstable();
    resent.dedup();
    assert_eq!(resent, vec![2, 3, 4], "续传只补缺失块");
}

#[tokio::test]
async fn a_wrong_ack_through_the_real_endpoints_is_retransmitted() {
    let bytes = content_bytes();
    let server = std::sync::Arc::new(Server::default());
    server.wrong_ack_once(1);
    let endpoint = UploadEndpoint::new(client(&server), ROOT).expect("上传端点");
    let source = SliceSource {
        bytes: bytes.clone(),
    };

    let outcome = upload::run(
        &source,
        &endpoint,
        None,
        &UploadOptions::new(4).with_part_concurrency(1),
    )
    .await
    .expect("重传后成功");
    assert_eq!(outcome.content.digest, digest_of(&bytes));
    let puts = server.puts();
    assert_eq!(
        puts.iter().filter(|(_, index)| *index == 1).count(),
        2,
        "回执不符的块必须重传：{puts:?}"
    );
}

#[tokio::test]
async fn a_download_through_the_real_endpoint_is_bound_to_the_version() {
    let bytes = content_bytes();
    let pin = VersionPin::new(
        "binding",
        ENTRY,
        VERSION,
        digest_of(&bytes),
        bytes.len() as u64,
    )
    .expect("固定版本");
    let server = std::sync::Arc::new(Server::default());
    let endpoint = ContentEndpoint::new(client(&server), ROOT).expect("内容端点");
    let temp = MemoryTemp::default();
    let progress = MemoryProgress::default();

    let outcome = download::run(
        &endpoint,
        &temp,
        &progress,
        &pin,
        &DownloadOptions::new(4).with_part_concurrency(2),
    )
    .await
    .expect("下载成功");
    assert_eq!(outcome.digest, digest_of(&bytes));
    assert_eq!(*temp.bytes.lock().expect("临时内容"), bytes);
    assert!(
        server
            .content_reads()
            .iter()
            .all(|(version, _, _)| version == VERSION),
        "请求必须绑在固定 VersionId 上"
    );
    assert!(server.content_reads().len() >= 5);
}

#[tokio::test]
async fn a_finalized_upload_is_not_treated_as_committed() {
    let bytes = content_bytes();
    let server = std::sync::Arc::new(Server::default());
    let endpoint = UploadEndpoint::new(client(&server), ROOT).expect("上传端点");
    let source = SliceSource {
        bytes: bytes.clone(),
    };
    let outcome = upload::run(&source, &endpoint, None, &UploadOptions::new(4))
        .await
        .expect("上传成功");
    // 结果只是可提交内容引用：没有 committed 标志，也没有提交条目的端点调用。
    assert_eq!(outcome.content.size, bytes.len() as u64);
    assert!(!outcome.content.storage_path.is_empty());
}

#[tokio::test]
async fn the_upload_endpoint_requires_credentials() {
    let server = std::sync::Arc::new(Server::default());
    let transport = std::sync::Arc::new(FakeTransport::new(|_request: TransportRequest| {
        Box::pin(async { panic!("没有凭据就不该发请求") })
    }));
    let client = ApiClient::with_transport(ClientConfig::new("https://vault.example"), transport)
        .expect("客户端配置");
    let endpoint = UploadEndpoint::new(client, ROOT).expect("上传端点");
    let source = SliceSource {
        bytes: content_bytes(),
    };
    let failure = upload::run(&source, &endpoint, None, &UploadOptions::new(4))
        .await
        .expect_err("没有凭据");
    assert!(failure.kind().needs_reauth(), "{failure}");
    assert_eq!(server.puts().len(), 0, "没有凭据时不上传任何块");
}
