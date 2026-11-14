//! Authenticated fixed-version Office viewing, signed read-only sessions and legacy PDF conversion.
use std::{
    collections::BTreeMap,
    future::{Future, IntoFuture},
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use reqwest::{Client, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    task::JoinHandle,
};
use vaultsyncd_domain::ids::{EntryId, SessionId, VersionId};
use vaultsyncd_protocol::generated::{
    ErrorCode, OfficeFileType, OfficePreviewRequest, OfficeViewerClosed, OfficeViewerSession,
};
use vaultsyncd_server_core::objects::ContentDigest;

use crate::{
    config::OfficeConfig,
    content::ContentContext,
    uploads::{Caller, bad_request, error_response, parse_identity},
};

/// Both original Office input and converted PDF output are bounded before allocation.
pub const MAX_OFFICE_BYTES: usize = 50 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONTROL_BYTES: usize = 64 * 1024;

/// A process-owned client, secret and non-queuing conversion limit; no secret is exposed by Debug.
pub struct OfficeRuntime {
    base: Url,
    client: Client,
    secret: Vec<u8>,
    slots: Arc<Semaphore>,
    viewers: Arc<Mutex<BTreeMap<String, ViewerLease>>>,
}

/// Only the initiating login session may revoke its source capability.
/// The source and its memory/slot expire independently of browser cleanup.
struct ViewerLease {
    owner: SessionId,
    target: (String, String, String),
    _source: Source,
    _slot: OwnedSemaphorePermit,
}

pub(crate) fn parse_document_server_url(text: &str) -> io::Result<Url> {
    let url = Url::parse(text).map_err(|_| invalid("Document Server URL 无法解析"))?;
    let local_http = url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host.trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
    if !(url.scheme() == "https" || local_http)
        || !url.host_str().is_some_and(|host| {
            host.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'[' | b']')
            })
        })
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(invalid(
            "Document Server 仅接受无凭据/路径/查询的 HTTPS 根 URL 或字面回环 HTTP 根 URL",
        ));
    }
    Ok(url)
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

impl OfficeRuntime {
    /// Validated deployment origin for the Web viewer's precise CSP allowance.
    pub fn document_server_url(&self) -> &str {
        self.base.as_str()
    }

