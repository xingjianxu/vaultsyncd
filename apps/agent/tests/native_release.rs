//! Native CFAPI/SQLite/real Linux HTTP release acceptance. Invoked by run-native-release.sh.
#![cfg(windows)]
#![allow(unsafe_code)]
use serde_json::Value;
use std::{
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::PathBuf,
    sync::Arc,
};
use vaultsyncd_agent::release::{HttpReleaseRemote, NativeReleaseLocal};
use vaultsyncd_agent_core::{
    executor::{apply::ControlledLocal, release::*},
    recovery::ContentEvidence,
};
use vaultsyncd_client_store::{
    bindings::{self, NewBinding, SyncMode},
    db::Database,
    entries::{self, BaselineEntry, EntryKind, LocalEntry},
};
use vaultsyncd_domain::{
    ids::*,
    state::{Dirty, Residency},
    version::{Generation, Revision, ServerEpoch},
};
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_http_client::{ApiClient, ClientConfig, Credentials, ReqwestTransport};
use vaultsyncd_platform_windows::{
    cfapi::{
        adapter::{Adapter, MetadataOnly},
        catalog::{Catalog, Identity},
    },
    files::RootHandle,
};
use vaultsyncd_transfer::{
    hash::from_hex,
    ports::{PortError, PortFuture, VersionPin},
};

