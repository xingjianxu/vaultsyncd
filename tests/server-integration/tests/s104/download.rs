//! T22：真实TCP中断、真实版本响应的错块/混版本注入，使用产品HTTP与下载执行器。
use super::*;
use std::{
    fs::File,
    os::unix::fs::FileExt,
    sync::{Arc, Mutex},
};
use vaultsyncd_http_client::{
    ApiClient, ClientConfig, Transport, TransportError, TransportErrorKind,
    transport::{TransportFuture, TransportRequest, TransportResponse},
};
use vaultsyncd_transfer::{
    bitmap::PartBitmap,
    chunk::ChunkGeometry,
    download::{self, DownloadOptions},
    ports::{
        DownloadKey, DownloadProgressStore, LocalErrorKind, PortError, PortFuture, StoredProgress,
        TempContent, VersionPin,
    },
};

struct FileTemp {
    path: String,
}
fn local(error: impl std::fmt::Display) -> PortError {
    PortError::local(LocalErrorKind::Write, error.to_string())
}
impl FileTemp {
    fn file(&self) -> Result<File, PortError> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&self.path)
            .map_err(local)
    }
}
impl TempContent for FileTemp {
    fn path(&self) -> &str {
        &self.path
    }
    fn ensure_capacity(&self, needed: u64) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let parent = Path::new(&self.path).parent().unwrap();
            let available = vaultsyncd_platform_linux_server::restore::Target::open(parent)
                .map_err(local)?
                .available()
                .map_err(local)?;
            if available < needed {
                return Err(PortError::local(
                    LocalErrorKind::NoSpace,
                    "test volume capacity insufficient",
                ));
            }
            self.file()?.set_len(needed).map_err(local)
        })
    }
    fn write_at<'a>(
        &'a self,
        offset: u64,
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move { self.file()?.write_all_at(bytes, offset).map_err(local) })
    }
    fn read_at(&self, offset: u64, len: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            let mut b = vec![0; len as usize];
            self.file()?.read_exact_at(&mut b, offset).map_err(local)?;
            Ok(b)
        })
    }
    fn sync(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.file()?.sync_all().map_err(local) })
    }
    fn discard(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(local(e)),
            }
        })
    }
}
struct FileProgress {
    path: PathBuf,
}
impl DownloadProgressStore for FileProgress {
    fn load<'a>(
        &'a self,
        key: &'a DownloadKey,
    ) -> PortFuture<'a, Result<Option<StoredProgress>, PortError>> {
        Box::pin(async move {
            let bytes = match std::fs::read(&self.path) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(local(e)),
            };
            let v: Value = serde_json::from_slice(&bytes).map_err(local)?;
            assert_eq!(v["key"], key.as_key());
            let chunk = v["chunk"].as_u64().unwrap();
            let size = v["size"].as_u64().unwrap();
            let bitmap = PartBitmap::from_bytes(
                ChunkGeometry::new(size, chunk).unwrap(),
                serde_json::from_value(v["bitmap"].clone()).unwrap(),
            )
            .unwrap();
            Ok(Some(
                StoredProgress::new(chunk, size, v["temp"].as_str().unwrap(), bitmap).unwrap(),
            ))
        })
    }
    fn save<'a>(
        &'a self,
        key: &'a DownloadKey,
        p: &'a StoredProgress,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let v = json!({"key":key.as_key(),"chunk":p.chunk_size(),"size":p.total_size(),"temp":p.temp_path(),"bitmap":p.bitmap().bytes()});
            let tmp = self.path.with_extension("new");
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(local)?;
            f.write_all(&serde_json::to_vec(&v).unwrap())
                .map_err(local)?;
            f.sync_all().map_err(local)?;
            std::fs::rename(&tmp, &self.path).map_err(local)?;
            File::open(self.path.parent().unwrap())
                .map_err(local)?
                .sync_all()
                .map_err(local)
        })
    }
    fn discard<'a>(&'a self, _: &'a DownloadKey) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(local(e)),
            }
        })
    }
}

