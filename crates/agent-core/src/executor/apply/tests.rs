//! 仅测试的磁盘平台端口，不构成 Linux 客户端。真实 SQLite/WAL、暂存、fsync、rename 与独立进程退出。
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex,
    atomic::{AtomicU32, Ordering},
};

use super::*;
use crate::adapters::ClientStorePorts;
use crate::ports::local::{ApplyStage, LocalEntryKind};
use vaultsyncd_client_store::bindings::{self, NewBinding, SyncMode};
use vaultsyncd_client_store::entries::{self, EntryKind, LocalEntry};
use vaultsyncd_domain::ids::{DeviceId, EntryId, RootId, ServerId, VersionId};
use vaultsyncd_domain::state::{Dirty, Residency, Usage};
use vaultsyncd_domain::version::{Generation, Revision, ServerEpoch};
use vaultsyncd_transfer::hash::{digest_of, to_hex};
use vaultsyncd_transfer::ports::LocalErrorKind;

#[path = "confirmation_tests.rs"]
mod confirmation_tests;
#[path = "consumer_tests.rs"]
mod consumer_tests;
#[path = "restore_tests.rs"]
mod restore_tests;

const OLD: &[u8] = b"confirmed old content";
const NEW: &[u8] = b"committed remote version two";
const EDIT: &[u8] = b"unconfirmed second local edit";
fn binding() -> BindingId {
    BindingId::from_bytes([0x59; 16])
}
fn entry() -> EntryId {
    EntryId::from_bytes([0x23; 16])
}
fn op() -> OpId {
    OpId::from_bytes([0x05; 16])
}
fn io(error: impl std::fmt::Display) -> PortError {
    PortError::local(LocalErrorKind::Read, error.to_string())
}

