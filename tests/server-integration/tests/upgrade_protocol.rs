//! S091默认生产ApiClient经真实TCP协议fixture代理，错误协商不得抵达真实认证/上传副作用。
mod common;
use common::{Lab, Running};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use vaultsyncd_http_client::{ApiClient, ApiRequest, AuthMode, ClientConfig, Credentials};
use vaultsyncd_protocol::generated::{LoginRequest, SessionIssued};
struct Proxy {
    url: String,
    calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Proxy {
    async fn start(real: String, capabilities: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let real = real.clone();
                let capabilities = capabilities.clone();
                let calls = Arc::clone(&count);
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let split = loop {
                        let mut b = [0; 1024];
                        let n = socket.read(&mut b).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&b[..n]);
                        assert!(bytes.len() < 64 * 1024);
                        if let Some(at) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                            break at + 4;
                        }
                    };
                    let header = String::from_utf8(bytes[..split].to_vec()).unwrap();
                    let mut first = header.lines().next().unwrap().split_whitespace();
                    let method = first.next().unwrap();
                    let path = first.next().unwrap();
                    let length = header
                        .lines()
                        .find_map(|l| {
                            l.split_once(':')
                                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    assert!(length < 32 * 1024);
                    while bytes.len() < split + length {
                        let mut b = [0; 1024];
                        let n = socket.read(&mut b).await.unwrap();
                        assert!(n != 0);
                        bytes.extend_from_slice(&b[..n]);
                    }
                    let (status, body) = if path == "/v1/capabilities" {
                        (200, serde_json::to_vec(&capabilities).unwrap())
                    } else {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let mut request = reqwest::Client::new()
                            .request(method.parse().unwrap(), format!("{real}{path}"));
                        for line in header.lines().skip(1) {
                            if let Some((key, value)) = line.split_once(':')
                                && ["authorization", "content-type"]
                                    .iter()
                                    .any(|k| key.eq_ignore_ascii_case(k))
                            {
                                request = request.header(key, value.trim());
                            }
                        }
                        let response = request
                            .body(bytes[split..split + length].to_vec())
                            .send()
                            .await
                            .unwrap();
                        (
                            response.status().as_u16(),
                            response.bytes().await.unwrap().to_vec(),
                        )
                    };
                    socket.write_all(format!("HTTP/1.1 {status} result\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                });
            }
        });
        Self { url, calls, task }
    }
    fn client(&self) -> ApiClient<vaultsyncd_http_client::ReqwestTransport> {
        ApiClient::new(ClientConfig::new(&self.url).allow_plaintext_http()).unwrap()
    }
}
fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../contract/fixtures/s091/remote-v1.0.json"
    ))
    .unwrap()
}
#[tokio::test]
async fn s091_default_real_tcp_refuses_major_or_missing_capability_before_auth_and_upload() {
    let lab = Lab::new("s091-wire", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.unwrap();
    let login = LoginRequest {
        login: common::LOGIN.into(),
        password: common::PASSWORD.into(),
        device_label: "S091-wire".into(),
    };
    let mut bad = fixture();
    bad["protocol"]["major"] = 2.into();
    let proxy = Proxy::start(running.uri(""), bad).await;
    let client = proxy.client();
    assert!(matches!(
        client.login(&login).await,
        Err(vaultsyncd_http_client::ClientError::Protocol(_))
    ));
    assert_eq!(proxy.calls.load(Ordering::SeqCst), 0);
    assert!(!client.auth().has_credentials());
    drop(proxy);
    let (code, issued) = running
        .json(
            "POST",
            "/v1/auth/login",
            None,
            Some(&serde_json::to_value(&login).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(code, 200);
    let token = issued["access_token"].as_str().unwrap();
    let root = common::register_root(&running, token, &lab.work())
        .await
        .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&mut common::db(&lab).await)
        .await
        .unwrap();
    let mut caps = fixture();
    caps["features"] = json!(["auth", "roots"]);
    let proxy = Proxy::start(running.uri(""), caps).await;
    let client = proxy.client();
    client.auth().install(
        Credentials::from_issued(&serde_json::from_value::<SessionIssued>(issued).unwrap())
            .unwrap(),
    );
    let request = ApiRequest::post(format!("/v1/roots/{root}/uploads"))
        .with_auth(AuthMode::Bearer)
        .with_json(&json!({"hash":"ab".repeat(32),"total_size":"8","chunk_size":"8"}))
        .unwrap();
    assert!(matches!(
        client.send(request).await,
        Err(vaultsyncd_http_client::ClientError::Protocol(_))
    ));
    assert_eq!(proxy.calls.load(Ordering::SeqCst), 0);
    assert!(client.auth().has_credentials());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM uploads")
            .fetch_one(&mut common::db(&lab).await)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions")
            .fetch_one(&mut common::db(&lab).await)
            .await
            .unwrap(),
        before
    );
    drop(proxy);
    let mut future = fixture();
    future["protocol"]["minor"] = 65535.into();
    future["unknown_increment"] = json!({"ignored":true});
    future["features"]
        .as_array_mut()
        .unwrap()
        .push("new_unknown_feature".into());
    let proxy = Proxy::start(running.uri(""), future).await;
    proxy.client().login(&login).await.unwrap();
    assert_eq!(proxy.calls.load(Ordering::SeqCst), 1);
    let response = reqwest::Client::new()
        .get(running.uri("/v1/capabilities"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let current: Value = response.json().await.unwrap();
    assert_eq!(current["protocol"]["major"], 1);
    assert!(current.get("path").is_none());
    assert!(
        current["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "backup_snapshots")
    );
    running.stop().await;
}