    /// Load the bounded runtime secret before startup opens application state.
    ///
    /// # Errors
    /// Invalid URLs, missing/empty/oversized secret files and HTTP client setup failures are fatal.
    pub fn load(config: &OfficeConfig) -> io::Result<Self> {
        let base = parse_document_server_url(&config.document_server_url)?;
        if !config.jwt_secret_file.is_absolute() {
            return Err(invalid("Office JWT 密钥路径必须为绝对路径"));
        }
        let file = std::fs::File::open(&config.jwt_secret_file)?;
        if !file.metadata()?.is_file() {
            return Err(invalid("Office JWT 密钥必须为普通文件"));
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut std::io::Read::take(file, 4097), &mut bytes)?;
        if bytes.len() > 4096 {
            return Err(invalid("Office JWT 密钥超过 4096 字节"));
        }
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        if bytes.len() < 32 || !bytes.iter().all(|byte| byte.is_ascii_graphic()) {
            return Err(invalid("Office JWT 密钥须为至少32字节的非空白ASCII文本"));
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(TIMEOUT)
            .build()
            .map_err(|_| invalid("Office HTTP 客户端无法初始化"))?;
        Ok(Self {
            base,
            client,
            secret: bytes,
            slots: Arc::new(Semaphore::new(2)),
            viewers: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    fn token(&self, payload: &serde_json::Value) -> Result<String, ConversionFailure> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let bytes = serde_json::to_vec(payload).map_err(|_| ConversionFailure::Unavailable)?;
        let payload = URL_SAFE_NO_PAD.encode(bytes);
        let message = format!("{header}.{payload}");
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret)
            .map_err(|_| ConversionFailure::Unavailable)?;
        mac.update(message.as_bytes());
        Ok(format!(
            "{message}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        ))
    }

    async fn convert(
        &self,
        source: &str,
        file_type: &str,
        now: i64,
    ) -> Result<Vec<u8>, ConversionFailure> {
        let payload = serde_json::json!({
            "async": false, "filetype": file_type, "outputtype": "pdf", "title": format!("preview.{file_type}"),
            "key": hex(&nonce()), "url": source, "exp": now.saturating_add(120),
            "spreadsheetLayout": { "ignorePrintArea": true }, "operation": "converter",
        });
        let token = self.token(&payload)?;
        // URLs and JWTs are intentionally never included in diagnostics.
        let response = self
            .client
            .post(
                self.base
                    .join("converter")
                    .map_err(|_| ConversionFailure::Unavailable)?,
            )
            .json(&serde_json::json!({ "token": token }))
            .send()
            .await
            .map_err(|_| ConversionFailure::Unavailable)?;
        if !response.status().is_success() {
            return Err(ConversionFailure::Unavailable);
        }
        let body = bounded_response(response, MAX_CONTROL_BYTES).await?;
        let reply: ConversionReply =
            serde_json::from_slice(&body).map_err(|_| ConversionFailure::Invalid)?;
        if reply.error.is_some() || reply.end_convert != Some(true) {
            return Err(ConversionFailure::Invalid);
        }
        let file = Url::parse(
            reply
                .file_url
                .as_deref()
                .ok_or(ConversionFailure::Invalid)?,
        )
        .map_err(|_| ConversionFailure::Invalid)?;
        if file.origin() != self.base.origin()
            || !file.username().is_empty()
            || file.password().is_some()
            || file.fragment().is_some()
            || !file.path().starts_with("/cache/files/")
        {
            return Err(ConversionFailure::Invalid);
        }
        let response = self
            .client
            .get(file)
            .send()
            .await
            .map_err(|_| ConversionFailure::Unavailable)?;
        if !response.status().is_success() {
            return Err(ConversionFailure::Invalid);
        }
        let bytes = bounded_response(response, MAX_OFFICE_BYTES).await?;
        if !bytes.starts_with(b"%PDF-") {
            return Err(ConversionFailure::Invalid);
        }
        Ok(bytes)
    }
}

#[derive(Deserialize)]
struct ConversionReply {
    error: Option<i64>,
    #[serde(rename = "endConvert")]
    end_convert: Option<bool>,
    #[serde(rename = "fileUrl")]
    file_url: Option<String>,
}
#[derive(Debug)]
enum ConversionFailure {
    Unavailable,
    Invalid,
    TooLarge,
}
async fn bounded_response(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ConversionFailure> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(ConversionFailure::TooLarge);
    }
    let mut output = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ConversionFailure::Unavailable)?
    {
        if chunk.len() > limit.saturating_sub(output.len()) {
            return Err(ConversionFailure::TooLarge);
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

// Source-specific socket cancellation also closes already accepted HTTP connections.
// Aborting axum's accept task alone would only request graceful connection shutdown.
struct SourceListener {
    listener: TcpListener,
    shutdown: watch::Receiver<bool>,
}
impl axum::serve::Listener for SourceListener {
    type Io = SourceStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, address)) => {
                    let mut shutdown = self.shutdown.clone();
                    let cancelled = Box::pin(async move {
                        if !*shutdown.borrow() {
                            let _ = shutdown.changed().await;
                        }
                    });
                    return (
                        SourceStream {
                            stream,
                            cancelled,
                            closed: false,
                        },
                        address,
                    );
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}
struct SourceStream {
    stream: TcpStream,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
    closed: bool,
}
impl SourceStream {
    fn cancelled(&mut self, cx: &mut Context<'_>) -> bool {
        self.closed = self.closed || self.cancelled.as_mut().poll(cx).is_ready();
        self.closed
    }
    fn error() -> io::Error {
        io::Error::new(io::ErrorKind::ConnectionAborted, "Office source closed")
    }
}
impl AsyncRead for SourceStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.cancelled(cx) {
            return Poll::Ready(Err(Self::error()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}
impl AsyncWrite for SourceStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.cancelled(cx) {
            return Poll::Ready(Err(Self::error()));
        }
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.cancelled(cx) {
            return Poll::Ready(Err(Self::error()));
        }
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.cancelled(cx) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

struct Source {
    task: JoinHandle<()>,
    active: Arc<AtomicBool>,
    shutdown: watch::Sender<bool>,
    url: String,
}
impl Drop for Source {
    fn drop(&mut self) {
        self.active.store(false, Ordering::SeqCst);
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}
impl Source {
    async fn open(
        context: Arc<ContentContext>,
        original_headers: HeaderMap,
        vault: String,
        bytes: Vec<u8>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let token = hex(&nonce());
        let route = format!("/{token}");
        let url = format!("http://{address}{route}");
        let (shutdown, receiver) = watch::channel(false);
        let listener = SourceListener {
            listener,
            shutdown: receiver.clone(),
        };
        let bytes = Bytes::from(bytes);
        let active = Arc::new(AtomicBool::new(true));
        let serving = Arc::clone(&active);
        let router = Router::new().route(
            &route,
            get(move || {
                let context = Arc::clone(&context);
                let headers = original_headers.clone();
                let vault = vault.clone();
                let bytes = bytes.clone();
                let active = Arc::clone(&serving);
                let mut shutdown = receiver.clone();
                async move {
                    tokio::select! {
                        result = Caller::resolve(&context.auth, &headers, &vault) => {
                            if let Err(response) = result { return response; }
                        }
                        _ = shutdown.changed() => return StatusCode::GONE.into_response(),
                    }
                    if !active.load(Ordering::SeqCst) {
                        return StatusCode::GONE.into_response();
                    }
                    let mut response = Response::new(Body::from(bytes.clone()));
                    response.headers_mut().insert(
                        header::CONTENT_LENGTH,
                        HeaderValue::from_str(&bytes.len().to_string()).expect("length"),
                    );
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/octet-stream"),
                    );
                    response
                        .headers_mut()
                        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                    response
                }
            }),
        );
        let task = tokio::spawn(async move {
            let _ =
                tokio::time::timeout(TIMEOUT, axum::serve(listener, router).into_future()).await;
        });
        Ok(Self {
            task,
            active,
            shutdown,
            url,
        })
    }
}
fn nonce() -> [u8; 32] {
    let mut bytes = [0; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Create a signed view-only configuration, with no storage write-back capability.
pub async fn create_viewer(
    State(context): State<Arc<ContentContext>>,
    Path(target): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Result<Json<OfficePreviewRequest>, JsonRejection>,
) -> Response {
    match tokio::time::timeout(TIMEOUT, create_viewer_inner(context, target, headers, body)).await {
        Ok(response) => response,
        Err(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::NeedsRecovery,
            &hex(&nonce()),
            "Office 查看器准备超时，请稍后重试",
            true,
            None,
        ),
    }
}

async fn create_viewer_inner(
    context: Arc<ContentContext>,
    target: (String, String, String),
    headers: HeaderMap,
    body: Result<Json<OfficePreviewRequest>, JsonRejection>,
) -> Response {
    let (vault, entry, version) = &target;
    let (caller, vault_id) = match Caller::resolve(&context.auth, &headers, vault).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let body = match body {
        Ok(Json(body)) => body,
        Err(_) => {
            return bad_request(
                &caller.request_id,
                "Office 查看器请求不是合法 JSON 或格式不受支持",
            );
        }
    };
    let (Some(entry_id), Some(version_id)) = (
        parse_identity::<EntryId>(entry),
        parse_identity::<VersionId>(version),
    ) else {
        return bad_request(&caller.request_id, "Office 查看器条目或版本身份不合法");
    };
    let (Ok(hash), Ok(size)) = (
        ContentDigest::from_hex(&body.hash),
        body.size.parse::<u64>(),
    ) else {
        return bad_request(&caller.request_id, "Office 查看器摘要或大小不合法");
    };
    if body.size != size.to_string() {
        return bad_request(&caller.request_id, "Office 查看器大小不是规范十进制文本");
    }
    if size > MAX_OFFICE_BYTES as u64 {
        return office_error(
            &caller,
            vault,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Office 查看器仅支持不超过 50 MiB 的文件",
            false,
        );
    }
    let Some(office) = &context.office else {
        return office_error(
            &caller,
            vault,
            StatusCode::SERVICE_UNAVAILABLE,
            "服务端尚未配置 Office 预览",
            true,
        );
    };
    let Ok(slot) = Arc::clone(&office.slots).try_acquire_owned() else {
        return office_error(
            &caller,
            vault,
            StatusCode::SERVICE_UNAVAILABLE,
            "Office 预览繁忙，请稍后重试",
            true,
        );
    };
    let bytes = match crate::content::read_office_source(
        &context, &caller, vault_id, entry_id, version_id, hash, size,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let source =
        match Source::open(Arc::clone(&context), headers.clone(), vault.clone(), bytes).await {
            Ok(source) => source,
            Err(_) => {
                return office_error(
                    &caller,
                    vault,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Office 预览源服务不可用",
                    true,
                );
            }
        };
    if let Err(response) = Caller::resolve(&context.auth, &headers, vault).await {
        return response;
    }
    let (file_type, document_type) = match body.file_type {
        OfficeFileType::Doc => ("doc", "word"),
        OfficeFileType::Docx => ("docx", "word"),
        OfficeFileType::Odt => ("odt", "word"),
        OfficeFileType::Xls => ("xls", "cell"),
        OfficeFileType::Xlsx => ("xlsx", "cell"),
        OfficeFileType::Ods => ("ods", "cell"),
        OfficeFileType::Ppt => ("ppt", "slide"),
        OfficeFileType::Pptx => ("pptx", "slide"),
        OfficeFileType::Odp => ("odp", "slide"),
    };
    let session_id = hex(&nonce());
    let mut config = serde_json::json!({
        "documentType": document_type,
        "type": "desktop",
        "document": {
            "key": session_id, "fileType": file_type, "title": format!("preview.{file_type}"),
            "url": source.url,
            "permissions": {"edit": false, "review": false, "comment": false, "fillForms": false,
                "download": false, "print": false, "chat": false, "modifyContentControl": false, "modifyFilter": false},
        },
        "editorConfig": {"mode": "view", "lang": "zh-CN", "coEditing": {"mode": "strict", "change": false},
            "user": {"id": session_id, "name": "查看者"},
            "customization": {"autosave": false, "forcesave": false, "macros": false, "macrosMode": "disable", "plugins": false},
            "plugins": {"autostart": [], "pluginsData": []},
        },
        "exp": caller.now.as_unix_seconds().saturating_add(120),
    });
    let token = match office.token(&config) {
        Ok(token) => token,
        Err(_) => {
            return office_error(
                &caller,
                vault,
                StatusCode::SERVICE_UNAVAILABLE,
                "Office 查看器签名失败",
                true,
            );
        }
    };
    config["token"] = serde_json::Value::String(token);
    let response_body = OfficeViewerSession {
        session_id: session_id.clone(),
        document_server_url: office.base.to_string(),
        config_json: config.to_string(),
        source_hash: hash.to_hex(),
        source_version: version.clone(),
    };
    office.viewers.lock().expect("viewer leases").insert(
        session_id.clone(),
        ViewerLease {
            owner: caller.principal.session_id,
            target: target.clone(),
            _source: source,
            _slot: slot,
        },
    );
    let viewers = Arc::downgrade(&office.viewers);
    tokio::spawn(async move {
        tokio::time::sleep(TIMEOUT).await;
        if let Some(viewers) = viewers.upgrade() {
            viewers.lock().expect("viewer leases").remove(&session_id);
        }
    });
    let mut response = Json(response_body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Revoke the initiating session's temporary source; already rendered upstream data cannot be recalled.
pub async fn close_viewer(
    State(context): State<Arc<ContentContext>>,
    Path((vault, entry, version, session)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (caller, _) = match Caller::resolve(&context.auth, &headers, &vault).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if parse_identity::<EntryId>(&entry).is_none()
        || parse_identity::<VersionId>(&version).is_none()
        || session.len() != 64
        || !session
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return bad_request(&caller.request_id, "Office 查看器身份不合法");
    }
    if let Some(office) = &context.office {
        let mut viewers = office.viewers.lock().expect("viewer leases");
        if let Some(viewer) = viewers.get(&session) {
            if viewer.owner != caller.principal.session_id
                || viewer.target != (vault.clone(), entry, version)
            {
                return office_error(
                    &caller,
                    &vault,
                    StatusCode::NOT_FOUND,
                    "Office 查看器不存在",
                    false,
                );
            }
            viewers.remove(&session);
        }
    }
    let mut response = Json(OfficeViewerClosed { closed: true }).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Return a PDF derived from exactly the authenticated entry/version and expected snapshot metadata.
pub async fn preview(
    State(context): State<Arc<ContentContext>>,
    Path((vault, entry, version)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Result<Json<OfficePreviewRequest>, JsonRejection>,
) -> Response {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    match tokio::time::timeout_at(
        deadline,
        preview_inner(context, (vault, entry, version), headers, body, deadline),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::NeedsRecovery,
            &hex(&nonce()),
            "Office 预览超时，请稍后重试",
            true,
            None,
        ),
    }
}

async fn preview_inner(
    context: Arc<ContentContext>,
    (vault, entry, version): (String, String, String),
    headers: HeaderMap,
    body: Result<Json<OfficePreviewRequest>, JsonRejection>,
    deadline: tokio::time::Instant,
) -> Response {
    let (caller, vault_id) = match Caller::resolve(&context.auth, &headers, &vault).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let body = match body {
        Ok(Json(body)) => body,
        Err(_) => {
            return bad_request(
                &caller.request_id,
                "Office 预览请求不是合法 JSON 或格式不受支持",
            );
        }
    };
    let (Some(entry_id), Some(version_id)) = (
        parse_identity::<EntryId>(&entry),
        parse_identity::<VersionId>(&version),
    ) else {
        return bad_request(&caller.request_id, "Office 预览条目或版本身份不合法");
    };
    let (Ok(hash), Ok(size)) = (
        ContentDigest::from_hex(&body.hash),
        body.size.parse::<u64>(),
    ) else {
        return bad_request(&caller.request_id, "Office 预览摘要或大小不合法");
    };
    if body.size != size.to_string() {
        return bad_request(&caller.request_id, "Office 预览大小不是规范十进制文本");
    }
    if size > MAX_OFFICE_BYTES as u64 {
        return office_error(
            &caller,
            &vault,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Office 预览仅支持不超过 50 MiB 的文件",
            false,
        );
    }
    let Some(office) = &context.office else {
        return office_error(
            &caller,
            &vault,
            StatusCode::SERVICE_UNAVAILABLE,
            "服务端尚未配置 Office 预览",
            true,
        );
    };
    let Ok(slot) = Arc::clone(&office.slots).try_acquire_owned() else {
        return office_error(
            &caller,
            &vault,
            StatusCode::SERVICE_UNAVAILABLE,
            "Office 预览繁忙，请稍后重试",
            true,
        );
    };
    let file_type = match body.file_type {
        OfficeFileType::Doc => "doc",
        OfficeFileType::Docx => "docx",
        OfficeFileType::Xls => "xls",
        OfficeFileType::Xlsx => "xlsx",
        OfficeFileType::Ppt => "ppt",
        OfficeFileType::Pptx => "pptx",
        OfficeFileType::Odt => "odt",
        OfficeFileType::Ods => "ods",
        OfficeFileType::Odp => "odp",
    };
    let work = async {
        let bytes = crate::content::read_office_source(
            &context, &caller, vault_id, entry_id, version_id, hash, size,
        )
        .await
        .map_err(|response| *response)?;
        let source = Source::open(Arc::clone(&context), headers.clone(), vault.clone(), bytes)
            .await
            .map_err(|_| {
                office_error(
                    &caller,
                    &vault,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Office 预览源服务不可用",
                    true,
                )
            })?;
        // The worker owns its slot until OnlyOffice responds or the original deadline expires.
        // Dropping an HTTP handler revokes its source, but cannot free capacity for a still-running job.
        let office = Arc::clone(office);
        let source_url = source.url.clone();
        let now = caller.now.as_unix_seconds();
        let worker = tokio::spawn(async move {
            let _slot = slot;
            tokio::time::timeout_at(deadline, office.convert(&source_url, file_type, now))
                .await
                .unwrap_or(Err(ConversionFailure::Unavailable))
        });
        let converted = worker.await.unwrap_or(Err(ConversionFailure::Unavailable));
        // Remove the source channel immediately; it cannot outlive conversion or cancellation.
        drop(source);
        // Revocation during conversion takes precedence over all conversion outcomes.
        Caller::resolve(&context.auth, &headers, &vault).await?;
        let bytes = match converted {
            Ok(bytes) => bytes,
            Err(ConversionFailure::Unavailable) => {
                return Err(office_error(
                    &caller,
                    &vault,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Office 转换服务不可用，请稍后重试",
                    true,
                ));
            }
            Err(ConversionFailure::Invalid) => {
                return Err(office_error(
                    &caller,
                    &vault,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Office 文件转换失败，文件可能损坏、受密码保护或格式不受支持",
                    false,
                ));
            }
            Err(ConversionFailure::TooLarge) => {
                return Err(office_error(
                    &caller,
                    &vault,
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Office 预览转换结果超过大小限制",
                    false,
                ));
            }
        };
        let digest = hex(&Sha256::digest(&bytes));
        let output_length = bytes.len();
        let mut response = Response::new(Body::from(bytes));
        for (key, value) in [
            (header::CONTENT_TYPE.as_str(), "application/pdf".to_owned()),
            (header::CONTENT_LENGTH.as_str(), output_length.to_string()),
            (header::CACHE_CONTROL.as_str(), "no-store".to_owned()),
            ("x-content-type-options", "nosniff".to_owned()),
            ("x-content-sha256", digest),
            ("x-preview-source-sha256", hash.to_hex()),
            ("x-preview-source-version", version.clone()),
        ] {
            response.headers_mut().insert(
                axum::http::HeaderName::from_bytes(key.as_bytes()).expect("static header"),
                HeaderValue::from_str(&value).expect("verified header value"),
            );
        }
        Ok(response)
    };
    match tokio::time::timeout(TIMEOUT, work).await {
        Ok(Ok(response)) | Ok(Err(response)) => response,
        Err(_) => office_error(
            &caller,
            &vault,
            StatusCode::SERVICE_UNAVAILABLE,
            "Office 预览超时，请稍后重试",
            true,
        ),
    }
}
fn office_error(
    caller: &Caller,
    vault: &str,
    status: StatusCode,
    message: &str,
    retry: bool,
) -> Response {
    let code = if status == StatusCode::SERVICE_UNAVAILABLE {
        ErrorCode::NeedsRecovery
    } else {
        ErrorCode::InvalidRequest
    };
    error_response(
        status,
        code,
        &caller.request_id,
        message,
        retry,
        Some(vault.to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    #[test]
    fn configured_origin_rejects_credentials_paths_queries_and_nonlocal_plaintext() {
        for valid in [
            "https://office.example",
            "https://office.example:15402/",
            "http://127.0.0.1:1234",
            "http://[::1]:1234/",
        ] {
            assert!(parse_document_server_url(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "http://office.example",
            "http://localhost:8080",
            "https://user:secret@office.example",
            "https://office.example/a",
            "https://office.example/?url=x",
            "https://office.example/#x",
            "file:///tmp/x",
            "not-url",
        ] {
            assert!(parse_document_server_url(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn startup_secret_loading_is_bounded_and_rejects_empty_or_nontext_credentials() {
        let path = std::env::temp_dir().join(format!("S213-office-secret-{}", hex(&nonce())));
        let config = OfficeConfig {
            document_server_url: "https://office.example".to_owned(),
            jwt_secret_file: path.clone(),
        };
        assert!(OfficeRuntime::load(&config).is_err());
        for bytes in [vec![], vec![b'x'; 31], vec![0; 64], vec![b'x'; 4097]] {
            std::fs::write(&path, bytes).unwrap();
            assert!(OfficeRuntime::load(&config).is_err());
        }
        std::fs::write(&path, format!("{}\n", "x".repeat(64))).unwrap();
        let runtime = OfficeRuntime::load(&config).unwrap();
        assert_eq!(runtime.secret, vec![b'x'; 64]);
        std::fs::remove_file(path).unwrap();
    }

    fn runtime(base: Url) -> OfficeRuntime {
        OfficeRuntime {
            base,
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            secret: vec![b'x'; 64],
            slots: Arc::new(Semaphore::new(2)),
            viewers: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    #[tokio::test]
    async fn converter_errors_untrusted_urls_redirects_and_nonpdf_output_never_become_previews() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let mode = Arc::new(AtomicU8::new(0));
        let control_mode = Arc::clone(&mode);
        let control_base = base.clone();
        let file_mode = Arc::clone(&mode);
        let router = Router::new().route("/converter", axum::routing::post(move |Json(request): Json<serde_json::Value>| {
            let mode = Arc::clone(&control_mode); let base = control_base.clone();
            async move {
                let token = request["token"].as_str().unwrap();
                let parts: Vec<_> = token.split('.').collect(); assert_eq!(parts.len(),3);
                let mut verifier = Hmac::<Sha256>::new_from_slice(&[b'x';64]).unwrap();
                verifier.update(format!("{}.{}",parts[0],parts[1]).as_bytes());
                verifier.verify_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
                let payload: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
                assert_eq!(payload["outputtype"], "pdf"); assert_eq!(payload["operation"],"converter");
                assert_eq!(payload["exp"], 1120);
                match mode.load(Ordering::SeqCst) {
                    1 => Json(serde_json::json!({"error":-5})).into_response(),
                    2 => Json(serde_json::json!({"endConvert":true,"fileUrl":"https://other.example/cache/files/x.pdf"})).into_response(),
                    3 => (StatusCode::FOUND, [(header::LOCATION,"/cache/files/x.pdf")]).into_response(),
                    _ => Json(serde_json::json!({"endConvert":true,"fileUrl":base.join("cache/files/x.pdf").unwrap().as_str()})).into_response(),
                }
            }
        })).route("/cache/files/x.pdf", get(move || {
            let mode = Arc::clone(&file_mode);
            async move { if mode.load(Ordering::SeqCst)==4 { "<html>not PDF</html>" } else { "%PDF-1.4\nfixture" } }
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let runtime = runtime(base);
        assert!(
            runtime
                .convert("http://127.0.0.1:3/source", "docx", 1000)
                .await
                .unwrap()
                .starts_with(b"%PDF-")
        );
        for value in 1..=4 {
            mode.store(value, Ordering::SeqCst);
            assert!(
                runtime
                    .convert("http://127.0.0.1:3/source", "docx", 1000)
                    .await
                    .is_err(),
                "mode {value}"
            );
        }
        mode.store(0, Ordering::SeqCst);
        let response = runtime
            .client
            .get(runtime.base.join("cache/files/x.pdf").unwrap())
            .send()
            .await
            .unwrap();
        assert!(matches!(
            bounded_response(response, 4).await,
            Err(ConversionFailure::TooLarge)
        ));
        let one = Arc::clone(&runtime.slots).try_acquire_owned().unwrap();
        let two = Arc::clone(&runtime.slots).try_acquire_owned().unwrap();
        assert!(Arc::clone(&runtime.slots).try_acquire_owned().is_err());
        drop((one, two));
        server.abort();
    }
    #[tokio::test]
    async fn source_shutdown_interrupts_already_accepted_socket_reads_and_writes() {
        use axum::serve::Listener;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = watch::channel(false);
        let mut listener = SourceListener {
            listener,
            shutdown: receiver,
        };
        let mut first = TcpStream::connect(address).await.unwrap();
        let (mut first_stream, _) = listener.accept().await;
        let _second = TcpStream::connect(address).await.unwrap();
        let (mut second_stream, _) = listener.accept().await;
        first.write_all(b"ok").await.unwrap();
        let mut bytes = [0; 2];
        first_stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(bytes, *b"ok");
        let pending = tokio::spawn(async move { first_stream.read_u8().await });
        tokio::task::yield_now().await;
        let source = Source {
            task: tokio::spawn(std::future::pending()),
            active: Arc::new(AtomicBool::new(true)),
            shutdown,
            url: "unused".to_owned(),
        };
        drop(source);
        let result = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(
            second_stream
                .write_all(b"must not leave source")
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            second_stream
                .write_all(b"repeat poll stays closed")
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionAborted
        );
    }
}
