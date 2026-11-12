use crate::bootstrap::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use vaultsyncd_agent_core::{
    adapters::ClientStorePorts,
    bindings::control::BindingControls,
    engine::scheduler::{ActionAdapter, Jitter, Limits, Scheduler},
    executor::{ActionFailure, ActionHandler, ActionOutcome},
    ports::{
        PortFuture, Ports,
        local::*,
        remote::RemotePort,
        state::{ActionIntent, QueuedOperation},
    },
};
use vaultsyncd_client_store::{
    bindings::{self, NewBinding, SyncMode},
    db::Database,
    entries::{self, LocalEntry},
};
use vaultsyncd_domain::{
    ids::{BindingId, DeviceId, EntryId, OpId, ServerId, VaultId, VersionId},
    state::{Dirty, Residency},
    version::{BoundError, Generation, ServerEpoch, UtcTimestamp},
};
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_http_client::{ApiClient, ClientConfig, Credentials};
use vaultsyncd_platform_windows::cfapi::{
    adapter::Adapter,
    catalog::{Catalog, Identity},
};
use vaultsyncd_protocol::generated::{OperationRequest, OperationResult, SessionIssued};
use vaultsyncd_transfer::download::DownloadOptions;
use vaultsyncd_transfer::{
    hash::{digest_of, to_hex},
    ports::{PortError, SnapshotSource, UploadTarget, VersionPin, VersionedContent},
};
struct NoWork;
fn not_used<T, E>() -> PortFuture<'static, Result<T, E>> {
    Box::pin(async { panic!("hydration must not execute synchronization actions") })
}
impl LocalPort for NoWork {
    fn capabilities(&self) -> LocalCapabilities {
        LocalCapabilities::none()
    }
    fn observe(&self, _: &RelativePath) -> PortFuture<'_, Result<LocalFacts, PortError>> {
        not_used()
    }
    fn snapshot(
        &self,
        _: &RelativePath,
        _: Generation,
    ) -> PortFuture<'_, Result<Arc<dyn SnapshotSource>, PortError>> {
        not_used()
    }
    fn apply(&self, _: &ApplyRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        not_used()
    }
    fn move_entry(&self, _: &MoveRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        not_used()
    }
    fn remove(&self, _: &RemoveRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        not_used()
    }
    fn release(&self, _: &ReleaseRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        not_used()
    }
}
impl RemotePort for NoWork {
    fn submit(&self, _: &OperationRequest) -> PortFuture<'_, Result<OperationResult, PortError>> {
        not_used()
    }
    fn operation(
        &self,
        _: BindingId,
        _: OpId,
    ) -> PortFuture<'_, Result<OperationResult, PortError>> {
        not_used()
    }
    fn upload(&self, _: BindingId) -> PortFuture<'_, Result<Arc<dyn UploadTarget>, PortError>> {
        not_used()
    }
    fn content(
        &self,
        _: BindingId,
    ) -> PortFuture<'_, Result<Arc<dyn VersionedContent>, PortError>> {
        not_used()
    }
}
impl ActionHandler for NoWork {
    fn plan_intent<'a>(
        &'a self,
        _: &'a QueuedOperation,
    ) -> PortFuture<'a, Result<Option<ActionIntent>, ActionFailure>> {
        not_used()
    }
    fn execute<'a>(
        &'a self,
        _: &'a Ports,
        _: &'a QueuedOperation,
        _: Option<&'a ActionIntent>,
    ) -> PortFuture<'a, Result<ActionOutcome, ActionFailure>> {
        not_used()
    }
}
impl vaultsyncd_domain::version::Clock for NoWork {
    fn now_utc(&self) -> Result<UtcTimestamp, BoundError> {
        UtcTimestamp::from_unix_seconds(1000)
    }
}
impl Jitter for NoWork {
    fn sample(&self) -> u64 {
        1
    }
}
const SIZE: usize = 256 * 1024;
fn bytes() -> Vec<u8> {
    bytes_for_size(SIZE)
}
fn bytes_for_size(size: usize) -> Vec<u8> {
    (0..size)
        .map(|n| ((n * 37 + n / 128) % 251) as u8)
        .collect()
}
struct Server {
    bytes: Arc<Vec<u8>>,
    size: usize,
    address: String,
    mode: Arc<AtomicU8>,
    requests: Arc<Mutex<Vec<(String, u64)>>>,
    stop: Arc<AtomicBool>,
    head: Arc<AtomicU8>,
    count: Arc<AtomicU64>,
}
impl Server {
    fn new(size: usize) -> Self {
        let content = Arc::new(bytes_for_size(size));
        let shared_content = content.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let mode = Arc::new(AtomicU8::new(0));
        let requests = Arc::new(Mutex::new(vec![]));
        let stop = Arc::new(AtomicBool::new(false));
        let head = Arc::new(AtomicU8::new(1));
        let count = Arc::new(AtomicU64::new(0));
        let (m, r, s, h, c) = (
            mode.clone(),
            requests.clone(),
            stop.clone(),
            head.clone(),
            count.clone(),
        );
        std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("http accept {e}"),
                };
                let (m, r, s, h, c) = (m.clone(), r.clone(), s.clone(), h.clone(), c.clone());
                let full = shared_content.clone();
                std::thread::spawn(move || {
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut header = vec![];
                    let mut byte = [0];
                    while !header.ends_with(b"\r\n\r\n") {
                        if socket.read_exact(&mut byte).is_err() {
                            break;
                        }
                        header.push(byte[0]);
                        if header.len() > 8192 {
                            panic!("oversized header");
                        }
                    }
                    if header.is_empty() {
                        return;
                    }
                    let header = String::from_utf8(header).unwrap();
                    eprintln!("fixture request {:?}", header.lines().next());
                    let path = header
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_owned();
                    if path == "/v1/capabilities" {
                        assert!(!header.to_lowercase().contains("authorization:"));
                        let body = serde_json::to_vec(&serde_json::json!({
                            "protocol": { "major": vaultsyncd_protocol::negotiation::MAJOR, "minor": vaultsyncd_protocol::negotiation::MINOR },
                            "server_id": ServerId::from_bytes([5;16]).to_string(), "epoch": "1",
                            "features": ["content"],
                            "limits": { "max_page_size": 500, "max_upload_part_bytes": "16777216" }
                        })).unwrap();
                        write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                        socket.write_all(&body).unwrap();
                        return;
                    }
                    assert!(
                        header
                            .to_lowercase()
                            .contains("authorization: bearer test-only-access"),
                        "test fixture header: {header:?}"
                    );
                    let range = header
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("range: bytes=")
                                .map(str::to_owned)
                        })
                        .unwrap();
                    let (a, b) = range.split_once('-').unwrap();
                    let (offset, end) = (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap());
                    r.lock().unwrap().push((path.clone(), offset as u64));
                    c.fetch_add(1, Ordering::SeqCst);
                    h.store(2, Ordering::SeqCst);
                    if m.load(Ordering::SeqCst) == 3 && offset > 0 {
                        while m.load(Ordering::SeqCst) == 3 && !s.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                    if m.load(Ordering::SeqCst) == 1 {
                        let _=socket.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }

                    let mut body = full[offset..=end].to_vec();
                    if m.load(Ordering::SeqCst) == 2 {
                        body[0] ^= 1;
                    }
                    assert!(
                        path.contains(&VersionId::from_bytes([4; 16]).to_string()),
                        "request changed fixed version: {path}"
                    );
                    let response = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nx-content-sha256: {}\r\nConnection: close\r\n\r\n",
                        body.len(),
                        offset,
                        end,
                        size,
                        to_hex(&digest_of(&full))
                    );
                    let _ = socket.write_all(response.as_bytes());
                    let _ = socket.write_all(&body);
                });
            }
        });
        Self {
            bytes: content,
            size,
            address,
            mode,
            requests,
            stop,
            head,
            count,
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
fn id(entry: u8) -> Identity {
    Identity {
        binding: BindingId::from_bytes([1; 16]),
        vault: VaultId::from_bytes([2; 16]),
        entry: EntryId::from_bytes([entry; 16]),
        version: VersionId::from_bytes([4; 16]),
    }
}
struct Rig {
    path: PathBuf,
    vault: PathBuf,
    stage: PathBuf,
    db: Arc<Database>,
    server: Server,
}
impl Rig {
    async fn new(tag: &str) -> Self {
        Self::new_size(tag, SIZE).await
    }
    async fn new_size(tag: &str, size: usize) -> Self {
        if std::env::var_os("VAULTSYNCD_SKIP_ABANDONED_HYDRATION_CLEANUP").is_none() {
            cleanup_abandoned_fixture_runs();
        }
        let path = PathBuf::from(format!(
            "C:\\vaultsyncd-lab\\s067-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let vault = path.join("vault");
        let stage = path.join("stage");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::create_dir(&stage).unwrap();
        let db = Arc::new(Database::open(path.join("client.sqlite")).await.unwrap());
        let mut tx = db.begin_write().await.unwrap();
        bindings::insert_binding(
            &mut tx,
            &NewBinding {
                id: id(3).binding,
                server: ServerId::from_bytes([5; 16]),
                vault: id(3).vault,
                device: DeviceId::from_bytes([6; 16]),
                local_path: vault.to_str().unwrap().into(),
                cache_path: stage.to_str().unwrap().into(),
                mode: SyncMode::OnDemand,
                rules_version: "s067".into(),
                server_epoch: ServerEpoch::new(1).unwrap(),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        Self {
            path,
            vault,
            stage,
            db,
            server: Server::new(size),
        }
    }
    fn connect(&self, entry: u8) -> (Adapter, Arc<HydrationCallbacks>) {
        self.connect_timed(entry, Duration::from_secs(15))
    }
    fn connect_timed(&self, entry: u8, timeout: Duration) -> (Adapter, Arc<HydrationCallbacks>) {
        let identity = id(entry);
        let mut cat = Catalog::new(identity.binding, identity.vault);
        cat.insert(identity).unwrap();
        let client =
            ApiClient::new(ClientConfig::new(&self.server.address).allow_plaintext_http()).unwrap();
        client.auth().install(
            Credentials::from_issued(&SessionIssued {
                session_id: "01010101010101010101010101010101".into(),
                access_token: "test-only-access".into(),
                refresh_token: "test-only-refresh".into(),
                access_expires_at: "4000000000".into(),
                refresh_expires_at: "4000000100".into(),
            })
            .unwrap(),
        );
        let state = Arc::new(ClientStorePorts::new(self.db.clone()));
        let unused = Arc::new(NoWork);
        let ports = Ports::new(
            unused.clone(),
            unused.clone(),
            unused.clone(),
            state.clone(),
            state.clone(),
            state,
        );
        let scheduler = Scheduler::new(
            self.db.clone(),
            ports,
            ActionAdapter(NoWork),
            Arc::new(BindingControls::new(self.db.clone())),
            unused,
            Limits::default(),
        )
        .unwrap();
        let pin = VersionPin::new(
            identity.binding.to_string(),
            identity.entry.to_string(),
            identity.version.to_string(),
            digest_of(&self.server.bytes),
            self.server.size as u64,
        )
        .unwrap();
        connect_hydration(
            &self.vault,
            cat,
            self.db.clone(),
            client,
            scheduler.user_reads(),
            HydrationConfig {
                staging: self.stage.clone(),
                versions: vec![PinnedVersion::new(identity, pin).unwrap()],
                download: DownloadOptions::new(if self.server.size > SIZE {
                    1024 * 1024
                } else {
                    65536
                })
                .with_part_concurrency(1),
                timeout,
                outstanding: 8,
            },
        )
        .unwrap()
    }
    async fn local(&self, entry: u8) {
        let mut tx = self.db.begin_write().await.unwrap();
        // The scheduler's real binding gate requires an authenticated baseline path.
        // This fixture serves this exact immutable version through its local HTTP peer.
        let root = EntryId::from_bytes([9; 16]);
        for baseline in [
            entries::BaselineEntry {
                entry: root,
                parent: None,
                name: String::new(),
                kind: entries::EntryKind::Directory,
                revision: vaultsyncd_domain::version::Revision::new(1).unwrap(),
                content_version: None,
                deleted: false,
            },
            entries::BaselineEntry {
                entry: id(entry).entry,
                parent: Some(root),
                name: "file.bin".into(),
                kind: entries::EntryKind::File,
                revision: vaultsyncd_domain::version::Revision::new(1).unwrap(),
                content_version: Some(id(entry).version),
                deleted: false,
            },
        ] {
            entries::put_baseline(&mut tx, id(entry).binding, &baseline)
                .await
                .unwrap();
        }
        entries::put_local(
            &mut tx,
            id(entry).binding,
            &LocalEntry {
                entry: id(entry).entry,
                generation: Generation::new(1).unwrap(),
                dirty: Dirty::Clean,
                residency: Residency::MetadataOnly,
                fingerprint: None,
                file_identity: None,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    async fn finish(self, mut adapter: Adapter, callbacks: Arc<HydrationCallbacks>) {
        callbacks.wait_idle(Duration::from_secs(5)).await.unwrap();
        std::fs::remove_file(self.vault.join("file.bin")).unwrap();
        adapter.disconnect().unwrap();
        drop(callbacks);
        adapter.unregister().unwrap();
        drop(adapter);
        self.db.pool().close().await;
        drop(self.db);
        std::fs::remove_dir_all(&self.path).unwrap();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_http_hydration_fixed_version_and_range_residency() {
    let rig = Rig::new("read").await;
    rig.local(3).await;
    let (adapter, callbacks) = rig.connect(3);
    let mut altered = VersionPin::new(
        id(3).binding.to_string(),
        id(3).entry.to_string(),
        id(3).version.to_string(),
        digest_of(&bytes()),
        SIZE as u64,
    )
    .unwrap();
    altered.digest[0] ^= 1;
    assert!(
        callbacks
            .register_version(PinnedVersion::new(id(3), altered).unwrap())
            .is_err(),
        "same version metadata must be immutable"
    );
    adapter
        .create("file.bin", id(3), SIZE as u64, false)
        .unwrap();
    let path = rig.vault.join("file.bin");
    let actual = tokio::task::spawn_blocking(move || std::fs::read(path))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual, bytes());
    callbacks.wait_idle(Duration::from_secs(5)).await.unwrap();
    assert!(callbacks.stats().served > 0);
    assert_eq!(rig.server.head.load(Ordering::SeqCst), 2);
    assert_eq!(rig.server.requests.lock().unwrap().len(), 4);
    let mut conn = rig.db.pool().acquire().await.unwrap();
    assert_eq!(
        entries::local_entry(&mut conn, id(3).binding, id(3).entry)
            .await
            .unwrap()
            .unwrap()
            .residency,
        Residency::Partial
    );
    drop(conn);
    let meta = adapter.inspect("file.bin").unwrap();
    assert_eq!(meta.cloud.unwrap().on_disk, SIZE as u64);
    rig.finish(adapter, callbacks).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_cancel_preserves_bitmap_reconnect_resumes_and_rereads() {
    let mut rig = Rig::new("cancel").await;
    rig.local(3).await;
    rig.server.mode.store(3, Ordering::SeqCst);
    let (mut adapter, callbacks) = rig.connect(3);
    adapter
        .create("file.bin", id(3), SIZE as u64, false)
        .unwrap();
    let path = rig.vault.join("file.bin");
    let reader = tokio::task::spawn_blocking(move || std::fs::read(path));
    let start = Instant::now();
    while rig.server.count.load(Ordering::SeqCst) < 2 {
        assert!(start.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cancel_hydration(&callbacks, id(3).binding, id(3).entry);
    assert!(reader.await.unwrap().is_err());
    callbacks.wait_idle(Duration::from_secs(5)).await.unwrap();
    assert!(callbacks.stats().cancelled > 0);
    eprintln!(
        "actual_os_cancel_callbacks={}",
        callbacks.stats().cancel_callbacks
    );
    assert_eq!(callbacks.stats().bytes, 0);
    let mut conn = rig.db.pool().acquire().await.unwrap();
    let bitmap: Vec<u8> = sqlx::query_scalar("SELECT bitmap FROM transfers")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(bitmap[0] & 1, 1);
    drop(conn);
    adapter.disconnect().unwrap();
    drop(adapter);
    drop(callbacks);
    rig.db.pool().close().await;
    rig.db = Arc::new(
        Database::open(rig.path.join("client.sqlite"))
            .await
            .unwrap(),
    );
    rig.server.mode.store(0, Ordering::SeqCst);
    let (adapter, callbacks) = rig.connect(3);
    let path = rig.vault.join("file.bin");
    assert_eq!(
        tokio::task::spawn_blocking(move || std::fs::read(path))
            .await
            .unwrap()
            .unwrap(),
        bytes()
    );
    callbacks.wait_idle(Duration::from_secs(5)).await.unwrap();
    let requests = rig.server.requests.lock().unwrap().clone();
    assert_eq!(
        requests.iter().filter(|(_, offset)| *offset == 0).count(),
        1,
        "verified first block must survive reopen: {requests:?}"
    );
    rig.finish(adapter, callbacks).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_offline_and_corruption_never_supply_unverified_bytes() {
    for mode in [1, 2] {
        let rig = Rig::new(&format!("error{mode}")).await;
        rig.local(3).await;
        rig.server.mode.store(mode, Ordering::SeqCst);
        let (adapter, callbacks) = rig.connect(3);
        adapter
            .create("file.bin", id(3), SIZE as u64, false)
            .unwrap();
        let path = rig.vault.join("file.bin");
        let start = Instant::now();
        assert!(
            tokio::task::spawn_blocking(move || std::fs::read(path))
                .await
                .unwrap()
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(10));
        callbacks.wait_idle(Duration::from_secs(5)).await.unwrap();
        assert_eq!(callbacks.stats().bytes, 0);
        assert_eq!(
            adapter.inspect("file.bin").unwrap().cloud.unwrap().on_disk,
            0
        );
        rig.finish(adapter, callbacks).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_verified_whole_download_only_supplies_requested_range() {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    let rig = Rig::new_size("range", 16 * 1024 * 1024).await;
    rig.local(3).await;
    let (adapter, callbacks) = rig.connect(3);
    adapter
        .create("file.bin", id(3), rig.server.size as u64, false)
        .unwrap();
    let path = rig.vault.join("file.bin");
    let hr = tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .access_mode(0x80)
            .share_mode(3)
            .open(path)
            .unwrap();
        unsafe {
            vaultsyncd_platform_windows::cfapi::raw::CfHydratePlaceholder(
                file.as_raw_handle(),
                65536,
                4096,
                0,
                std::ptr::null_mut(),
            )
        }
    })
    .await
    .unwrap();
    assert!(hr >= 0, "range hydrate {hr:08x}");
    callbacks.wait_idle(Duration::from_secs(5)).await.unwrap();
    let local = adapter.inspect("file.bin").unwrap().cloud.unwrap().on_disk;
    assert!(
        local > 0 && local < rig.server.size as u64,
        "range hydration materialized {local} bytes"
    );
    assert_eq!(
        rig.server.requests.lock().unwrap().len(),
        16,
        "whole remote version must be verified before supplying a range"
    );
    assert!(callbacks.stats().bytes < rig.server.size as u64);
    let sealed = rig.stage.join(format!(
        "{}-{}-{}.part",
        id(3).binding,
        id(3).entry,
        id(3).version
    ));
    let external = std::fs::OpenOptions::new()
        .write(true)
        .open(&sealed)
        .unwrap_err();
    assert_eq!(
        external.raw_os_error(),
        Some(32),
        "verified staging must remain write-protected"
    );
    // S199: recovery may close an idle exclusive lease, without evicting bytes or
    // changing the live file's range residency. A later read must revalidate/reuse it.
    callbacks.close_idle_staging().unwrap();
    assert_eq!(std::fs::read(&sealed).unwrap(), *rig.server.bytes);
    assert_eq!(
        adapter.inspect("file.bin").unwrap().cloud.unwrap().on_disk,
        local
    );
    let requests_before = rig.server.requests.lock().unwrap().len();
    let path = rig.vault.join("file.bin");
    let read = tokio::task::spawn_blocking(move || std::fs::read(path))
        .await
        .unwrap();
    callbacks.wait_idle(Duration::from_secs(65)).await.unwrap();
    assert_eq!(
        read.unwrap_or_else(|e| panic!("second read {e}, stats {:?}", callbacks.stats())),
        *rig.server.bytes
    );
    callbacks.wait_idle(Duration::from_secs(5)).await.unwrap();
    assert_eq!(
        rig.server.requests.lock().unwrap().len(),
        requests_before,
        "verified persistent staging must be reusable"
    );
    rig.finish(adapter, callbacks).await;
}

// Only known synthetic run directories from this harness, with no live owner process.
fn cleanup_abandoned_fixture_runs() {
    use vaultsyncd_platform_windows::cfapi::adapter::MetadataOnly;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }
    for child in std::fs::read_dir("C:\\vaultsyncd-lab").unwrap() {
        let child = child.unwrap();
        let name = child.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("s067-") else {
            continue;
        };
        let parts: Vec<_> = rest.split('-').collect();
        if parts.len() != 3
            || !matches!(
                parts[1],
                "read" | "cancel" | "error1" | "error2" | "range" | "timeout"
            )
            || parts[2].parse::<u128>().is_err()
        {
            continue;
        }
        let Ok(pid) = parts[0].parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        if pid != std::process::id() {
            let process = unsafe { OpenProcess(0x1000, 0, pid) };
            if !process.is_null() {
                unsafe {
                    CloseHandle(process);
                };
                continue;
            }
        }
        let path = child.path();
        if path.is_symlink() || !path.is_dir() {
            panic!("unexpected fixture vault");
        }
        for entry in std::fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            assert!(
                matches!(
                    entry.file_name().to_str(),
                    Some(
                        "vault"
                            | "stage"
                            | "client.sqlite"
                            | "client.sqlite-wal"
                            | "client.sqlite-shm"
                    )
                ),
                "unexpected fixture data: {:?}",
                entry.path()
            );
        }
        let vault = path.join("vault");
        let mut catalog = Catalog::new(id(3).binding, id(3).vault);
        catalog.insert(id(3)).unwrap();
        let mut adapter = Adapter::connect(&vault, catalog, Arc::new(MetadataOnly)).unwrap();
        match adapter.inspect("file.bin") {
            Ok(meta) => {
                assert_eq!(
                    Identity::decode(&meta.cloud.unwrap().identity).unwrap(),
                    id(3)
                );
                std::fs::remove_file(vault.join("file.bin")).unwrap();
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("cleanup inspection {error}"),
        }
        adapter.unregister().unwrap();
        drop(adapter);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_timeout_keeps_received_parts_and_never_fakes_success() {
    let rig = Rig::new("timeout").await;
    rig.local(3).await;
    rig.server.mode.store(3, Ordering::SeqCst);
    let (adapter, callbacks) = rig.connect_timed(3, Duration::from_millis(300));
    adapter
        .create("file.bin", id(3), SIZE as u64, false)
        .unwrap();
    let path = rig.vault.join("file.bin");
    let started = Instant::now();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || std::fs::read(path))
        )
        .await
        .unwrap()
        .unwrap()
        .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    callbacks.wait_idle(Duration::from_secs(5)).await.unwrap();
    assert!(callbacks.stats().failed > 0);
    assert_eq!(callbacks.stats().bytes, 0);
    let bitmap: Vec<u8> = sqlx::query_scalar("SELECT bitmap FROM transfers")
        .fetch_one(rig.db.pool())
        .await
        .unwrap();
    assert_eq!(bitmap[0] & 1, 1);
    rig.server.mode.store(0, Ordering::SeqCst);
    rig.finish(adapter, callbacks).await;
}
