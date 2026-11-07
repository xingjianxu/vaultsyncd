//! S074 测试用本机 TLS 入口与响应丢失注入。
//!
//! 真实产品链路在 Windows 客体上是 `agent → HTTPS(TLS) → 测试反向代理 → SSH 回环隧道 →
//! Linux 服务端`。生产部署的 TLS 终止由 S097 的反向代理负责；本模块只承担测试装置职责：
//!
//! - 用 rcgen 现场生成私有 CA 与服务端证书，只把 CA PEM 交给 agent 的 `server.ca_file`，
//!   客户端仍走完整证书校验（不关闭、不替换验证）。
//! - 每个 HTTP/1.1 请求单独转发到上游 `http://127.0.0.1:<tunnel>`，按 `Connection: close`
//!   读到完整响应后再写给客户端。
//! - 可对指定类别的 `POST /v1/roots/{root}/operations` **先转发、再丢弃响应**，制造
//!   “服务端已提交但客户端没收到 ACK”的真实网络故障，并记录每次提交的 op_id/结果。
//!
//! 解析只覆盖产品实际使用的形状（JSON 请求体、content-length 或 chunked 响应）；不认识的
//! 形状按原样转发，不猜测语义。

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;

/// 一次操作提交在代理侧留下的证据。
#[derive(Debug, Clone, Serialize)]
pub struct OperationEvent {
    /// 请求体里的 `op_id`。
    pub op_id: String,
    /// 请求体里的 `kind`（`put_content`、`delete` 等）。
    pub kind: String,
    /// 同一 op_id 的第几次提交（从 1 开始；响应丢失后的重试是第 2 次）。
    pub attempt: u32,
    /// 是否丢弃了这次响应。
    pub dropped: bool,
    /// 上游 HTTP 状态码。
    pub status: u16,
    /// 上游应答的操作状态（`committed`/`pending`/`rejected`）。
    pub state: Option<String>,
    /// 上游应答的 `replayed` 字段（只有提交端点会给出）。
    pub replayed: Option<bool>,
    /// 提交后的条目 revision。
    pub revision: Option<String>,
    /// 提交产生或影响的固定版本。
    pub version_id: Option<String>,
    /// 提交占用的 RootSeq。
    pub root_seq: Option<String>,
    /// 冲突副本条目身份（preserve-both 时）。
    pub conflict_entry_id: Option<String>,
    /// POST 提交的原始请求体（供测试按同一 OpId 重试；GET 查询没有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    armed: Option<Armed>,
    events: Vec<OperationEvent>,
    requests: u64,
    dropped: u64,
    audit: Vec<(String, u16)>,
    block_kind: Option<String>,
    blocked: Vec<serde_json::Value>,
    sse_fault: Option<String>,
    sse_effects: Vec<String>,
}

#[derive(Debug)]
struct Armed {
    kind: String,
    remaining: usize,
}