struct Rig {
    path: PathBuf,
}
impl Rig {
    async fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "vaultsyncd-s059-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("work")).unwrap();
        fs::create_dir_all(path.join("stage")).unwrap();
        fs::create_dir_all(path.join("server")).unwrap();
        persist(&path.join("work/file.txt"), OLD);
        persist(&path.join("server/version-two"), NEW);
        let db = Database::open(path.join("client.sqlite")).await.unwrap();
        let platform = DiskLocal::new(&path);
        let actual = recovery::inspect(&platform, &RelativePath::parse("file.txt").unwrap())
            .await
            .unwrap();
        fs::write(
            path.join("expected.json"),
            serde_json::to_vec(&actual).unwrap(),
        )
        .unwrap();
        let mut tx = db.begin_write().await.unwrap();
        bindings::insert_binding(
            &mut tx,
            &NewBinding {
                id: binding(),
                server: ServerId::from_bytes([1; 16]),
                device: DeviceId::from_bytes([2; 16]),
                root: RootId::from_bytes([3; 16]),
                local_path: path.join("work").to_str().unwrap().to_owned(),
                cache_path: path.join("stage").to_str().unwrap().to_owned(),
                mode: SyncMode::Full,
                rules_version: "s059".to_owned(),
                server_epoch: ServerEpoch::new(1).unwrap(),
            },
        )
        .await
        .unwrap();
        entries::put_baseline(&mut tx, binding(), &base(1))
            .await
            .unwrap();
        let receipt = store::receipt(&actual).unwrap();
        entries::put_local(
            &mut tx,
            binding(),
            &LocalEntry {
                entry: entry(),
                generation: receipt.generation,
                dirty: Dirty::Clean,
                residency: Residency::Materialized,
                fingerprint: Some(digest_of(OLD).to_vec()),
                file_identity: receipt.file_identity,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        db.close().await;
        Self { path }
    }
    async fn db(&self) -> Arc<Database> {
        Arc::new(
            Database::open(self.path.join("client.sqlite"))
                .await
                .unwrap(),
        )
    }
    fn spec(&self) -> ApplySpec {
        spec(&self.path)
    }
    fn local(&self) -> Arc<DiskLocal> {
        Arc::new(DiskLocal::new(&self.path))
    }
    async fn assert_state(&self, bytes: &[u8], revision: u64, pending: i64) {
        assert_eq!(fs::read(self.path.join("work/file.txt")).unwrap(), bytes);
        let db = self.db().await;
        let mut conn = db.pool().acquire().await.unwrap();
        assert_eq!(
            entries::baseline_entry(&mut conn, binding(), entry())
                .await
                .unwrap()
                .unwrap()
                .revision
                .get(),
            revision
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM local_intents")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(count, pending);
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(integrity, "ok");
        drop(conn);
        db.pool().close().await;
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}
fn persist(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
    fs::File::open(path.parent().unwrap())
        .unwrap()
        .sync_all()
        .unwrap();
}
fn base(revision: u64) -> BaselineEntry {
    BaselineEntry {
        entry: entry(),
        parent: Some(EntryId::from_bytes([9; 16])),
        name: "file.txt".to_owned(),
        kind: EntryKind::File,
        revision: Revision::new(revision).unwrap(),
        content_version: Some(VersionId::from_bytes([revision as u8; 16])),
        deleted: false,
    }
}
fn spec(path: &Path) -> ApplySpec {
    ApplySpec {
        op: op(),
        binding: binding(),
        target: RelativePath::parse("file.txt").unwrap(),
        baseline_before: Some(base(1)),
        baseline_after: base(2),
        pin: VersionPin::new(
            binding().to_string(),
            entry().to_string(),
            base(2).content_version.unwrap().to_string(),
            digest_of(NEW),
            NEW.len() as u64,
        )
        .unwrap(),
        expected: serde_json::from_slice(&fs::read(path.join("expected.json")).unwrap()).unwrap(),
    }
}

struct Bytes(Vec<u8>);
impl SnapshotSource for Bytes {
    fn identity(&self) -> &str {
        "fixed-disk-snapshot"
    }
    fn total_size(&self) -> u64 {
        self.0.len() as u64
    }
    fn read(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move { Ok(self.0[offset as usize..(offset + length) as usize].to_vec()) })
    }
}

struct DiskLocal {
    root: PathBuf,
    gate: Mutex<()>,
    race: bool,
    unknown: bool,
}
impl DiskLocal {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            gate: Mutex::new(()),
            race: false,
            unknown: false,
        }
    }
}
impl ControlledLocal for DiskLocal {
    fn observe_content(
        &self,
        path: &RelativePath,
    ) -> PortFuture<'_, Result<ContentObservation, PortError>> {
        let path = path.to_string();
        Box::pin(async move {
            if self.unknown {
                return Err(io("根不可读：unknown"));
            }
            let target = self.root.join("work").join(&path);
            let bytes = match fs::read(&target) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(ContentObservation::Absent);
                }
                Err(error) => return Err(io(error)),
            };
            // 测试观察器在磁盘持久递增代际；内容摘要而不是 mtime 触发新代际。
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt;
                let m = fs::metadata(&target).map_err(io)?;
                [m.dev().to_le_bytes(), m.ino().to_le_bytes()].concat()
            };
            #[cfg(not(unix))]
            let identity = b"test-only-file-identity".to_vec();
            let key = format!(
                "{}:{}",
                to_hex(&digest_of(&bytes)),
                to_hex(&digest_of(&identity))
            );
            let stamp = self.root.join("stamp.json");
            let previous: Option<(String, u64)> = fs::read(&stamp)
                .ok()
                .map(|b| serde_json::from_slice(&b).unwrap());
            let generation = previous.map_or(7, |(old, previous_generation)| {
                if old == key {
                    previous_generation
                } else {
                    previous_generation + 1
                }
            });
            persist(&stamp, &serde_json::to_vec(&(key, generation)).unwrap());
            Ok(ContentObservation::File {
                facts: LocalFacts {
                    path: RelativePath::parse(&path).unwrap(),
                    kind: LocalEntryKind::File,
                    generation: Generation::new(generation).unwrap(),
                    size: bytes.len() as u64,
                    file_identity: Some(identity),
                    usage: Usage::Idle,
                },
                snapshot: Arc::new(Bytes(bytes)),
            })
        })
    }
    fn replace_verified<'a>(
        &'a self,
        request: &'a ApplyRequest,
        before: &'a ContentEvidence,
    ) -> PortFuture<'a, Result<ApplyReceipt, PortError>> {
        Box::pin(async move {
            // Mutex 覆盖本测试端口的所有发布，外部修改通过明确注入点模拟。
            // 生产端口必须使用 Windows 使用保护/独占句柄，而不是这种测试锁。
            if self.race {
                persist(&self.root.join("work/file.txt"), EDIT);
            }
            if std::env::var("S059_CRASH").as_deref() == Ok("intent") {
                std::process::exit(81);
            }
            let actual = recovery::inspect(self, &request.target).await?;
            if &actual != before {
                return Err(changed());
            }
            let _guard = self.gate.lock().unwrap();
            let staged = PathBuf::from(request.staged.as_ref().unwrap());
            let bytes = fs::read(&staged).map_err(io)?;
            if Some(digest_of(&bytes)) != request.expected_digest {
                return Err(io("暂存校验失败"));
            }
            let target = self.root.join("work").join(request.target.to_string());
            match before {
                ContentEvidence::Absent if target.exists() => return Err(changed()),
                ContentEvidence::File {
                    generation,
                    identity,
                    digest,
                    size,
                } => {
                    let current = fs::read(&target).map_err(io)?;
                    let (_, observed_generation): (String, u64) = serde_json::from_slice(
                        &fs::read(self.root.join("stamp.json")).map_err(io)?,
                    )
                    .map_err(io)?;
                    #[cfg(unix)]
                    let current_identity = {
                        use std::os::unix::fs::MetadataExt;
                        let m = fs::metadata(&target).map_err(io)?;
                        [m.dev().to_le_bytes(), m.ino().to_le_bytes()].concat()
                    };
                    #[cfg(not(unix))]
                    let current_identity = b"test-only-file-identity".to_vec();
                    if current.len() as u64 != *size
                        || digest_of(&current) != *digest
                        || current_identity != *identity
                        || observed_generation != *generation
                    {
                        return Err(changed());
                    }
                }
                ContentEvidence::Absent => {}
            }
            fs::rename(&staged, &target).map_err(io)?;
            fs::File::open(target.parent().unwrap())
                .map_err(io)?
                .sync_all()
                .map_err(io)?;
            drop(_guard);
            if std::env::var("S059_CRASH").as_deref() == Ok("replaced") {
                std::process::exit(82);
            }
            let actual = recovery::inspect(self, &request.target).await?;
            let mut receipt = store::receipt(&actual)?;
            receipt.stage = ApplyStage::Applied;
            Ok(receipt)
        })
    }
}