#[link(name = "cldapi")]
unsafe extern "system" {
    fn CfConvertToPlaceholder(
        handle: *mut std::ffi::c_void,
        identity: *const std::ffi::c_void,
        length: u32,
        flags: u32,
        usn: *mut i64,
        overlapped: *mut std::ffi::c_void,
    ) -> i32;
}
const B: BindingId = BindingId::from_bytes([0x69; 16]);
const PARENT: EntryId = EntryId::from_bytes([0x68; 16]);
fn manifest() -> Value {
    serde_json::from_slice(
        &std::fs::read(
            std::env::var_os("VAULTSYNCD_RELEASE_FIXTURE")
                .expect("run-native-release.sh supplies real HTTP fixture"),
        )
        .unwrap(),
    )
    .unwrap()
}
fn client(v: &Value, url: Option<&str>) -> ApiClient<ReqwestTransport> {
    let c = ApiClient::new(
        ClientConfig::new(url.unwrap_or(v["url"].as_str().unwrap())).allow_plaintext_http(),
    )
    .unwrap();
    c.auth().install(
        Credentials::from_issued(&serde_json::from_value(v["issued"].clone()).unwrap()).unwrap(),
    );
    c
}
fn identity(v: &Value) -> Identity {
    Identity {
        binding: B,
        root: RootId::from_hex(v["root"].as_str().unwrap()).unwrap(),
        entry: EntryId::from_hex(v["entry"]["entry_id"].as_str().unwrap()).unwrap(),
        version: VersionId::from_hex(v["entry"]["version_id"].as_str().unwrap()).unwrap(),
    }
}
fn pin(v: &Value) -> VersionPin {
    let id = identity(v);
    VersionPin::new(
        B.to_string(),
        id.entry.to_string(),
        id.version.to_string(),
        from_hex(v["entry"]["hash"].as_str().unwrap()).unwrap(),
        v["entry"]["size"].as_str().unwrap().parse().unwrap(),
    )
    .unwrap()
}
fn spec(v: &Value, marker: u8) -> ReleaseSpec {
    ReleaseSpec {
        op: OpId::from_bytes([marker; 16]),
        binding: B,
        entry: identity(v).entry,
        path: RelativePath::parse("file.txt").unwrap(),
        pin: pin(v),
    }
}
struct Fixture {
    path: PathBuf,
    root: PathBuf,
    db: Arc<Database>,
    adapter: Adapter,
    v: Value,
}
impl Fixture {
    async fn prepare(path: PathBuf) -> Self {
        Self::prepare_version(path, false).await
    }
    async fn prepare_version(path: PathBuf, missing_version: bool) -> Self {
        let mut v = manifest();
        if missing_version {
            v["entry"]["version_id"] = Value::String("f".repeat(32));
        }
        let root = path.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(path.join("cache")).unwrap();
        let id = identity(&v);
        let mut catalog = Catalog::new(B, id.root);
        catalog.insert(id).unwrap();
        let adapter = Adapter::connect(&root, catalog, Arc::new(MetadataOnly)).unwrap();
        std::fs::write(root.join("file.txt"), v["payload"].as_str().unwrap()).unwrap();
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(7)
            .open(root.join("file.txt"))
            .unwrap();
        let bytes = id.encode();
        let mut usn = 0;
        let hr = unsafe {
            CfConvertToPlaceholder(
                f.as_raw_handle(),
                bytes.as_ptr().cast(),
                bytes.len() as u32,
                1,
                &mut usn,
                std::ptr::null_mut(),
            )
        };
        assert!(hr >= 0, "convert {hr:08x}");
        drop(f);
        let metadata = RootHandle::open(&root)
            .unwrap()
            .inspect("file.txt")
            .unwrap();
        assert!(metadata.cloud.as_ref().unwrap().on_disk >= pin(&v).size);
        let mut physical = metadata.identity.volume.to_le_bytes().to_vec();
        physical.extend(metadata.identity.file);
        let db = Arc::new(Database::open(path.join("client.sqlite")).await.unwrap());
        let mut tx = db.begin_write().await.unwrap();
        bindings::insert_binding(
            &mut tx,
            &NewBinding {
                id: B,
                server: ServerId::from_bytes([1; 16]),
                root: id.root,
                device: DeviceId::from_bytes([2; 16]),
                local_path: root.to_str().unwrap().into(),
                cache_path: path.join("cache").to_str().unwrap().into(),
                mode: SyncMode::OnDemand,
                rules_version: "S069".into(),
                server_epoch: ServerEpoch::new(1).unwrap(),
            },
        )
        .await
        .unwrap();
        for base in [
            BaselineEntry {
                entry: PARENT,
                parent: None,
                name: "".into(),
                kind: EntryKind::Directory,
                revision: Revision::new(1).unwrap(),
                content_version: None,
                deleted: false,
            },
            BaselineEntry {
                entry: id.entry,
                parent: Some(PARENT),
                name: "file.txt".into(),
                kind: EntryKind::File,
                revision: Revision::new(1).unwrap(),
                content_version: Some(id.version),
                deleted: false,
            },
        ] {
            entries::put_baseline(&mut tx, B, &base).await.unwrap();
        }
        entries::put_local(
            &mut tx,
            B,
            &LocalEntry {
                entry: id.entry,
                generation: Generation::new(1).unwrap(),
                dirty: Dirty::Clean,
                residency: Residency::Materialized,
                fingerprint: Some(pin(&v).digest.to_vec()),
                file_identity: Some(physical),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        Self {
            path,
            root,
            db,
            adapter,
            v,
        }
    }
    async fn load(path: PathBuf) -> Self {
        let v = manifest();
        let root = path.join("root");
        let id = identity(&v);
        let mut cat = Catalog::new(B, id.root);
        cat.insert(id).unwrap();
        let adapter = Adapter::connect(&root, cat, Arc::new(MetadataOnly)).unwrap();
        let db = Arc::new(Database::open(path.join("client.sqlite")).await.unwrap());
        Self {
            path,
            root,
            db,
            adapter,
            v,
        }
    }
    fn service(&self, phase: u8, offline: bool) -> ManualRelease {
        let local = Arc::new(
            NativeReleaseLocal::new(
                &self.root,
                self.db.clone(),
                identity(&self.v),
                RelativePath::parse("file.txt").unwrap(),
                pin(&self.v),
            )
            .unwrap(),
        );
        ManualRelease::new(
            self.db.clone(),
            Arc::new(HookLocal {
                inner: local,
                phase,
            }),
            Arc::new(
                HttpReleaseRemote::new(
                    client(&self.v, offline.then_some("http://127.0.0.1:9")),
                    identity(&self.v).root,
                )
                .unwrap(),
            ),
        )
    }
    fn full_bytes(&self) {
        assert_eq!(
            std::fs::read(self.root.join("file.txt")).unwrap(),
            self.v["payload"].as_str().unwrap().as_bytes()
        );
    }
    async fn local(&self) -> LocalEntry {
        let mut c = self.db.pool().acquire().await.unwrap();
        entries::local_entry(&mut c, B, identity(&self.v).entry)
            .await
            .unwrap()
            .unwrap()
    }
    async fn finish(mut self) {
        self.db.pool().close().await;
        drop(self.db);
        std::fs::remove_file(self.root.join("file.txt")).unwrap();
        self.adapter.unregister().unwrap();
        drop(self.adapter);
        for attempt in 0..100 {
            match std::fs::remove_dir_all(&self.path) {
                Ok(()) => return,
                Err(e) if attempt < 99 && e.raw_os_error() == Some(32) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await
                }
                Err(e) => panic!(
                    "fixture cleanup {}: {e}; entries {:?}",
                    self.path.display(),
                    std::fs::read_dir(&self.path).map(|it| it
                        .filter_map(Result::ok)
                        .map(|e| e.path())
                        .collect::<Vec<_>>())
                ),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_sqlite_full_refuses_release_and_keeps_local_copy() {
    let f = Fixture::prepare(run_path("sqlite-full")).await;
    let mut conn = f.db.pool().acquire().await.unwrap();
    // max_page_count belongs to this connection's pager. Reserve the other pool slots so the
    // actual release uses the same exhausted pager, instead of accidentally opening an uncapped one.
    let mut reserved = Vec::new();
    for _ in 0..3 {
        reserved.push(f.db.pool().acquire().await.unwrap());
    }
    let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let _: i64 = sqlx::query_scalar(&format!("PRAGMA max_page_count={}", pages + 20))
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let mut id = 1u128;
    for length in [3500, 1000, 100, 1] {
        let json = format!("{{\"padding\":\"{}\"}}", "x".repeat(length));
        let mut saw_full = false;
        for _ in 0..5000 {
            let result=sqlx::query("INSERT INTO controlled_releases(op_id,binding_id,target_path,intent_id,journal,completed) VALUES(?,?,'capacity-only',NULL,?,1)")
                .bind(id.to_le_bytes().as_slice()).bind(B.as_bytes().as_slice()).bind(&json).execute(&mut *conn).await;
            id += 1;
            if let Err(e) = result {
                assert!(e.to_string().contains("full"), "actual SQLite error: {e}");
                saw_full = true;
                break;
            }
        }
        assert!(saw_full, "dedicated SQLite pager reached SQLITE_FULL");
    }
    drop(conn);
    let error = f
        .service(0, false)
        .release(&spec(&f.v, 0x7f))
        .await
        .unwrap_err();
    assert!(
        error.is_no_space(),
        "capacity error must retain typed storage classification: {error}"
    );
    eprintln!("actual bounded SQLite capacity release error: {error}");
    f.full_bytes();
    assert_eq!(f.local().await.residency, Residency::Materialized);
    drop(reserved);
    f.finish().await;
}
fn run_path(tag: &str) -> PathBuf {
    PathBuf::from(format!(
        "C:\\vaultsyncd-lab\\s069-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}
struct HookLocal {
    inner: Arc<NativeReleaseLocal>,
    phase: u8,
}
impl ReleaseLocal for HookLocal {
    fn protect<'a>(
        &'a self,
        path: &'a RelativePath,
    ) -> PortFuture<'a, Result<Box<dyn ReleaseProtection>, PortError>> {
        Box::pin(async move {
            Ok(Box::new(HookGuard {
                inner: self.inner.protect(path).await?,
                phase: self.phase,
            }) as Box<dyn ReleaseProtection>)
        })
    }
}
struct HookGuard {
    inner: Box<dyn ReleaseProtection>,
    phase: u8,
}
impl ReleaseProtection for HookGuard {
    fn content(&self) -> &dyn ControlledLocal {
        self.inner.content()
    }
    fn released(&self) -> PortFuture<'_, Result<Option<ReleasedFile>, PortError>> {
        self.inner.released()
    }
    fn dehydrate_verified<'a>(
        &'a self,
        before: &'a ContentEvidence,
        lease: &'a dyn VersionLease,
    ) -> PortFuture<'a, Result<ReleasedFile, PortError>> {
        Box::pin(async move {
            if self.phase == 1 {
                std::process::exit(81)
            }
            let r = self.inner.dehydrate_verified(before, lease).await?;
            if self.phase == 2 {
                std::process::exit(82)
            }
            Ok(r)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_release_uses_real_lease_and_preserves_metadata() {
    let f = Fixture::prepare(run_path("success")).await;
    let before = f.adapter.inspect("file.txt").unwrap();
    let receipt = f
        .service(0, false)
        .release(&spec(&f.v, 0x69))
        .await
        .unwrap();
    assert_eq!(receipt.pin, pin(&f.v));
    let after = f.adapter.inspect("file.txt").unwrap();
    assert_eq!(before.identity, after.identity);
    assert_eq!(before.size, after.size);
    assert_eq!(
        after.cloud.as_ref().unwrap().identity,
        before.cloud.unwrap().identity
    );
    assert_eq!(after.cloud.unwrap().on_disk, 0);
    assert_eq!(f.local().await.residency, Residency::MetadataOnly);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_owned_release_protection_stays_active_across_await() {
    let f = Fixture::prepare(run_path("owned-protection")).await;
    let local = NativeReleaseLocal::new(
        &f.root,
        f.db.clone(),
        identity(&f.v),
        RelativePath::parse("file.txt").unwrap(),
        pin(&f.v),
    )
    .unwrap();
    let target = RelativePath::parse("file.txt").unwrap();
    let held = local.protect(&target).await.unwrap();
    let path = f.root.join("file.txt");
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(7)
            .open(path)
            .map(drop);
        sender.send(result).unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let completed = match receiver.try_recv() {
        Ok(Ok(())) => panic!("writer passed continuous release protection"),
        Ok(Err(_)) => true,
        Err(std::sync::mpsc::TryRecvError::Empty) => false,
        Err(e) => panic!("writer channel: {e}"),
    };
    drop(held);
    if !completed {
        receiver
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap()
            .unwrap();
    }
    worker.join().unwrap();
    drop(local);
    f.full_bytes();
    f.finish().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_dirty_busy_and_network_failures_keep_unique_bytes() {
    for case in 0..5 {
        let f = Fixture::prepare_version(run_path(&format!("refuse-{case}")), case == 4).await;
        let lock = if case == 1 {
            Some(
                std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(f.root.join("file.txt"))
                    .unwrap(),
            )
        } else {
            None
        };
        if case == 0 {
            let mut tx = f.db.begin_write().await.unwrap();
            let mut state = f.local().await;
            state.dirty = Dirty::Dirty;
            entries::put_local(&mut tx, B, &state).await.unwrap();
            tx.commit().await.unwrap();
        }
        if case == 3 {
            std::fs::write(f.root.join("file.txt"), b"UNCONFIRMED unique bytes").unwrap();
        }
        let refused = f
            .service(0, case == 2)
            .release(&spec(&f.v, 0x70 + case))
            .await
            .unwrap_err();
        if case == 4 {
            assert!(
                refused.to_string().contains("VERSION_GONE"),
                "actual remote fixed-version refusal: {refused}"
            );
        }
        drop(lock);
        if case == 3 {
            assert_eq!(
                std::fs::read(f.root.join("file.txt")).unwrap(),
                b"UNCONFIRMED unique bytes"
            )
        } else {
            f.full_bytes()
        }
        assert_eq!(f.local().await.residency, Residency::Materialized);
        f.finish().await;
    }
}
#[test]
fn native_release_crash_recovery() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    if let Some(phase) = std::env::var_os("S069_CRASH_PHASE") {
        let phase: u8 = phase.to_str().unwrap().parse().unwrap();
        let path = PathBuf::from(std::env::var_os("S069_CRASH_PATH").unwrap());
        runtime.block_on(async move {
            let f = Fixture::prepare(path).await;
            f.service(phase, false)
                .release(&spec(&f.v, 0x75))
                .await
                .unwrap();
            assert_eq!(phase, 3);
            std::process::exit(83)
        });
    }
    for phase in 1..=3u8 {
        let path = run_path(&format!("crash-{phase}"));
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_release_crash_recovery",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("S069_CRASH_PHASE", phase.to_string())
            .env("S069_CRASH_PATH", &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(80 + phase as i32));
        runtime.block_on(async {
            let f = Fixture::load(path).await;
            let recovered = f.service(0, false).recover(B).await.unwrap();
            for (_, result) in recovered {
                result.unwrap();
            }
            assert_eq!(f.local().await.residency, Residency::MetadataOnly);
            assert_eq!(
                f.adapter
                    .inspect("file.txt")
                    .unwrap()
                    .cloud
                    .unwrap()
                    .on_disk,
                0
            );
            let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM local_intents")
                .fetch_one(f.db.pool())
                .await
                .unwrap();
            assert_eq!(pending, 0);
            f.finish().await;
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a dedicated run_id NTFS VHD from run-native-release.sh capacity mode"]
async fn native_ntfs_exhaustion_keeps_downloaded_bytes_and_reports_no_space() {
    use std::io::Write;
    let volume = PathBuf::from(
        std::env::var_os("VAULTSYNCD_RELEASE_FULL_VOLUME").expect("dedicated NTFS volume"),
    );
    assert!(volume.is_absolute() && volume.components().count() == 2);
    let f = Fixture::prepare(volume.join(format!("s105-full-{}", std::process::id()))).await;
    let mut connection = f.db.pool().acquire().await.unwrap();
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&mut *connection)
        .await
        .unwrap();
    drop(connection);
    let fill_path = volume.join("s105-allocated-data.bin");
    let mut fill = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&fill_path)
        .unwrap();
    let mut wrote = 0u64;
    for chunk in [vec![0x5a; 1024 * 1024], vec![0x5a; 4096]] {
        loop {
            match fill.write_all(&chunk) {
                Ok(()) => wrote += chunk.len() as u64,
                Err(e) => {
                    assert!(
                        matches!(e.raw_os_error(), Some(112 | 39)),
                        "actual NTFS full error: {e}"
                    );
                    break;
                }
            }
            assert!(
                wrote < 256 * 1024 * 1024,
                "test must only fill the bounded 128MiB VHD"
            );
        }
    }
    let _ = fill.sync_all();
    assert!(
        wrote > 64 * 1024 * 1024,
        "must allocate real bytes, not sparse metadata"
    );
    let before = f.adapter.inspect("file.txt").unwrap();
    assert!(
        before.cloud.unwrap().on_disk >= pin(&f.v).size,
        "storage pressure must not autonomously evict downloaded content"
    );
    f.full_bytes();
    let rejected = f
        .service(0, false)
        .release(&spec(&f.v, 0xa5))
        .await
        .unwrap_err();
    assert!(
        rejected.is_no_space(),
        "actual NTFS exhaustion must remain typed: {rejected}"
    );
    f.full_bytes();
    drop(fill);
    std::fs::remove_file(fill_path).unwrap();
    assert_eq!(f.local().await.residency, Residency::Materialized);
    eprintln!(
        "check=pass s105_real_ntfs_enospc allocated={wrote} complete_cache_retained typed_no_space=true"
    );
    f.finish().await;
}