/// 一个已启动的测试 TLS 入口。
pub struct Proxy {
    /// 监听地址（HTTP 客户端应使用 `https://127.0.0.1:<port>`）。
    pub address: SocketAddr,
    /// 私有 CA 的 PEM 文件路径（给 agent 的 `server.ca_file`）。
    pub ca_file: PathBuf,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

impl Proxy {
    /// 生成 CA/服务端证书并在 `127.0.0.1:0` 起 TLS 入口，上游是 `upstream`（例如
    /// `http://127.0.0.1:18770`）。
    ///
    /// # Errors
    ///
    /// 证书生成、文件写入、绑定或 TLS 配置失败。
    pub async fn start(
        upstream: &str,
        ca_dir: &Path,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        tls_provider();
        let upstream = upstream.to_owned();
        let (ca_file, acceptor) = certificates(ca_dir)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = Arc::new(Mutex::new(State::default()));
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let upstream = upstream.clone();
                let state = task_state.clone();
                tokio::spawn(async move {
                    let _ = serve(acceptor, stream, &upstream, &state).await;
                });
            }
        });
        Ok(Self {
            address,
            ca_file,
            state,
            task,
        })
    }

    /// HTTPS 根地址，例如 `https://127.0.0.1:53412`。
    #[must_use]
    pub fn url(&self) -> String {
        format!("https://{}", self.address)
    }

    /// 让下一次（`remaining` 次）指定 `kind` 的操作提交在服务端已处理后丢弃响应。
    pub fn arm_drop(&self, kind: &str, remaining: usize) {
        self.state.lock().expect("proxy state").armed = Some(Armed {
            kind: kind.to_owned(),
            remaining,
        });
    }

    /// 当前累计的操作事件。
    #[must_use]
    pub fn events(&self) -> Vec<OperationEvent> {
        self.state.lock().expect("proxy state").events.clone()
    }

    /// S102：只改真实上游 SSE 的通知，不注入 changes 或假快照。
    pub fn arm_sse(&self, mode: &str) {
        assert!(matches!(mode, "loss" | "duplicate" | "reorder"));
        self.state.lock().expect("proxy state").sse_fault = Some(mode.into());
    }

    pub fn sse_effects(&self) -> Vec<String> {
        self.state.lock().expect("proxy state").sse_effects.clone()
    }

    /// S102：断开指定写请求，明确发生在上游收到请求之前。
    pub fn block_before_forward(&self, kind: Option<&str>) {
        self.state.lock().expect("proxy state").block_kind = kind.map(str::to_owned);
    }

    pub fn blocked_requests(&self) -> Vec<serde_json::Value> {
        self.state.lock().expect("proxy state").blocked.clone()
    }

    /// S102：请求路径/真实上游状态，不记录认证头。
    pub fn audit(&self) -> Vec<(String, u16)> {
        self.state.lock().expect("proxy state").audit.clone()
    }

    /// 已观察到的请求总数（含非操作请求）。
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.state.lock().expect("proxy state").requests
    }

    /// 已丢弃的响应数。
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.state.lock().expect("proxy state").dropped
    }

    /// 是否还有已武装但未触发的丢弃规则（用于等待故障真正发生）。
    #[must_use]
    pub fn armed(&self) -> bool {
        self.state
            .lock()
            .expect("proxy state")
            .armed
            .as_ref()
            .is_some_and(|armed| armed.remaining > 0)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 安装进程唯一的 rustls provider（reqwest/http-client 可能已装过，重复安装不是错误）。
pub fn tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn certificates(
    dir: &Path,
) -> Result<(PathBuf, TlsAcceptor), Box<dyn std::error::Error + Send + Sync>> {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let mut ca = CertificateParams::new(Vec::new())?;
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "vaultsyncd S074 test CA");
    let ca_key = KeyPair::generate()?;
    let ca_certificate = ca.self_signed(&ca_key)?;

    let mut server = CertificateParams::new(vec!["127.0.0.1".to_owned()])?;
    server
        .distinguished_name
        .push(DnType::CommonName, "vaultsyncd S074 test proxy");
    let server_key = KeyPair::generate()?;
    let server_certificate = server.signed_by(&server_key, &ca_certificate, &ca_key)?;

    std::fs::create_dir_all(dir)?;
    let ca_file = dir.join("ca.pem");
    std::fs::write(&ca_file, ca_certificate.pem())?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(server_certificate.der().to_vec())],
            PrivateKeyDer::Pkcs8(server_key.serialize_der().into()),
        )?;
    Ok((ca_file, TlsAcceptor::from(Arc::new(config))))
}

struct Request {
    head: Vec<u8>,
    body: Vec<u8>,
    method: String,
    path: String,
}

