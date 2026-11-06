//! 真实SQLite/WAL、磁盘字节、租约保护对象与独立进程退出；不是Windows平台能力证明。
use super::*;
use crate::{
    executor::apply::ContentObservation,
    ports::local::{ApplyReceipt, ApplyRequest, LocalEntryKind, LocalFacts},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
};
use vaultsyncd_client_store::{
    bindings::{self, NewBinding, SyncMode},
    entries::{self, BaselineEntry, EntryKind, LocalEntry},
    intents::{self, LocalIntentKind, NewLocalIntent},
    queue::{self, NewPendingOp},
};
use vaultsyncd_domain::{
    ids::{DeviceId, RootId, ServerId, VersionId},
    state::{Dirty, Residency, Usage},
    version::{Generation, Revision, ServerEpoch},
};
use vaultsyncd_transfer::{
    hash::{digest_of, to_hex},
    ports::{LocalErrorKind, SnapshotSource},
};

const B: BindingId = BindingId::from_bytes([0x63; 16]);
const E: EntryId = EntryId::from_bytes([2; 16]);
const ROOT: EntryId = EntryId::from_bytes([1; 16]);
const V: VersionId = VersionId::from_bytes([3; 16]);
const O: OpId = OpId::from_bytes([4; 16]);
const OLD: &[u8] = b"confirmed content retained by a leased immutable version";
const EDIT: &[u8] = b"unconfirmed same-path content must never be released";