struct Remote {
    path: PathBuf,
    wrong: bool,
}
impl VersionedContent for Remote {
    fn read_range<'a>(
        &'a self,
        pin: &'a VersionPin,
        offset: u64,
        length: u64,
    ) -> PortFuture<'a, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            assert_eq!(pin.version, base(2).content_version.unwrap().to_string());
            let mut bytes = fs::read(&self.path).map_err(io)?
                [offset as usize..(offset + length) as usize]
                .to_vec();
            if self.wrong {
                bytes.fill(b'!');
            }
            Ok(bytes)
        })
    }
}
struct DiskTemp(String);
impl TempContent for DiskTemp {
    fn path(&self) -> &str {
        &self.0
    }
    fn ensure_capacity(&self, _: u64) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }
    fn write_at<'a>(
        &'a self,
        offset: u64,
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let mut f = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&self.0)
                .map_err(io)?;
            f.seek(SeekFrom::Start(offset)).map_err(io)?;
            f.write_all(bytes).map_err(io)
        })
    }
    fn read_at(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            let mut f = fs::File::open(&self.0).map_err(io)?;
            f.seek(SeekFrom::Start(offset)).map_err(io)?;
            let mut bytes = vec![0; length as usize];
            f.read_exact(&mut bytes).map_err(io)?;
            Ok(bytes)
        })
    }
    fn sync(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { fs::File::open(&self.0).map_err(io)?.sync_all().map_err(io) })
    }
    fn discard(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            if Path::new(&self.0).exists() {
                fs::remove_file(&self.0).map_err(io)?;
            }
            Ok(())
        })
    }
}
async fn apply(rig: &Rig, local: Arc<DiskLocal>, wrong: bool) -> Result<ApplyReceipt, PortError> {
    let db = rig.db().await;
    let result = ControlledApply::new(db.clone(), local)
        .apply(
            &rig.spec(),
            &Remote {
                path: rig.path.join("server/version-two"),
                wrong,
            },
            &DiskTemp(
                rig.path
                    .join("stage/remote.part")
                    .to_str()
                    .unwrap()
                    .to_owned(),
            ),
            &ClientStorePorts::new(db.clone()),
            &DownloadOptions::new(5),
        )
        .await;
    db.pool().close().await;
    result
}
fn child(rig: &Rig, mode: &str, crash: &str, expected: i32) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "executor::apply::tests::recovery_child",
            "--nocapture",
        ])
        .env("S059_CHILD_ROOT", &rig.path)
        .env("S059_CHILD_MODE", mode)
        .env("S059_CRASH", crash)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn recovery_child() {
    let Ok(path) = std::env::var("S059_CHILD_ROOT") else {
        return;
    };
    let root = PathBuf::from(path);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let db = Arc::new(Database::open(root.join("client.sqlite")).await.unwrap());
        let runner = ControlledApply::new(db.clone(), Arc::new(DiskLocal::new(&root)));
        let mode = std::env::var("S059_CHILD_MODE").unwrap();
        if mode == "apply" || mode == "confirmed" {
            let spec = spec(&root);
            let remote = Remote {
                path: root.join("server/version-two"),
                wrong: false,
            };
            let temp = DiskTemp(root.join("stage/remote.part").to_str().unwrap().to_owned());
            let progress = ClientStorePorts::new(db.clone());
            let options = DownloadOptions::new(5);
            if mode == "confirmed" {
                runner
                    .apply_after_confirmed_copy(
                        &spec,
                        &remote,
                        &temp,
                        &progress,
                        &options,
                        confirmation_tests::copy_op(),
                    )
                    .await
                    .unwrap();
            } else {
                runner
                    .apply(
                        &spec,
                        &Remote {
                            path: root.join("server/version-two"),
                            wrong: false,
                        },
                        &DiskTemp(root.join("stage/remote.part").to_str().unwrap().to_owned()),
                        &ClientStorePorts::new(db.clone()),
                        &DownloadOptions::new(5),
                    )
                    .await
                    .unwrap();
            }
            if std::env::var("S059_CRASH").as_deref() == Ok("database") {
                std::process::exit(83);
            }
        } else {
            let recovered = runner.recover(binding()).await.unwrap();
            assert_eq!(recovered.len(), 1);
            recovered.into_iter().next().unwrap().1.unwrap();
        }
        db.pool().close().await;
    });
}