async fn read_request(
    stream: &mut tokio_rustls::server::TlsStream<TcpStream>,
) -> std::io::Result<Option<Request>> {
    // 一次读取可能同时包含请求头与请求体（reqwest 常把小 body 跟头写在一起），
    // 因此保留头结束后的剩余字节，不能丢弃。
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let end = loop {
        if let Some(end) = find(&buffer, b"\r\n\r\n") {
            break end;
        }
        if buffer.len() > 64 * 1024 {
            return Err(std::io::Error::other("request head too large"));
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = buffer[..end + 4].to_vec();
    let leftover = buffer[end + 4..].to_vec();
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.parse::<usize>().ok(),
            "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => chunked = true,
            _ => {}
        }
    }
    let mut reader = std::io::Cursor::new(leftover).chain(stream);
    let mut body = Vec::new();
    if let Some(length) = content_length {
        if length > 64 * 1024 * 1024 {
            return Err(std::io::Error::other("request body too large"));
        }
        body.resize(length, 0);
        reader.read_exact(&mut body).await?;
    } else if chunked {
        // 保留 chunked 原始帧，原样转发；这里只负责按终止块结束读取。
        loop {
            let size_line = read_line(&mut reader).await?;
            let text = String::from_utf8_lossy(&size_line);
            let size = usize::from_str_radix(text.trim().split(';').next().unwrap_or_default(), 16)
                .map_err(std::io::Error::other)?;
            body.extend_from_slice(&size_line);
            if size == 0 {
                // 终止块后面只跟 trailer 与空行；这里不再读“数据 + CRLF”。
                loop {
                    let line = read_line(&mut reader).await?;
                    body.extend_from_slice(&line);
                    if line == b"\r\n" {
                        break;
                    }
                }
                break;
            }
            let mut data = vec![0_u8; size + 2];
            reader.read_exact(&mut data).await?;
            body.extend_from_slice(&data);
        }
    }
    Ok(Some(Request {
        head,
        body,
        method,
        path,
    }))
}

async fn read_line<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let read = reader.read(&mut byte).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "early eof",
            ));
        }
        line.push(byte[0]);
        if line.ends_with(b"\r\n") {
            return Ok(line);
        }
        if line.len() > 8192 {
            return Err(std::io::Error::other("line too large"));
        }
    }
}

async fn serve(
    acceptor: TlsAcceptor,
    stream: TcpStream,
    upstream: &str,
    state: &Arc<Mutex<State>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = acceptor.accept(stream).await?;
    let Some(request) = read_request(&mut stream).await? else {
        return Ok(());
    };
    state.lock().expect("proxy state").requests += 1;
    let operation = operation_identity(&request);
    {
        let mut locked = state.lock().expect("proxy state");
        if operation
            .as_ref()
            .is_some_and(|(_, kind)| locked.block_kind.as_ref() == Some(kind))
        {
            locked.blocked.push(serde_json::from_slice(&request.body)?);
            return Ok(());
        }
    }
    let forward_head = rewrite_head(&request.head);
    let upstream_host = upstream
        .strip_prefix("http://")
        .ok_or("only plaintext loopback upstreams are supported")?;
    let mut upstream_stream = TcpStream::connect(upstream_host).await?;
    upstream_stream.write_all(forward_head.as_bytes()).await?;
    upstream_stream.write_all(&request.body).await?;
    upstream_stream.flush().await?;
    if request.path.ends_with("/events") {
        return sse(&mut stream, &mut upstream_stream, state).await;
    }
    let mut response = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(120),
        upstream_stream.read_to_end(&mut response),
    )
    .await
    .map_err(|_| "upstream response timed out")??;
    let (status, _head, body) = split_response(&response);
    state
        .lock()
        .expect("proxy state")
        .audit
        .push((request.path.clone(), status));
    let drop = if let Some((op_id, kind)) = operation {
        let mut locked = state.lock().expect("proxy state");
        let attempt = locked
            .events
            .iter()
            .filter(|event| event.op_id == op_id)
            .count() as u32
            + 1;
        let droppable = locked.armed.as_mut().is_some_and(|armed| {
            if armed.remaining > 0 && armed.kind == kind {
                armed.remaining -= 1;
                true
            } else {
                false
            }
        });
        let details = operation_details(&body);
        let is_query = kind == "query";
        locked.events.push(OperationEvent {
            op_id,
            kind,
            attempt,
            dropped: droppable,
            status,
            state: details.state,
            replayed: details.replayed,
            revision: details.revision,
            version_id: details.version_id,
            root_seq: details.root_seq,
            conflict_entry_id: details.conflict_entry_id,
            request_body: (!is_query && !request.body.is_empty())
                .then(|| String::from_utf8_lossy(&request.body).into_owned()),
        });
        if droppable {
            locked.dropped += 1;
        }
        droppable
    } else {
        false
    };
    if !drop {
        stream.write_all(&response).await?;
        stream.flush().await?;
    }
    let _ = stream.shutdown().await;
    Ok(())
}