#[tokio::test]
async fn typed_startup_specs_refuse_malformed_durable_journal_without_io() {
    let rig = Rig::new().await;
    let db = rig.db().await;
    sqlx::query(
        "INSERT INTO controlled_releases(op_id,binding_id,target_path,journal) VALUES(?,?,?,'{}')",
    )
    .bind(O.as_bytes().as_slice())
    .bind(B.as_bytes().as_slice())
    .bind("file.txt")
    .execute(db.pool())
    .await
    .unwrap();
    assert!(ManualRelease::pending_specs(db.clone(), B).await.is_err());
    assert_eq!(fs::read(rig.0.join("file.txt")).unwrap(), OLD);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM controlled_releases")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        1
    );
}
fn err(e: impl std::fmt::Display) -> PortError {
    PortError::local(LocalErrorKind::Read, e.to_string())
}
fn persist(path: &Path, bytes: &[u8]) {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}
fn spec() -> ReleaseSpec {
    ReleaseSpec {
        op: O,
        binding: B,
        entry: E,
        path: RelativePath::parse("file.txt").unwrap(),
        pin: VersionPin::new(
            B.to_string(),
            E.to_string(),
            V.to_string(),
            digest_of(OLD),
            OLD.len() as u64,
        )
        .unwrap(),
    }
}
struct Rig(PathBuf);
impl Rig {
    async fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "vaultsyncd-s063-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        persist(&path.join("file.txt"), OLD);
        persist(&path.join("object"), OLD);
        let db = Arc::new(Database::open(path.join("client.sqlite")).await.unwrap());
        let disk = Disk::new(path.clone());
        let actual = recovery::inspect(&disk, &spec().path).await.unwrap();
        let ContentEvidence::File {
            generation,
            identity,
            digest,
            ..
        } = actual
        else {
            panic!()
        };
        let mut tx = db.begin_write().await.unwrap();
        bindings::insert_binding(
            &mut tx,
            &NewBinding {
                id: B,
                server: ServerId::from_bytes([5; 16]),
                device: DeviceId::from_bytes([6; 16]),
                root: RootId::from_bytes([7; 16]),
                local_path: path.to_str().unwrap().into(),
                cache_path: path.join("cache").to_str().unwrap().into(),
                mode: SyncMode::OnDemand,
                rules_version: "initial".into(),
                server_epoch: ServerEpoch::new(1).unwrap(),
            },
        )
        .await
        .unwrap();
        for base in [
            BaselineEntry {
                entry: ROOT,
                parent: None,
                name: "".into(),
                kind: EntryKind::Directory,
                revision: Revision::new(1).unwrap(),
                content_version: None,
                deleted: false,
            },
            BaselineEntry {
                entry: E,
                parent: Some(ROOT),
                name: "file.txt".into(),
                kind: EntryKind::File,
                revision: Revision::new(2).unwrap(),
                content_version: Some(V),
                deleted: false,
            },
        ] {
            entries::put_baseline(&mut tx, B, &base).await.unwrap();
        }
        entries::put_local(
            &mut tx,
            B,
            &LocalEntry {
                entry: E,
                generation: Generation::new(generation).unwrap(),
                dirty: Dirty::Clean,
                residency: Residency::Materialized,
                fingerprint: Some(digest.to_vec()),
                file_identity: Some(identity),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        db.pool().close().await;
        Self(path)
    }
    async fn db(&self) -> Arc<Database> {
        Arc::new(Database::open(self.0.join("client.sqlite")).await.unwrap())
    }
    async fn service(&self, hook: u8) -> (Arc<Database>, ManualRelease, Disk, Arc<Remote>) {
        let db = self.db().await;
        let disk = Disk::new(self.0.clone());
        let remote = Arc::new(Remote {
            disk: disk.clone(),
            db: db.clone(),
            hook,
            acquired: AtomicUsize::new(0),
        });
        let service = ManualRelease::new(db.clone(), Arc::new(disk.clone()), remote.clone());
        (db, service, disk, remote)
    }
    async fn assert_full(&self) {
        assert_eq!(fs::read(self.0.join("file.txt")).unwrap(), OLD);
        let db = self.db().await;
        let mut conn = db.pool().acquire().await.unwrap();
        assert_eq!(
            entries::local_entry(&mut conn, B, E)
                .await
                .unwrap()
                .unwrap()
                .residency,
            Residency::Materialized
        );
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct Disk {
    root: PathBuf,
    held: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    fail: Arc<AtomicU8>,
    lease_valid: Arc<AtomicBool>,
}
impl Disk {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            held: Arc::new(AtomicBool::new(false)),
            calls: Arc::new(AtomicUsize::new(0)),
            fail: Arc::new(AtomicU8::new(0)),
            lease_valid: Arc::new(AtomicBool::new(true)),
        }
    }
}
struct Bytes(Vec<u8>);
impl SnapshotSource for Bytes {
    fn identity(&self) -> &str {
        "s063-fixed-bytes"
    }
    fn total_size(&self) -> u64 {
        self.0.len() as u64
    }
    fn read(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            self.0
                .get(offset as usize..(offset + length) as usize)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| err("short snapshot"))
        })
    }
}
impl ControlledLocal for Disk {
    fn observe_content(
        &self,
        path: &RelativePath,
    ) -> PortFuture<'_, Result<ContentObservation, PortError>> {
        let path = path.clone();
        Box::pin(async move {
            if self.root.join("released.json").exists() {
                return Err(reject(
                    "PLACEHOLDER_OFFLINE",
                    "metadata-only must not hydrate",
                ));
            }
            let bytes = fs::read(self.root.join("file.txt")).map_err(err)?;
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt;
                let m = fs::metadata(self.root.join("file.txt")).map_err(err)?;
                [m.dev().to_le_bytes(), m.ino().to_le_bytes()].concat()
            };
            #[cfg(not(unix))]
            let identity = b"test-file-identity".to_vec();
            let key = format!(
                "{}:{}",
                to_hex(&digest_of(&bytes)),
                to_hex(&digest_of(&identity))
            );
            let stamp = self.root.join("stamp.json");
            let previous: Option<(String, u64)> = fs::read(&stamp)
                .ok()
                .map(|b| serde_json::from_slice(&b).unwrap());
            let generation = previous.map_or(7, |(old, n)| if old == key { n } else { n + 1 });
            persist(&stamp, &serde_json::to_vec(&(key, generation)).unwrap());
            Ok(ContentObservation::File {
                facts: LocalFacts {
                    path,
                    kind: LocalEntryKind::File,
                    generation: Generation::new(generation).unwrap(),
                    file_identity: Some(identity),
                    size: bytes.len() as u64,
                    usage: Usage::Idle,
                },
                snapshot: Arc::new(Bytes(bytes)),
            })
        })
    }
    fn replace_verified<'a>(
        &'a self,
        _: &'a ApplyRequest,
        _: &'a ContentEvidence,
    ) -> PortFuture<'a, Result<ApplyReceipt, PortError>> {
        Box::pin(async {
            Err(reject(
                "UNSUPPORTED",
                "release fixture cannot install content",
            ))
        })
    }
}
#[derive(Serialize, Deserialize)]
struct Marker {
    generation: u64,
    identity: Vec<u8>,
    version: String,
    digest: [u8; 32],
    size: u64,
}
struct Guard(Disk);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.held.store(false, Ordering::SeqCst);
    }
}
impl ReleaseLocal for Disk {
    fn protect<'a>(
        &'a self,
        _: &'a RelativePath,
    ) -> PortFuture<'a, Result<Box<dyn ReleaseProtection>, PortError>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) == 1 || self.held.swap(true, Ordering::SeqCst) {
                return Err(reject("LOCAL_IN_USE", "busy"));
            }
            Ok(Box::new(Guard(self.clone())) as Box<dyn ReleaseProtection>)
        })
    }
}
impl ReleaseProtection for Guard {
    fn content(&self) -> &dyn ControlledLocal {
        &self.0
    }
    fn released(&self) -> PortFuture<'_, Result<Option<ReleasedFile>, PortError>> {
        Box::pin(async move {
            if !self.0.root.join("released.json").exists() {
                return Ok(None);
            }
            let marker: Marker =
                serde_json::from_slice(&fs::read(self.0.root.join("released.json")).map_err(err)?)
                    .map_err(err)?;
            if !fs::read(self.0.root.join("file.txt"))
                .map_err(err)?
                .is_empty()
            {
                return Err(recovery::changed());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let m = fs::metadata(self.0.root.join("file.txt")).map_err(err)?;
                if marker.identity != [m.dev().to_le_bytes(), m.ino().to_le_bytes()].concat() {
                    return Err(recovery::changed());
                }
            }
            Ok(Some(ReleasedFile {
                generation: marker.generation,
                identity: marker.identity,
                pin: VersionPin::new(
                    B.to_string(),
                    E.to_string(),
                    marker.version,
                    marker.digest,
                    marker.size,
                )
                .map_err(err)?,
            }))
        })
    }
    fn dehydrate_verified<'a>(
        &'a self,
        expected: &'a ContentEvidence,
        lease: &'a dyn VersionLease,
    ) -> PortFuture<'a, Result<ReleasedFile, PortError>> {
        Box::pin(async move {
            assert!(self.0.held.load(Ordering::SeqCst));
            if self.0.fail.load(Ordering::SeqCst) == 2 {
                return Err(reject("INSUFFICIENT_STORAGE", "platform failure"));
            }
            if self.0.fail.load(Ordering::SeqCst) == 3 {
                self.0.lease_valid.store(false, Ordering::SeqCst);
            }
            lease.ensure_valid().await?;
            if recovery::inspect(&self.0, &spec().path).await? != *expected {
                return Err(recovery::changed());
            }
            let ContentEvidence::File {
                generation,
                identity,
                digest,
                size,
            } = expected
            else {
                return Err(recovery::changed());
            };
            let marker = Marker {
                generation: *generation,
                identity: identity.clone(),
                version: lease.pin().version.clone(),
                digest: *digest,
                size: *size,
            };
            persist(&self.0.root.join("file.txt"), b"");
            persist(
                &self.0.root.join("released.json"),
                &serde_json::to_vec(&marker).unwrap(),
            );
            self.0.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.released().await?.unwrap())
        })
    }
}
struct Remote {
    disk: Disk,
    db: Arc<Database>,
    hook: u8,
    acquired: AtomicUsize,
}
struct Lease {
    disk: Disk,
    pin: VersionPin,
}
impl Drop for Lease {
    fn drop(&mut self) {
        assert!(
            self.disk.held.load(Ordering::SeqCst),
            "lease must end while guard is held"
        );
        let _ = fs::remove_file(self.disk.root.join("lease"));
    }
}
impl VersionLease for Lease {
    fn pin(&self) -> &VersionPin {
        &self.pin
    }
    fn ensure_valid(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            if !self.disk.lease_valid.load(Ordering::SeqCst)
                || !self.disk.root.join("lease").exists()
            {
                return Err(reject("LEASE_EXPIRED", "short content lease expired"));
            }
            if fs::read(self.disk.root.join("object")).map_err(err)? != OLD {
                return Err(reject("VERSION_GONE", "immutable version unavailable"));
            }
            Ok(())
        })
    }
}
fn gc(root: &Path) -> bool {
    if root.join("lease").exists() {
        false
    } else {
        fs::remove_file(root.join("object")).is_ok()
    }
}
impl ReleaseRemote for Remote {
    fn acquire<'a>(
        &'a self,
        pin: &'a VersionPin,
    ) -> PortFuture<'a, Result<Box<dyn VersionLease>, PortError>> {
        Box::pin(async move {
            self.acquired.fetch_add(1, Ordering::SeqCst);
            if self.hook == 2 {
                return Err(reject("VERSION_GONE", "expired server version"));
            }
            if fs::read(self.disk.root.join("object")).map_err(err)? != OLD {
                return Err(reject("VERSION_GONE", "version unreadable"));
            }
            persist(&self.disk.root.join("lease"), b"short server lease");
            if self.hook == 1 {
                self.disk.lease_valid.store(false, Ordering::SeqCst);
            }
            if self.hook == 4 {
                sqlx::query("UPDATE local_entries SET dirty='dirty',generation=generation+1 WHERE binding_id=?").bind(B.as_bytes().as_slice()).execute(self.db.pool()).await.map_err(err)?;
            }
            if self.hook == 5 {
                BindingControls::new(self.db.clone())
                    .change_mode(B, SyncMode::Full)
                    .await?;
            }
            if self.hook == 6 {
                BindingControls::new(self.db.clone())
                    .set_paused(B, true)
                    .await?;
            }
            if self.hook == 7 {
                persist(&self.disk.root.join("file.txt"), EDIT);
            }
            if self.hook == 8 {
                assert!(!gc(&self.disk.root));
                assert_eq!(fs::read(self.disk.root.join("object")).unwrap(), OLD);
            }
            let mut pin = pin.clone();
            if self.hook == 3 {
                pin.version = VersionId::from_bytes([0xff; 16]).to_string();
            }
            Ok(Box::new(Lease {
                disk: self.disk.clone(),
                pin,
            }) as Box<dyn VersionLease>)
        })
    }
}

