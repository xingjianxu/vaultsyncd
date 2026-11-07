//! 真实 SQLite/WAL、磁盘内容与独立进程。测试端口不生成 Linux 产品，不替代 Windows 原生验收。
use super::*;
mod safety_tests;
use crate::{
    adapters::client_store::ClientStorePorts,
    bindings::import::{ImportInstall, LocalImportEntry, RootAdmission, SnapshotSeed},
    executor::apply::{ContentObservation, ControlledApply},
    ports::{
        local::{ApplyReceipt, ApplyRequest, LocalEntryKind, LocalFacts},
        state::OperationStore,
    },
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};
use vaultsyncd_client_store::{
    bindings::{NewBinding, SyncMode},
    entries::LocalEntry,
    events,
    queue::{self, NewPendingOp},
    upload_resumes::{self, NewUploadResume},
};
use vaultsyncd_domain::{
    ids::{DeviceId, RootId, ServerId, VersionId},
    state::Usage,
    version::{Generation, UtcTimestamp},
};
use vaultsyncd_fs_common::{name::NameEquivalence, path::RelativePath};
use vaultsyncd_transfer::{
    hash::digest_of,
    ports::{PortFuture, SnapshotSource},
};

fn binding() -> BindingId {
    BindingId::from_bytes([1; 16])
}
fn root_entry() -> EntryId {
    EntryId::from_bytes([2; 16])
}
fn file_entry() -> EntryId {
    EntryId::from_bytes([3; 16])
}
fn op() -> OpId {
    OpId::from_bytes([4; 16])
}
const ORIGINAL: &[u8] = b"original";
const EDITED: &[u8] = b"new-edit"; // 同长度、同 inode，旧 clean 不能授权丢弃。
pub(super) fn checkpoint(stage: &str, code: i32) {
    if std::env::var("S060_CRASH").as_deref() == Ok(stage) {
        std::process::exit(code);
    }
}
fn persist(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::File::open(path).unwrap().sync_all().unwrap();
    fs::File::open(path.parent().unwrap())
        .unwrap()
        .sync_all()
        .unwrap();
}
struct Rig(PathBuf);
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
impl Rig {
    async fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "vaultsyncd-s060-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("work")).unwrap();
        fs::create_dir_all(path.join("cache")).unwrap();
        persist(&path.join("work/file.txt"), ORIGINAL);
        let rig = Self(path);
        let database = rig.open().await;
        let actual = inspect(
            &DiskLocal(rig.0.clone()),
            &RelativePath::parse("file.txt").unwrap(),
        )
        .await
        .unwrap();
        let ContentEvidence::File {
            digest,
            identity,
            generation,
            ..
        } = actual
        else {
            panic!()
        };
        let mut tx = database.begin_write().await.unwrap();
        bindings::insert_binding(
            &mut tx,
            &NewBinding {
                id: binding(),
                server: ServerId::from_bytes([7; 16]),
                device: DeviceId::from_bytes([8; 16]),
                root: RootId::from_bytes([9; 16]),
                local_path: rig.0.join("work").display().to_string(),
                cache_path: rig.0.join("cache").display().to_string(),
                mode: SyncMode::Full,
                rules_version: "s060".to_owned(),
                server_epoch: ServerEpoch::new(7).unwrap(),
            },
        )
        .await
        .unwrap();
        for entry in [root_snapshot(), file_snapshot()] {
            let remote = remote(&entry).unwrap();
            entries::put_baseline(
                &mut tx,
                binding(),
                &SavedBase {
                    entry: entry.entry_id,
                    parent: entry.parent_id,
                    name: entry.name,
                    kind: entry.kind,
                    revision: 7,
                    version: entry.version_id,
                    deleted: false,
                }
                .baseline()
                .unwrap(),
            )
            .await
            .unwrap();
            entries::put_remote(&mut tx, binding(), &remote)
                .await
                .unwrap();
        }
        entries::put_local(
            &mut tx,
            binding(),
            &LocalEntry {
                entry: file_entry(),
                generation: Generation::new(generation).unwrap(),
                dirty: Dirty::Clean,
                residency: Residency::Materialized,
                fingerprint: Some(digest.to_vec()),
                file_identity: Some(identity),
            },
        )
        .await
        .unwrap();
        queue::enqueue(
            &mut tx,
            &NewPendingOp {
                op: op(),
                binding: binding(),
                kind: "delete".to_owned(),
                entry: Some(file_entry()),
                baseline_revision: Some(Revision::new(7).unwrap()),
                baseline_version: Some(VersionId::from_bytes([5; 16])),
                depends_on: None,
                pinned_version: None,
            },
        )
        .await
        .unwrap();
        upload_resumes::save(
            &mut tx,
            &NewUploadResume {
                op: op(),
                binding: binding(),
                snapshot_identity: "old-generation".to_owned(),
                total_size: 8,
                digest,
                session: "old-session".to_owned(),
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE bindings SET received_seq = 10, applied_seq = 9")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO received_changes(binding_id, seq, tx_id, events_json, state) VALUES (?, 10, ?, '[{\"old_delete\":true}]', 'received')")
            .bind(binding().as_bytes().as_slice()).bind(vec![6u8;16]).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        database.close().await;
        rig.server(7, 20, false);
        rig
    }
    fn server(&self, epoch: u64, high_water: u64, include_file: bool) {
        let entries = if include_file {
            vec![root_snapshot(), file_snapshot()]
        } else {
            vec![root_snapshot()]
        };
        let snapshot = Snapshot {
            snapshot_id: EntryId::from_bytes([12; 16]).to_string(),
            root_id: RootId::from_bytes([9; 16]).to_string(),
            server_epoch: epoch.to_string(),
            high_water_seq: high_water.to_string(),
            entry_count: entries.len().to_string(),
            expires_at: "9999999999".to_owned(),
        };
        persist(
            &self.0.join("snapshot.json"),
            &serde_json::to_vec(&snapshot).unwrap(),
        );
        persist(
            &self.0.join("entries.json"),
            &serde_json::to_vec(&entries).unwrap(),
        );
    }
    async fn open(&self) -> Database {
        Database::open(self.0.join("client.sqlite")).await.unwrap()
    }
    fn resync(&self, database: Arc<Database>) -> Resync {
        Resync::new(
            database,
            Arc::new(DiskLocal(self.0.clone())),
            Arc::new(DiskLocal(self.0.clone())),
            Arc::new(DiskSnapshot(self.0.clone())),
        )
    }
}
fn root_snapshot() -> SnapshotEntry {
    SnapshotEntry {
        entry_id: root_entry().to_string(),
        parent_id: None,
        name: String::new(),
        kind: WireKind::Directory,
        revision: "7".to_owned(),
        hash: None,
        size: None,
        version_id: None,
    }
}
fn file_snapshot() -> SnapshotEntry {
    SnapshotEntry {
        entry_id: file_entry().to_string(),
        parent_id: Some(root_entry().to_string()),
        name: "file.txt".to_owned(),
        kind: WireKind::File,
        revision: "7".to_owned(),
        hash: Some(hex(&digest_of(ORIGINAL))),
        size: Some("8".to_owned()),
        version_id: Some(VersionId::from_bytes([5; 16]).to_string()),
    }
}
struct Bytes(Vec<u8>);
impl SnapshotSource for Bytes {
    fn identity(&self) -> &str {
        "s060-fixed-content"
    }
    fn total_size(&self) -> u64 {
        self.0.len() as u64
    }
    fn read(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move { Ok(self.0[offset as usize..(offset + length) as usize].to_vec()) })
    }
}
struct DiskLocal(PathBuf);
impl ControlledLocal for DiskLocal {
    fn observe_content(
        &self,
        path: &RelativePath,
    ) -> PortFuture<'_, Result<ContentObservation, PortError>> {
        let path = path.clone();
        Box::pin(async move {
            if self.0.join("unknown").exists() {
                return Err(error("磁盘不可读/unknown"));
            }
            let target = self.0.join("work").join(path.to_string());
            let bytes = match fs::read(&target) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(ContentObservation::Absent);
                }
                Err(e) => return Err(error(e)),
            };
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt;
                let m = fs::metadata(&target).map_err(error)?;
                [m.dev().to_le_bytes(), m.ino().to_le_bytes()].concat()
            };
            #[cfg(not(unix))]
            let identity = b"test-file-identity".to_vec();
            Ok(ContentObservation::File {
                facts: LocalFacts {
                    path,
                    kind: LocalEntryKind::File,
                    generation: Generation::new(7).unwrap(),
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
        _: &'a ApplyRequest,
        _: &'a ContentEvidence,
    ) -> PortFuture<'a, Result<ApplyReceipt, PortError>> {
        Box::pin(async { panic!("重建不能修改本地文件") })
    }
}
impl NameEquivalence for DiskLocal {
    fn equivalent(&self, a: &str, b: &str) -> bool {
        a.eq_ignore_ascii_case(b)
    }
}
impl ImportLocal for DiskLocal {
    fn validate<'a>(
        &'a self,
        _: &'a BindingRecord,
        _: &'a [String],
    ) -> PortFuture<'a, Result<RootAdmission, PortError>> {
        Box::pin(async move {
            Ok(RootAdmission {
                identity: fs::canonicalize(self.0.join("work"))
                    .map_err(error)?
                    .display()
                    .to_string(),
                healthy: !self.0.join("unhealthy").exists(),
                directory: true,
                disjoint: true,
                representable: true,
            })
        })
    }
    fn scan(&self, _: BindingId) -> PortFuture<'_, Result<Vec<LocalImportEntry>, PortError>> {
        Box::pin(async move {
            if self.0.join("scan-failed").exists() {
                return Err(error("扫描失败不是空目录"));
            }
            let mut entries = Vec::new();
            scan(&self.0.join("work"), "", &mut entries)?;
            Ok(entries)
        })
    }
    fn preserve<'a>(
        &'a self,
        _: BindingId,
        _: OpId,
        _: &'a RelativePath,
        _: &'a RelativePath,
        _: Generation,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async { panic!("重建不另名/上传") })
    }
    fn install(&self, _: &ImportInstall) -> PortFuture<'_, Result<Generation, PortError>> {
        Box::pin(async { panic!("重建不自动安装") })
    }
}
fn scan(root: &Path, prefix: &str, entries: &mut Vec<LocalImportEntry>) -> Result<(), PortError> {
    for entry in fs::read_dir(root).map_err(error)? {
        let entry = entry.map_err(error)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| error("名称不可表示"))?;
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let metadata = entry.metadata().map_err(error)?;
        if metadata.is_dir() {
            entries.push(LocalImportEntry {
                path: path.clone(),
                kind: WireKind::Directory,
                generation: 7,
                size: None,
                hash: None,
            });
            scan(&entry.path(), &path, entries)?;
        } else {
            let bytes = fs::read(entry.path()).map_err(error)?;
            entries.push(LocalImportEntry {
                path,
                kind: WireKind::File,
                generation: 7,
                size: Some(bytes.len() as u64),
                hash: Some(digest_of(&bytes)),
            });
        }
    }
    Ok(())
}
struct DiskSnapshot(PathBuf);
impl ImportSnapshot for DiskSnapshot {
    fn begin(&self, _: RootId) -> PortFuture<'_, Result<SnapshotSeed, PortError>> {
        Box::pin(async move {
            checkpoint("fenced", 91);
            if self.0.join("offline").exists() {
                return Err(error("网络断开"));
            }
            Ok(SnapshotSeed {
                metadata: serde_json::from_slice(
                    &fs::read(self.0.join("snapshot.json")).map_err(error)?,
                )
                .map_err(error)?,
                server_time: UtcTimestamp::from_unix_seconds(100).unwrap(),
            })
        })
    }
    fn page<'a>(
        &'a self,
        _: RootId,
        snapshot: &'a str,
        cursor: Option<&'a str>,
    ) -> PortFuture<'a, Result<SnapshotPage, PortError>> {
        Box::pin(async move {
            if cursor.is_some() {
                checkpoint("page", 92);
            }
            let meta: Snapshot =
                serde_json::from_slice(&fs::read(self.0.join("snapshot.json")).map_err(error)?)
                    .map_err(error)?;
            if meta.snapshot_id != snapshot {
                return Err(error("快照已过期"));
            }
            let entries: Vec<SnapshotEntry> =
                serde_json::from_slice(&fs::read(self.0.join("entries.json")).map_err(error)?)
                    .map_err(error)?;
            let start = cursor.map(number).transpose()?.unwrap_or(0) as usize;
            let next = start + 1;
            Ok(SnapshotPage {
                snapshot_id: meta.snapshot_id,
                high_water_seq: if self.0.join("mixed-page").exists() {
                    "99".to_owned()
                } else {
                    meta.high_water_seq
                },
                entries: entries[start..next.min(entries.len())].to_vec(),
                next_cursor: (next < entries.len()).then(|| next.to_string()),
            })
        })
    }
}
async fn assert_preserved(database: &Database, rig: &Rig, expected: &[u8]) {
    let mut conn = database.pool().acquire().await.unwrap();
    assert!(queue::pending_op(&mut conn, op()).await.unwrap().is_some());
    assert!(
        upload_resumes::load(&mut conn, op())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(fs::read(rig.0.join("work/file.txt")).unwrap(), expected);
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(integrity, "ok");
}
#[tokio::test]
async fn expired_clean_missing_is_retired_never_uploaded_and_cursors_publish_atomically() {
    let rig = Rig::new().await;
    let database = Arc::new(rig.open().await);
    let report = rig
        .resync(database.clone())
        .run(binding(), ResyncReason::CursorExpired)
        .await
        .unwrap();
    assert!(report.complete);
    assert_eq!(report.high_water, Some(20));
    assert_eq!(report.items[0].disposition, Disposition::RetiredClean);
    assert_preserved(&database, &rig, ORIGINAL).await;
    let mut conn = database.pool().acquire().await.unwrap();
    assert!(
        entries::baseline_entry(&mut conn, binding(), file_entry())
            .await
            .unwrap()
            .unwrap()
            .deleted
    );
    assert!(
        entries::remote_entry(&mut conn, binding(), file_entry())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        events::cursors(&mut conn, binding()).await.unwrap(),
        events::Cursors {
            received: vaultsyncd_domain::version::RootSeq::new(20).unwrap(),
            applied: vaultsyncd_domain::version::RootSeq::new(20).unwrap()
        }
    );
    let (text, complete): (String, bool) =
        sqlx::query_as("SELECT journal, complete FROM binding_resyncs")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    let old: Journal = serde_json::from_str(&text).unwrap();
    assert!(complete);
    assert!(
        !old.old_baseline
            .iter()
            .find(|e| e.entry == file_entry().to_string())
            .unwrap()
            .deleted
    );
    assert_eq!(old.old_events[0].seq, 10);
    assert!(
        ClientStorePorts::new(database.clone())
            .pending_ops(binding())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        ClientStorePorts::new(database.clone())
            .complete(op())
            .await
            .is_err()
    );
    // 隔离依赖不能被 S055 的“缺失依赖已完成”规则误放行。
    drop(conn);
    let mut tx = database.begin_write().await.unwrap();
    queue::enqueue(
        &mut tx,
        &NewPendingOp {
            op: OpId::from_bytes([99; 16]),
            binding: binding(),
            kind: "upload".to_owned(),
            entry: None,
            baseline_revision: None,
            baseline_version: None,
            depends_on: Some(op()),
            pinned_version: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(
        ClientStorePorts::new(database.clone())
            .pending_ops(binding())
            .await
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
async fn same_size_unobserved_edit_and_uncommitted_generations_become_recovery_candidates() {
    for dirty in ["clean", "dirty", "snapshotting", "awaiting_commit"] {
        let rig = Rig::new().await;
        persist(&rig.0.join("work/file.txt"), EDITED);
        let database = Arc::new(rig.open().await);
        sqlx::query("UPDATE local_entries SET dirty = ?")
            .bind(dirty)
            .execute(database.pool())
            .await
            .unwrap();
        let report = rig
            .resync(database.clone())
            .run(binding(), ResyncReason::CursorExpired)
            .await
            .unwrap();
        assert!(report.prepared && !report.complete);
        assert_eq!(report.items[0].disposition, Disposition::RecoveryCandidate);
        assert_preserved(&database, &rig, EDITED).await;
        let mut conn = database.pool().acquire().await.unwrap();
        assert!(
            !entries::baseline_entry(&mut conn, binding(), file_entry())
                .await
                .unwrap()
                .unwrap()
                .deleted
        );
        assert_eq!(
            events::cursors(&mut conn, binding())
                .await
                .unwrap()
                .received
                .get(),
            10
        );
        assert!(
            ClientStorePorts::new(database.clone())
                .pending_ops(binding())
                .await
                .is_err()
        );
        assert!(
            ControlledApply::new(database.clone(), Arc::new(DiskLocal(rig.0.clone())))
                .recover(binding())
                .await
                .is_err()
        );
    }
}
#[tokio::test]
async fn backup_restore_is_not_log_expiry_and_same_seq_belongs_to_new_epoch() {
    let rig = Rig::new().await;
    rig.server(2, 9, true);
    let database = Arc::new(rig.open().await);
    let report = rig
        .resync(database.clone())
        .run(binding(), ResyncReason::ServerRollback { new_epoch: 2 })
        .await
        .unwrap();
    assert!(report.complete);
    assert_eq!(report.items[0].disposition, Disposition::Current);
    assert_ne!(report.reason, ResyncReason::CursorExpired);
    assert_preserved(&database, &rig, ORIGINAL).await;
    let mut conn = database.pool().acquire().await.unwrap();
    assert_eq!(
        bindings::binding(&mut conn, binding())
            .await
            .unwrap()
            .unwrap()
            .server_epoch
            .get(),
        2
    );
    assert!(
        events::received_batch(
            &mut conn,
            binding(),
            vaultsyncd_domain::version::RootSeq::new(10).unwrap()
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        entries::baseline_entry(&mut conn, binding(), file_entry())
            .await
            .unwrap()
            .is_some_and(|b| !b.deleted)
    );
    assert!(
        ClientStorePorts::new(database.clone())
            .pending_ops(binding())
            .await
            .unwrap()
            .is_empty()
    );
    // 在新 epoch 重建后，旧 seq=10 的事件不作为重复批次/旧 Delete 重放。
    drop(conn);
    let mut tx = database.begin_write().await.unwrap();
    events::receive_batch(
        &mut tx,
        &events::IncomingBatch {
            binding: binding(),
            seq: vaultsyncd_domain::version::RootSeq::new(10).unwrap(),
            tx: vaultsyncd_domain::ids::TxId::from_bytes([55; 16]),
            events_json: "[{\"new_epoch\":true}]".to_owned(),
            changes: vec![],
            tasks: vec![],
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}
#[tokio::test]
async fn rollback_lower_high_water_is_allowed_only_with_changed_epoch() {
    let rig = Rig::new().await;
    rig.server(7, 3, true);
    let database = Arc::new(rig.open().await);
    assert!(
        rig.resync(database.clone())
            .run(binding(), ResyncReason::CursorExpired)
            .await
            .is_err()
    );
    let other = Rig::new().await;
    other.server(2, 3, true);
    let db = Arc::new(other.open().await);
    assert!(
        other
            .resync(db)
            .run(binding(), ResyncReason::ServerRollback { new_epoch: 2 })
            .await
            .unwrap()
            .complete
    );
}
#[tokio::test]
async fn open_rebuild_is_visible_to_assembly_and_keeps_the_fence() {
    let rig = Rig::new().await;
    let database = Arc::new(rig.open().await);
    assert!(!rebuild_open(&database, binding()).await.unwrap());
    // 未证实的新文件让重建停在开放日志上：装配必须能看到它仍开放（从而跳过旧操作恢复），
    // 同时队列/破坏性动作仍被围栏拒绝。
    persist(&rig.0.join("work/unproven.txt"), b"review");
    let report = rig
        .resync(database.clone())
        .run(binding(), ResyncReason::CursorExpired)
        .await
        .unwrap();
    assert!(report.prepared && !report.complete);
    assert!(rebuild_open(&database, binding()).await.unwrap());
    assert!(ensure_unblocked(&database, binding()).await.is_err());
    assert_preserved(&database, &rig, ORIGINAL).await;
}
#[tokio::test]
async fn unknown_unproven_source_scan_and_network_faults_never_become_deletes_or_uploads() {
    for fault in [
        "unknown",
        "scan-failed",
        "offline",
        "mixed-page",
        "unhealthy",
        "new-file",
        "no-fingerprint",
    ] {
        let rig = Rig::new().await;
        let database = Arc::new(rig.open().await);
        match fault {
            "new-file" => persist(&rig.0.join("work/untracked.txt"), b"unproven"),
            "no-fingerprint" => {
                sqlx::query("UPDATE local_entries SET fingerprint = NULL")
                    .execute(database.pool())
                    .await
                    .unwrap();
            }
            _ => persist(&rig.0.join(fault), b"fault"),
        }
        let result = rig
            .resync(database.clone())
            .run(binding(), ResyncReason::CursorExpired)
            .await;
        if let Ok(report) = result {
            assert!(!report.complete);
            assert!(
                report
                    .items
                    .iter()
                    .any(|i| i.disposition == Disposition::Review)
            );
        }
        assert_preserved(&database, &rig, ORIGINAL).await;
        assert!(ensure_unblocked(&database, binding()).await.is_err());
    }
}
fn child(rig: &Rig, stage: &str, expected: i32) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "recovery::resync::tests::resync_child",
            "--nocapture",
        ])
        .env("S060_ROOT", &rig.0)
        .env("S060_CRASH", stage)
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
#[tokio::test]
async fn resync_child() {
    let Ok(path) = std::env::var("S060_ROOT") else {
        return;
    };
    let rig = std::mem::ManuallyDrop::new(Rig(PathBuf::from(path)));
    let database = Arc::new(rig.open().await);
    let service = rig.resync(database.clone());
    if service.resume_pending(binding()).await.unwrap().is_none() {
        service
            .run(binding(), ResyncReason::ServerRollback { new_epoch: 2 })
            .await
            .unwrap();
    }
}
#[tokio::test]
async fn independent_process_deaths_keep_old_state_until_atomic_publish_and_resume_from_sqlite() {
    for (stage, exit) in [
        ("fenced", 91),
        ("page", 92),
        ("classified", 93),
        ("before-commit", 94),
        ("committed", 95),
    ] {
        let rig = Rig::new().await;
        rig.server(2, 3, true);
        child(&rig, stage, exit);
        let database = rig.open().await;
        let mut conn = database.pool().acquire().await.unwrap();
        let (epoch, received, applied): (i64, i64, i64) =
            sqlx::query_as("SELECT server_epoch,received_seq,applied_seq FROM bindings")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(
            (epoch, received, applied),
            if stage == "committed" {
                (2, 3, 3)
            } else {
                (7, 10, 9)
            }
        );
        drop(conn);
        assert_preserved(&database, &rig, ORIGINAL).await;
        database.close().await;
        if stage != "committed" {
            child(&rig, "resume", 0);
        }
        let database = rig.open().await;
        let mut conn = database.pool().acquire().await.unwrap();
        let (epoch, received, applied): (i64, i64, i64) =
            sqlx::query_as("SELECT server_epoch,received_seq,applied_seq FROM bindings")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!((epoch, received, applied), (2, 3, 3));
        let (complete,quarantined):(i64,i64)=sqlx::query_as("SELECT (SELECT complete FROM binding_resyncs),(SELECT count(*) FROM resync_quarantined_ops)").fetch_one(&mut *conn).await.unwrap();
        assert_eq!((complete, quarantined), (1, 1));
        drop(conn);
        assert_preserved(&database, &rig, ORIGINAL).await;
        database.close().await;
    }
}