/// SSE 只解 HTTP chunk framing；每次故障只作用于一个/一对真实通知。
async fn sse(
    downstream: &mut tokio_rustls::server::TlsStream<TcpStream>,
    upstream: &mut TcpStream,
    state: &Arc<Mutex<State>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(upstream.read_u8().await?);
        if head.len() > 65536 {
            return Err("SSE head too large".into());
        }
    }
    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
    if !text.starts_with("http/1.1 200 ") || !text.contains("transfer-encoding: chunked") {
        return Err("SSE upstream must be successful chunked HTTP/1.1".into());
    }
    downstream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .await?;
    let mode = state.lock().expect("proxy state").sse_fault.clone();
    let mut held: Option<Vec<u8>> = None;
    let mut fired = false;
    loop {
        let line = read_line(upstream).await?;
        let length = usize::from_str_radix(
            std::str::from_utf8(&line)?
                .trim()
                .split(';')
                .next()
                .unwrap_or(""),
            16,
        )?;
        if length == 0 {
            return Ok(());
        }
        if length > 1024 * 1024 {
            return Err("SSE chunk too large".into());
        }
        let mut chunk = vec![0; length];
        upstream.read_exact(&mut chunk).await?;
        if read_line(upstream).await? != b"\r\n" {
            return Err("bad SSE chunk terminator".into());
        }
        // Axum serializes each Event into one chunk; reject fragmented events rather than
        // silently claiming an injection on an unrecognised shape.
        let hint = chunk.windows(6).any(|s| s == b"event:") && chunk.ends_with(b"\n\n");
        if hint && !fired {
            match mode.as_deref() {
                Some("loss") => {
                    fired = true;
                    state
                        .lock()
                        .expect("proxy state")
                        .sse_effects
                        .push("loss".into());
                    continue;
                }
                Some("duplicate") => {
                    downstream.write_all(&chunk).await?;
                    fired = true;
                }
                Some("reorder") => {
                    if let Some(first) = held.take() {
                        downstream.write_all(&chunk).await?;
                        downstream.write_all(&first).await?;
                        fired = true;
                        state
                            .lock()
                            .expect("proxy state")
                            .sse_effects
                            .push("reorder".into());
                        continue;
                    }
                    held = Some(chunk);
                    continue;
                }
                _ => {}
            }
            if fired {
                state
                    .lock()
                    .expect("proxy state")
                    .sse_effects
                    .push("duplicate".into());
            }
        }
        downstream.write_all(&chunk).await?;
        downstream.flush().await?;
    }
}

/// 把请求行之外的头里与连接管理相关的字段收敛成 `Connection: close`，其余原样保留。
fn rewrite_head(head: &[u8]) -> String {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let mut out = String::new();
    if let Some(request_line) = lines.next() {
        out.push_str(request_line);
        out.push_str("\r\n");
    }
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let name = line.split(':').next().unwrap_or_default();
        let name = name.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "connection" | "keep-alive" | "proxy-connection"
        ) {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    out
}