#[tokio::test]
async fn sqlite_and_disk_recover_independent_processes_at_each_publish_boundary() {
    for (crash, code, bytes) in [("intent", 81, OLD), ("replaced", 82, NEW)] {
        let rig = Rig::new().await;
        child(&rig, "apply", crash, code);
        rig.assert_state(bytes, 1, 1).await;
        child(&rig, "recover", "none", 0);
        rig.assert_state(NEW, 2, 0).await;
        // 完成后同 OpId 重放只核对，不再下载或替换。
        apply(&rig, rig.local(), false).await.unwrap();
        rig.assert_state(NEW, 2, 0).await;
    }
    let rig = Rig::new().await;
    child(&rig, "apply", "database", 83);
    rig.assert_state(NEW, 2, 0).await;
    persist(&rig.path.join("work/file.txt"), EDIT);
    assert!(apply(&rig, rig.local(), false).await.is_err());
    rig.assert_state(EDIT, 2, 0).await;
}

#[tokio::test]
async fn second_edit_during_download_or_platform_guard_never_gets_overwritten() {
    let rig = Rig::new().await;
    let local = Arc::new(DiskLocal {
        race: true,
        ..DiskLocal::new(&rig.path)
    });
    assert!(apply(&rig, local, false).await.is_err());
    rig.assert_state(EDIT, 1, 1).await;
    let db = rig.db().await;
    let results = ControlledApply::new(db.clone(), rig.local())
        .recover(binding())
        .await
        .unwrap();
    assert!(results[0].1.is_err());
    db.pool().close().await;
    rig.assert_state(EDIT, 1, 1).await;
}