#[derive(Clone)]
enum Mode {
    Normal,
    Disconnect,
    WrongBlock,
    MixedVersion(String),
}
struct Forward {
    base: Mutex<String>,
    mode: Mutex<Mode>,
    control: PathBuf,
    requests: Mutex<Vec<String>>,
}
impl Transport for Forward {
    fn send(
        &self,
        request: TransportRequest,
    ) -> TransportFuture<'_, Result<TransportResponse, TransportError>> {
        Box::pin(async move {
            let logical = reqwest::Url::parse(request.url()).unwrap();
            let mut url = format!("{}{}", self.base.lock().unwrap(), logical.path());
            if let Some(q) = logical.query() {
                url.push('?');
                url.push_str(q);
            }
            let range = request.header("range").map(str::to_owned);
            let mode = self.mode.lock().unwrap().clone();
            if let Some(range) = &range {
                self.requests.lock().unwrap().push(range.clone());
                if matches!(mode, Mode::Disconnect) && range.starts_with("bytes=65536-") {
                    std::fs::write(self.control.join("armed"), "download_open").unwrap();
                }
                if let Mode::MixedVersion(version) = &mode {
                    let segments: Vec<_> = url.split('/').collect();
                    let old = segments[segments.len() - 2];
                    url = url.replace(
                        &format!("/versions/{old}/"),
                        &format!("/versions/{version}/"),
                    );
                }
            }
            let client = reqwest::Client::new();
            let method = reqwest::Method::from_bytes(request.method().as_str().as_bytes()).unwrap();
            let mut outgoing = client.request(method, url);
            for (k, v) in request.headers() {
                outgoing = outgoing.header(k, v);
            }
            if let Some(body) = request.body() {
                outgoing = outgoing.body(body.to_vec());
            }
            let result = outgoing
                .send()
                .await
                .map_err(|e| TransportError::new(TransportErrorKind::Io, e.to_string()))?;
            let status = result.status().as_u16();
            let headers = result
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
                .collect();
            let mut bytes = result
                .bytes()
                .await
                .map_err(|e| TransportError::new(TransportErrorKind::Io, e.to_string()))?
                .to_vec();
            // 仅篡改真实响应字节；保持原始整份摘要头，要求产品下载执行器在最终hash时拒绝。
            if range.is_some() && matches!(mode, Mode::WrongBlock) && !bytes.is_empty() {
                bytes[0] ^= 0x81;
            }
            Ok(TransportResponse::new(status, headers, bytes))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "S104 real TCP and dedicated btrfs required"]
async fn download_tcp_cut_resumes_only_missing_ranges_and_rejects_corruption_or_other_version() {
    let bytes = deterministic(4 * 65536 + 117);
    let (lab, server, token, vault, old_entry) = setup("download", &bytes).await;
    let epoch = epoch_of(&server.http, &token, &vault).await;
    let other: Vec<_> = bytes.iter().map(|b| b ^ 0x5a).collect();
    let (uploaded, _, _) = upload(&server.http, &token, &vault, &other, 65536)
        .await
        .unwrap();
    let (status,changed)=submit(&server.http,&token,&vault,json!({"op_id":identity(0x61),"device_id":identity(0x62),"server_epoch":epoch,"kind":"put_content","entry_id":old_entry["entry_id"],"base_revision":old_entry["revision"],"upload_id":uploaded,"local_generation":"1","conflict_policy":"preserve_both"})).await;
    assert_eq!(status, 200, "{changed}");
    let new_entry = entry_named(&snapshot(&server.http, &token, &vault).await, "note").clone();
    prepare(&lab, BACKUP).await;
    let transport = Arc::new(Forward {
        base: Mutex::new(server.http.uri("")),
        mode: Mutex::new(Mode::Disconnect),
        control: control(&lab),
        requests: Mutex::new(Vec::new()),
    });
    // 自定义测试传输把逻辑HTTPS URL映射到本run回环HTTP；产品TLS构造器/验证策略未改动。
    let client =
        ApiClient::with_transport(ClientConfig::new("https://s104.invalid"), transport.clone())
            .unwrap();
    client
        .login(&vaultsyncd_protocol::generated::LoginRequest {
            hostname: None,
            operating_system: None,
            login: common::LOGIN.into(),
            password: common::PASSWORD.into(),
            device_label: "S104 TCP download".into(),
        })
        .await
        .unwrap();
    let content =
        vaultsyncd_http_client::content::ContentEndpoint::new(client, vault.clone()).unwrap();
    let pin = VersionPin::new(
        identity(0x63),
        old_entry["entry_id"].as_str().unwrap(),
        old_entry["version_id"].as_str().unwrap(),
        digest(&bytes),
        bytes.len() as u64,
    )
    .unwrap();
    let opts = DownloadOptions::new(65536)
        .with_part_concurrency(1)
        .with_content_attempts(1);
    let staging = lab.state().join("download-staging");
    std::fs::create_dir(&staging).unwrap();
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700)).unwrap();
    let temporary = Arc::new(FileTemp {
        path: staging.join("candidate").to_string_lossy().into_owned(),
    });
    let progress = Arc::new(FileProgress {
        path: lab.state().join("download-progress.json"),
    });
    let saved = lab.state().join("existing-saved-content");
    std::fs::write(&saved, b"unique local saved bytes").unwrap();
    let request = {
        let content = content.clone();
        let t = temporary.clone();
        let p = progress.clone();
        let pin = pin.clone();
        let opts = opts.clone();
        tokio::spawn(
            async move { download::run(&content, t.as_ref(), p.as_ref(), &pin, &opts).await },
        )
    };
    reached(&lab, "download_open").await;
    gc_start(&lab, now()).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(!control(&lab).join("gc-result").exists());
    server.kill();
    assert!(request.await.unwrap().is_err());
    let key = DownloadKey::new(&pin.binding, &pin.entry, &pin.version).unwrap();
    assert_eq!(
        progress.load(&key).await.unwrap().unwrap().bitmap().count(),
        1
    );
    assert_eq!(std::fs::read(&saved).unwrap(), b"unique local saved bytes");
    arm(&lab, &[]);
    let server = Server::start(&lab).await;
    *transport.base.lock().unwrap() = server.http.uri("");
    *transport.mode.lock().unwrap() = Mode::Normal;
    transport.requests.lock().unwrap().clear();
    // 重新打开实际磁盘的候选和进度，不能靠上一次调用的内存位图续传。
    let reopened = FileTemp {
        path: temporary.path.clone(),
    };
    let reloaded = FileProgress {
        path: progress.path.clone(),
    };
    let outcome = download::run(&content, &reopened, &reloaded, &pin, &opts)
        .await
        .unwrap();
    assert!(outcome.resumed);
    assert_eq!(outcome.downloaded_parts, 4);
    assert_eq!(std::fs::read(&reopened.path).unwrap(), bytes);
    let requests = transport.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(!requests.iter().any(|r| r.starts_with("bytes=0-")));
    trace(
        json!({"event":"tcp-resume-pass","persisted_parts":1,"resumed_ranges":requests,"transferred_after_restart":bytes.len()-65536,"retransmitted_confirmed_prefix":0}),
    );
    for (label, mode) in [
        ("wrong-block", Mode::WrongBlock),
        (
            "mixed-version",
            Mode::MixedVersion(new_entry["version_id"].as_str().unwrap().into()),
        ),
    ] {
        *transport.mode.lock().unwrap() = mode;
        let candidate = FileTemp {
            path: staging.join(label).to_string_lossy().into_owned(),
        };
        let bitmap = FileProgress {
            path: lab.state().join(format!("{label}.json")),
        };
        let result = download::run(&content, &candidate, &bitmap, &pin, &opts).await;
        assert!(
            result.is_err(),
            "must not offer incomplete or mixed bytes for installation"
        );
        assert_eq!(std::fs::read(&saved).unwrap(), b"unique local saved bytes");
        trace(
            json!({"event":"download-rejected","fault":label,"error":result.unwrap_err().to_string(),"local_original_preserved":true}),
        );
    }
    assert!(gc(&lab, now() + 3601).await.contains("objects=0"));
    assert_eq!(all_objects(&lab).await, 2);
    release(&lab, BACKUP).await;
    server.kill();
}