fn split_response(response: &[u8]) -> (u16, &[u8], Vec<u8>) {
    let end = find(response, b"\r\n\r\n").unwrap_or(response.len());
    let head = &response[..end.min(response.len())];
    let body = response.get(end + 4..).unwrap_or_default().to_vec();
    let status = String::from_utf8_lossy(head)
        .split(' ')
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    (status, head, body)
}

fn operation_identity(request: &Request) -> Option<(String, String)> {
    if request.method == "POST" && request.path.ends_with("/operations") {
        let body: serde_json::Value = serde_json::from_slice(&request.body).ok()?;
        let op_id = body.get("op_id")?.as_str()?.to_owned();
        let kind = body.get("kind")?.as_str()?.to_owned();
        return Some((op_id, kind));
    }
    if request.method == "GET"
        && let Some((_, tail)) = request.path.split_once("/operations/")
    {
        let op_id = tail.split('?').next().unwrap_or_default();
        if op_id.len() == 32 && op_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some((op_id.to_owned(), "query".to_owned()));
        }
    }
    None
}

#[derive(Default)]
struct Details {
    state: Option<String>,
    replayed: Option<bool>,
    revision: Option<String>,
    version_id: Option<String>,
    root_seq: Option<String>,
    conflict_entry_id: Option<String>,
}

fn operation_details(body: &[u8]) -> Details {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Details::default();
    };
    Details {
        state: value
            .get("state")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        replayed: value.get("replayed").and_then(|v| v.as_bool()),
        revision: value
            .get("revision")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        version_id: value
            .get("version_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        root_seq: value
            .get("root_seq")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        conflict_entry_id: value
            .get("conflict")
            .and_then(|v| v.get("conflict_entry_id"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_connection_headers_and_preserves_request_line() {
        let head = b"POST /v1/roots/x/operations HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\nContent-Length: 2\r\n\r\n";
        let rewritten = rewrite_head(head);
        assert!(rewritten.starts_with("POST /v1/roots/x/operations HTTP/1.1\r\n"));
        assert!(rewritten.ends_with("Connection: close\r\n\r\n"));
        assert!(!rewritten.contains("keep-alive"));
        assert!(rewritten.contains("Content-Length: 2"));
    }

    #[test]
    fn detects_operation_identity_only_for_operation_posts() {
        let request = Request {
            head: Vec::new(),
            body: br#"{"op_id":"0123456789abcdef0123456789abcdef","kind":"put_content"}"#.to_vec(),
            method: "POST".into(),
            path: "/v1/roots/root/operations".into(),
        };
        assert_eq!(
            operation_identity(&request),
            Some((
                "0123456789abcdef0123456789abcdef".into(),
                "put_content".into()
            ))
        );
        let mut get = request;
        get.method = "GET".into();
        assert_eq!(operation_identity(&get), None);
        let query = Request {
            head: Vec::new(),
            body: Vec::new(),
            method: "GET".into(),
            path: "/v1/roots/root/operations/0123456789abcdef0123456789abcdef".into(),
        };
        assert_eq!(
            operation_identity(&query),
            Some(("0123456789abcdef0123456789abcdef".into(), "query".into()))
        );
    }

    #[test]
    fn parses_committed_drop_details() {
        let body = br#"{"op_id":"x","state":"committed","replayed":true,"revision":"7","version_id":"v","root_seq":"9","conflict":{"conflict_entry_id":"c"}}"#;
        let details = operation_details(body);
        assert_eq!(details.state.as_deref(), Some("committed"));
        assert_eq!(details.replayed, Some(true));
        assert_eq!(details.revision.as_deref(), Some("7"));
        assert_eq!(details.version_id.as_deref(), Some("v"));
        assert_eq!(details.root_seq.as_deref(), Some("9"));
        assert_eq!(details.conflict_entry_id.as_deref(), Some("c"));
    }
}