#[tokio::test]
async fn old_clean_record_is_not_authority_even_when_size_and_identity_are_unchanged() {
    let rig = Rig::new().await;
    persist(&rig.path.join("work/file.txt"), b"changed old content!!");
    assert_eq!(OLD.len(), b"changed old content!!".len());
    assert!(apply(&rig, rig.local(), false).await.is_err());
    rig.assert_state(b"changed old content!!", 1, 0).await;
    assert!(!rig.path.join("stage/remote.part").exists());
}

#[tokio::test]
async fn unknown_and_wrong_fixed_version_bytes_cannot_create_an_install_intent() {
    let rig = Rig::new().await;
    assert!(
        apply(
            &rig,
            Arc::new(DiskLocal {
                unknown: true,
                ..DiskLocal::new(&rig.path)
            }),
            false
        )
        .await
        .is_err()
    );
    rig.assert_state(OLD, 1, 0).await;
    assert!(apply(&rig, rig.local(), true).await.is_err());
    rig.assert_state(OLD, 1, 0).await;
}

#[tokio::test]
async fn newer_dirty_generation_after_publish_is_preserved_by_baseline_transaction() {
    let rig = Rig::new().await;
    child(&rig, "apply", "replaced", 82);
    let db = rig.db().await;
    let mut tx = db.begin_write().await.unwrap();
    entries::put_local(
        &mut tx,
        binding(),
        &LocalEntry {
            entry: entry(),
            generation: Generation::new(99).unwrap(),
            dirty: Dirty::AwaitingCommit,
            residency: Residency::Materialized,
            fingerprint: Some(digest_of(NEW).to_vec()),
            file_identity: Some(vec![99]),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(
        ControlledApply::new(db.clone(), rig.local())
            .recover(binding())
            .await
            .unwrap()[0]
            .1
            .is_err()
    );
    db.pool().close().await;
    rig.assert_state(NEW, 1, 1).await;
}

#[tokio::test]
async fn restart_with_unknown_or_third_content_retains_both_evidence_and_baseline() {
    let rig = Rig::new().await;
    child(&rig, "apply", "intent", 81);
    let db = rig.db().await;
    let unknown = Arc::new(DiskLocal {
        unknown: true,
        ..DiskLocal::new(&rig.path)
    });
    assert!(
        ControlledApply::new(db.clone(), unknown)
            .recover(binding())
            .await
            .unwrap()[0]
            .1
            .is_err()
    );
    persist(&rig.path.join("work/file.txt"), EDIT);
    assert!(
        ControlledApply::new(db.clone(), rig.local())
            .recover(binding())
            .await
            .unwrap()[0]
            .1
            .is_err()
    );
    assert_eq!(fs::read(rig.path.join("stage/remote.part")).unwrap(), NEW);
    db.pool().close().await;
    rig.assert_state(EDIT, 1, 1).await;
}