#[tokio::test]
async fn manual_release_holds_lease_across_gc_and_only_then_changes_residency() {
    let rig = Rig::new().await;
    let (db, service, disk, remote) = rig.service(8).await;
    let result = service.release(&spec()).await.unwrap();
    assert_eq!(result.pin, spec().pin);
    assert_eq!(disk.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fs::read(rig.0.join("file.txt")).unwrap(), b"");
    assert_eq!(fs::read(rig.0.join("object")).unwrap(), OLD);
    let mut conn = db.pool().acquire().await.unwrap();
    assert_eq!(
        entries::local_entry(&mut conn, B, E)
            .await
            .unwrap()
            .unwrap()
            .residency,
        Residency::MetadataOnly
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM local_intents")
            .fetch_one(&mut *conn)
            .await
            .unwrap(),
        0
    );
    assert_eq!(remote.acquired.load(Ordering::SeqCst), 1);
    assert_eq!(service.release(&spec()).await.unwrap(), result);
    assert_eq!(disk.calls.load(Ordering::SeqCst), 1);
    assert!(gc(&rig.0)); // not permanently pinning old history or claiming that a plain read prevents GC
}
#[tokio::test]
async fn full_dirty_partial_unknown_and_conflict_candidates_keep_unique_content() {
    for condition in [
        "full",
        "dirty",
        "snapshotting",
        "awaiting_commit",
        "partial",
        "metadata_only",
        "unknown",
        "conflict",
    ] {
        let rig = Rig::new().await;
        let (db, service, disk, remote) = rig.service(0).await;
        match condition {
            "full" => {
                BindingControls::new(db.clone())
                    .change_mode(B, SyncMode::Full)
                    .await
                    .unwrap();
            }
            "dirty" | "snapshotting" | "awaiting_commit" => {
                sqlx::query("UPDATE local_entries SET dirty=?")
                    .bind(condition)
                    .execute(db.pool())
                    .await
                    .unwrap();
            }
            "partial" | "metadata_only" => {
                sqlx::query("UPDATE local_entries SET residency=?")
                    .bind(condition)
                    .execute(db.pool())
                    .await
                    .unwrap();
            }
            "unknown" => {
                sqlx::query("UPDATE local_entries SET fingerprint=NULL")
                    .execute(db.pool())
                    .await
                    .unwrap();
            }
            "conflict" => {
                let mut tx = db.begin_write().await.unwrap();
                queue::enqueue(
                    &mut tx,
                    &NewPendingOp {
                        op: OpId::from_bytes([9; 16]),
                        binding: B,
                        kind: "create_conflict_copy".into(),
                        entry: Some(E),
                        baseline_revision: None,
                        baseline_version: None,
                        pinned_version: None,
                        depends_on: None,
                    },
                )
                .await
                .unwrap();
                tx.commit().await.unwrap();
            }
            _ => unreachable!(),
        }
        let error = service.release(&spec()).await.unwrap_err();
        if condition == "full" {
            assert!(error.to_string().contains("MODE_REQUIRES_LOCAL_COPY"));
        }
        assert_eq!(
            fs::read(rig.0.join("file.txt")).unwrap(),
            OLD,
            "{condition}"
        );
        assert_eq!(disk.calls.load(Ordering::SeqCst), 0);
        assert_eq!(remote.acquired.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn clean_database_cannot_authorize_unobserved_bytes_or_replaced_identity() {
    for replace in [false, true] {
        let rig = Rig::new().await;
        let (_db, service, disk, _) = rig.service(0).await;
        if replace {
            persist(&rig.0.join("other.txt"), OLD);
            fs::rename(rig.0.join("other.txt"), rig.0.join("file.txt")).unwrap();
        } else {
            persist(&rig.0.join("file.txt"), EDIT);
        }
        let bytes = fs::read(rig.0.join("file.txt")).unwrap();
        assert!(service.release(&spec()).await.is_err());
        assert_eq!(fs::read(rig.0.join("file.txt")).unwrap(), bytes);
        assert_eq!(disk.calls.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn lease_or_state_changes_after_server_check_never_release() {
    for hook in [1, 2, 3, 4, 5, 6, 7] {
        let rig = Rig::new().await;
        let (_db, service, disk, _) = rig.service(hook).await;
        assert!(service.release(&spec()).await.is_err(), "hook={hook}");
        assert_eq!(disk.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            fs::read(rig.0.join("file.txt")).unwrap(),
            if hook == 7 { EDIT } else { OLD }
        );
    }
}
#[tokio::test]
async fn busy_failed_platform_and_expired_lease_preserve_residency_and_intent() {
    for failure in [1, 2, 3] {
        let rig = Rig::new().await;
        let (db, service, disk, _) = rig.service(0).await;
        disk.fail.store(failure, Ordering::SeqCst);
        assert!(service.release(&spec()).await.is_err());
        rig.assert_full().await;
        let intents = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM local_intents")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(intents, if failure == 1 { 0 } else { 1 });
    }
}
#[tokio::test]
async fn unknown_intents_and_reused_nonrelease_operation_are_refused() {
    for unknown in [true, false] {
        let rig = Rig::new().await;
        let (db, service, disk, _) = rig.service(0).await;
        let mut tx = db.begin_write().await.unwrap();
        if unknown {
            intents::begin_intent(
                &mut tx,
                &NewLocalIntent {
                    binding: B,
                    kind: LocalIntentKind::Replace,
                    entry: Some(E),
                    target_path: "other.txt".into(),
                    source_path: None,
                    content_hash: None,
                    generation: None,
                },
            )
            .await
            .unwrap();
        } else {
            queue::enqueue(
                &mut tx,
                &NewPendingOp {
                    op: O,
                    binding: B,
                    kind: "upload_content".into(),
                    entry: Some(E),
                    baseline_revision: None,
                    baseline_version: None,
                    pinned_version: None,
                    depends_on: None,
                },
            )
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        assert!(service.release(&spec()).await.is_err());
        assert_eq!(disk.calls.load(Ordering::SeqCst), 0);
        rig.assert_full().await;
    }
}

pub(super) fn checkpoint(phase: &str, exit: i32) {
    if std::env::var("VAULTSYNCD_S063_EXIT").as_deref() == Ok(phase) {
        std::process::exit(exit);
    }
}
#[tokio::test]
async fn release_child() {
    let Ok(root) = std::env::var("VAULTSYNCD_S063_ROOT") else {
        return;
    };
    let rig = Rig(PathBuf::from(root));
    let (_db, service, _disk, _) = rig.service(0).await;
    service.release(&spec()).await.unwrap();
    panic!("checkpoint was not reached");
}
#[tokio::test]
async fn durable_release_recovers_before_and_after_platform_and_database() {
    for (phase, exit) in [("prepared", 111), ("platform", 112), ("finished", 113)] {
        let rig = Rig::new().await;
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "executor::release::tests::release_child",
                "--nocapture",
            ])
            .env("VAULTSYNCD_S063_ROOT", &rig.0)
            .env("VAULTSYNCD_S063_EXIT", phase)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(exit));
        if phase != "prepared" {
            let _ = fs::remove_file(rig.0.join("lease"));
            assert!(gc(&rig.0));
        }
        let (db, service, disk, remote) = rig.service(0).await;
        let pending = ManualRelease::pending_specs(db.clone(), B).await.unwrap();
        assert_eq!(
            pending,
            if phase == "finished" {
                vec![]
            } else {
                vec![spec()]
            }
        );
        let recovered = service.recover(B).await.unwrap();
        if phase == "finished" {
            assert!(recovered.is_empty());
            service.release(&spec()).await.unwrap();
        } else {
            assert_eq!(recovered.len(), 1);
            assert!(recovered[0].1.is_ok(), "{phase}: {:?}", recovered[0].1);
        }
        assert_eq!(
            disk.calls.load(Ordering::SeqCst),
            usize::from(phase == "prepared")
        );
        assert_eq!(
            remote.acquired.load(Ordering::SeqCst),
            usize::from(phase == "prepared")
        );
        let mut conn = db.pool().acquire().await.unwrap();
        assert_eq!(
            entries::local_entry(&mut conn, B, E)
                .await
                .unwrap()
                .unwrap()
                .residency,
            Residency::MetadataOnly
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
                .fetch_one(&mut *conn)
                .await
                .unwrap(),
            "ok"
        );
    }
}

#[tokio::test]
async fn interrupted_release_never_clears_new_dirty_or_unknown_journal_identity() {
    for mutation in ["dirty", "wrong_binding", "wrong_op"] {
        let rig = Rig::new().await;
        let (db, service, disk, _) = rig.service(0).await;
        disk.fail.store(2, Ordering::SeqCst);
        assert!(service.release(&spec()).await.is_err());
        match mutation {
            "dirty" => {
                persist(&rig.0.join("file.txt"), EDIT);
                sqlx::query(
                    "UPDATE local_entries SET dirty='awaiting_commit',generation=generation+1",
                )
                .execute(db.pool())
                .await
                .unwrap();
            }
            _ => {
                let text: String = sqlx::query_scalar("SELECT journal FROM controlled_releases")
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
                let mut json: serde_json::Value = serde_json::from_str(&text).unwrap();
                let key = if mutation == "wrong_binding" {
                    "binding"
                } else {
                    "op"
                };
                json[key] = serde_json::to_value(vec![255_u8; 16]).unwrap();
                sqlx::query("UPDATE controlled_releases SET journal=?")
                    .bind(json.to_string())
                    .execute(db.pool())
                    .await
                    .unwrap();
            }
        }
        disk.fail.store(0, Ordering::SeqCst);
        let recovered = service.recover(B).await;
        if mutation == "dirty" {
            assert!(recovered.unwrap()[0].1.is_err());
        } else {
            assert!(recovered.is_err());
        }
        assert_eq!(
            fs::read(rig.0.join("file.txt")).unwrap(),
            if mutation == "dirty" { EDIT } else { OLD }
        );
        assert_eq!(disk.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM local_intents")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            1
        );
    }
}

#[tokio::test]
async fn completed_release_replay_does_not_remove_subsequent_local_content() {
    let rig = Rig::new().await;
    let (db, service, disk, _) = rig.service(0).await;
    service.release(&spec()).await.unwrap();
    fs::remove_file(rig.0.join("released.json")).unwrap();
    persist(&rig.0.join("file.txt"), EDIT);
    sqlx::query(
        "UPDATE local_entries SET dirty='dirty',residency='materialized',generation=generation+1",
    )
    .execute(db.pool())
    .await
    .unwrap();
    assert!(service.release(&spec()).await.is_err());
    assert_eq!(fs::read(rig.0.join("file.txt")).unwrap(), EDIT);
    assert_eq!(disk.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn queued_release_must_match_the_fixed_baseline_and_version() {
    for mismatch in [false, true] {
        let rig = Rig::new().await;
        let (db, service, disk, _) = rig.service(0).await;
        let mut tx = db.begin_write().await.unwrap();
        queue::enqueue(
            &mut tx,
            &NewPendingOp {
                op: O,
                binding: B,
                kind: "release_local".into(),
                entry: Some(E),
                baseline_revision: Some(Revision::new(2).unwrap()),
                baseline_version: Some(V),
                depends_on: None,
                pinned_version: Some(if mismatch {
                    VersionId::from_bytes([0xee; 16])
                } else {
                    V
                }),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        if mismatch {
            assert!(service.release(&spec()).await.is_err());
            rig.assert_full().await;
            assert_eq!(disk.calls.load(Ordering::SeqCst), 0);
        } else {
            service.release(&spec()).await.unwrap();
            assert_eq!(disk.calls.load(Ordering::SeqCst), 1);
        }
    }
}
