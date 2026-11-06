//! Real SQLite/WAL and disk checks; no Windows capability claim.
use super::*;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use vaultsyncd_client_store::bindings::{self, NewBinding};
use vaultsyncd_client_store::entries::{self, BaselineEntry, EntryKind};
use vaultsyncd_client_store::queue::{self, NewPendingOp};
use vaultsyncd_domain::ids::{DeviceId, OpId, RootId, ServerId};
use vaultsyncd_domain::version::{Generation, ServerEpoch};
use vaultsyncd_transfer::hash::digest_of;

const B: BindingId = BindingId::from_bytes([1; 16]);
const ROOT: EntryId = EntryId::from_bytes([2; 16]);
const DIRTY: EntryId = EntryId::from_bytes([3; 16]);
const COLD: EntryId = EntryId::from_bytes([4; 16]);
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "vaultsyncd-s061-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn local(entry: EntryId, dirty: Dirty, residency: Residency) -> LocalEntry {
    LocalEntry {
        entry,
        generation: Generation::new(7).unwrap(),
        dirty,
        residency,
        fingerprint: Some(
            digest_of(if entry == DIRTY {
                b"unconfirmed"
            } else {
                b"remote"
            })
            .to_vec(),
        ),
        file_identity: Some(entry.as_bytes().to_vec()),
    }
}
fn base(entry: EntryId, name: &str) -> BaselineEntry {
    BaselineEntry {
        entry,
        parent: if entry == ROOT { None } else { Some(ROOT) },
        name: name.into(),
        kind: if entry == ROOT {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
        revision: Revision::new(2).unwrap(),
        content_version: if entry == ROOT {
            None
        } else {
            Some(VersionId::from_bytes(*entry.as_bytes()))
        },
        deleted: false,
    }
}
async fn setup(dir: &Dir) -> Arc<Database> {
    let db = Arc::new(Database::open(dir.0.join("state.sqlite")).await.unwrap());
    let mut tx = db.begin_write().await.unwrap();
    bindings::insert_binding(
        &mut tx,
        &NewBinding {
            id: B,
            server: ServerId::from_bytes([5; 16]),
            device: DeviceId::from_bytes([6; 16]),
            root: RootId::from_bytes([7; 16]),
            local_path: dir.0.join("files").to_str().unwrap().into(),
            cache_path: dir.0.join("cache").to_str().unwrap().into(),
            mode: SyncMode::OnDemand,
            rules_version: "initial".into(),
            server_epoch: ServerEpoch::new(1).unwrap(),
        },
    )
    .await
    .unwrap();
    for item in [
        base(ROOT, ""),
        base(DIRTY, "draft.txt"),
        base(COLD, "cold.txt"),
    ] {
        entries::put_baseline(&mut tx, B, &item).await.unwrap();
        entries::put_remote(
            &mut tx,
            B,
            &entries::RemoteEntry {
                entry: item.entry,
                parent: item.parent,
                name: item.name,
                kind: item.kind,
                revision: item.revision,
                content_version: item.content_version,
                deleted: false,
            },
        )
        .await
        .unwrap();
    }
    entries::put_local(
        &mut tx,
        B,
        &local(DIRTY, Dirty::Dirty, Residency::Materialized),
    )
    .await
    .unwrap();
    entries::put_local(
        &mut tx,
        B,
        &local(COLD, Dirty::Clean, Residency::MetadataOnly),
    )
    .await
    .unwrap();
    queue::enqueue(
        &mut tx,
        &NewPendingOp {
            op: OpId::from_bytes([8; 16]),
            binding: B,
            kind: "upload_content".into(),
            entry: Some(DIRTY),
            baseline_revision: Some(Revision::new(2).unwrap()),
            baseline_version: Some(VersionId::from_bytes([3; 16])),
            depends_on: None,
            pinned_version: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    fs::create_dir_all(dir.0.join("files")).unwrap();
    fs::write(dir.0.join("files/draft.txt"), b"unconfirmed").unwrap();
    db
}
struct Disk {
    db: Arc<Database>,
    root: PathBuf,
    calls: AtomicUsize,
    cancel_at: usize,
    mutate: Option<&'static str>,
}
impl Disk {
    fn new(db: Arc<Database>, dir: &Dir) -> Self {
        Self {
            db,
            root: dir.0.join("files"),
            calls: AtomicUsize::new(0),
            cancel_at: usize::MAX,
            mutate: None,
        }
    }
}
impl FullMaterializer for Disk {
    fn ensure_full<'a>(
        &'a self,
        file: &'a FullFile,
    ) -> PortFuture<'a, Result<LocalEntry, PortError>> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::Relaxed) == self.cancel_at {
                return Err(PortError::Cancelled);
            }
            let path = self.root.join(file.path.to_string());
            let mut conn = self.db.pool().acquire().await.map_err(db)?;
            let mut observed = entries::local_entry(&mut conn, B, file.entry)
                .await
                .map_err(db)?
                .unwrap();
            drop(conn);
            if observed.residency != Residency::Materialized {
                assert_eq!(observed.dirty, Dirty::Clean, "测试适配器也不覆盖dirty");
                fs::write(&path, b"remote").map_err(db)?;
                observed.residency = Residency::Materialized;
                let mut tx = self.db.begin_write().await.map_err(db)?;
                entries::put_local(&mut tx, B, &observed)
                    .await
                    .map_err(db)?;
                tx.commit().await.map_err(db)?;
            }
            let bytes = fs::read(path).map_err(db)?;
            if Some(digest_of(&bytes).to_vec()) != observed.fingerprint {
                return Err(rejected("LOCAL_CHANGED", "真实字节不一致"));
            }
            if let Some(mutate) = self.mutate {
                let mut tx = self.db.begin_write().await.map_err(db)?;
                match mutate {
                    "generation" => {
                        sqlx::query("UPDATE local_entries SET generation=generation+1,dirty='awaiting_commit' WHERE binding_id=? AND entry_id=?").bind(B.as_bytes().as_slice()).bind(file.entry.as_bytes().as_slice()).execute(&mut *tx).await.map_err(db)?;
                    }
                    "remote" => {
                        sqlx::query("UPDATE remote_entries SET revision=revision+1 WHERE binding_id=? AND entry_id=?").bind(B.as_bytes().as_slice()).bind(file.entry.as_bytes().as_slice()).execute(&mut *tx).await.map_err(db)?;
                    }
                    "pause" => {
                        sqlx::query("UPDATE binding_controls SET paused=1,revision=revision+1 WHERE binding_id=?").bind(B.as_bytes().as_slice()).execute(&mut *tx).await.map_err(db)?;
                    }
                    _ => unreachable!(),
                }
                tx.commit().await.map_err(db)?;
            }
            Ok(observed)
        })
    }
}
async fn preserved(db: &Database, dir: &Dir) {
    assert_eq!(
        fs::read(dir.0.join("files/draft.txt")).unwrap(),
        b"unconfirmed"
    );
    let dirty: String = sqlx::query_scalar("SELECT dirty FROM local_entries WHERE entry_id=?")
        .bind(DIRTY.as_bytes().as_slice())
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_ne!(dirty, "clean");
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM pending_ops")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(pending, 1);
    let deletes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pending_ops WHERE kind='delete_entry'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(deletes, 0);
    let baseline: i64 = sqlx::query_scalar("SELECT count(*) FROM baseline_entries")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(baseline, 3);
}
#[tokio::test]
async fn full_requires_all_bytes_and_preserves_dirty_across_budgeted_restart() {
    let dir = Dir::new();
    let db = setup(&dir).await;
    let controls = BindingControls::new(db.clone());
    controls.change_mode(B, SyncMode::Full).await.unwrap();
    assert!(!controls.mode_progress(B).await.unwrap().complete);
    let first = controls
        .resume_mode(B, &Disk::new(db.clone(), &dir), 1)
        .await
        .unwrap();
    assert_eq!(
        (first.verified, first.remaining, first.complete),
        (1, 1, false)
    );
    preserved(&db, &dir).await;
    drop(controls);
    drop(db);
    let reopened = Arc::new(Database::open(dir.0.join("state.sqlite")).await.unwrap());
    let controls = BindingControls::new(reopened.clone());
    controls.change_mode(B, SyncMode::Full).await.unwrap(); // idempotent continuation
    let disk = Disk::new(reopened.clone(), &dir);
    let done = controls.resume_mode(B, &disk, 10).await.unwrap();
    assert!(done.complete);
    assert_eq!(disk.calls.load(Ordering::Relaxed), 1);
    assert_eq!(fs::read(dir.0.join("files/cold.txt")).unwrap(), b"remote");
    controls.change_mode(B, SyncMode::OnDemand).await.unwrap();
    assert!(controls.mode_progress(B).await.unwrap().complete);
    assert_eq!(fs::read(dir.0.join("files/cold.txt")).unwrap(), b"remote");
    preserved(&reopened, &dir).await;
}
#[tokio::test]
async fn failure_pause_and_unbind_keep_queue_bytes_and_recovery_choices() {
    let dir = Dir::new();
    let db = setup(&dir).await;
    let controls = BindingControls::new(db.clone());
    controls.change_mode(B, SyncMode::Full).await.unwrap();
    let mut disk = Disk::new(db.clone(), &dir);
    disk.cancel_at = 1;
    assert_eq!(
        controls.resume_mode(B, &disk, 10).await.unwrap_err(),
        PortError::Cancelled
    );
    assert_eq!(controls.mode_progress(B).await.unwrap().verified, 1);
    controls.set_paused(B, true).await.unwrap();
    controls.set_paused(B, true).await.unwrap();
    assert_eq!(
        controls.execution_gate(B, Some(DIRTY)).await.unwrap(),
        ExecutionGate::Paused
    );
    assert!(controls.resume_mode(B, &disk, 10).await.is_err());
    controls.set_paused(B, false).await.unwrap();
    assert!(
        controls
            .resume_mode(B, &Disk::new(db.clone(), &dir), 10)
            .await
            .unwrap()
            .complete
    );
    let report = controls.unbind_keep_data(B).await.unwrap();
    assert_eq!(report.unconfirmed, vec![DIRTY]);
    assert!(report.offer_export_recovery);
    assert_eq!(controls.unbind_keep_data(B).await.unwrap(), report);
    assert_eq!(
        controls.execution_gate(B, None).await.unwrap(),
        ExecutionGate::Unbound
    );
    assert!(controls.set_paused(B, false).await.is_err());
    assert!(!controls.mode_progress(B).await.unwrap().complete);
    preserved(&db, &dir).await;
}
#[tokio::test]
async fn unbind_warns_about_partial_and_unconfirmed_without_cleanup() {
    let dir = Dir::new();
    let db = setup(&dir).await;
    let controls = BindingControls::new(db.clone());
    let preview = controls.preview_unbind(B).await.unwrap();
    assert_eq!(preview.incomplete, vec![COLD]);
    assert_eq!(controls.unbind_keep_data(B).await.unwrap(), preview);
    preserved(&db, &dir).await;
    assert!(!dir.0.join("files/cold.txt").exists());
}
#[tokio::test]
async fn exclusions_keep_content_and_reinclude_requires_latest_reconciliation() {
    let dir = Dir::new();
    let db = setup(&dir).await;
    let controls = BindingControls::new(db.clone());
    controls.change_rules(B, "rules-1", "*.txt").await.unwrap();
    assert_eq!(
        controls.execution_gate(B, Some(DIRTY)).await.unwrap(),
        ExecutionGate::Excluded
    );
    assert_eq!(
        controls.execution_gate(B, None).await.unwrap(),
        ExecutionGate::ReconcileRequired
    );
    controls.change_mode(B, SyncMode::Full).await.unwrap();
    assert!(
        controls
            .resume_mode(B, &Disk::new(db.clone(), &dir), 10)
            .await
            .unwrap()
            .complete
    );
    assert!(!dir.0.join("files/cold.txt").exists());
    controls.change_rules(B, "rules-2", "").await.unwrap();
    assert_eq!(
        controls.execution_gate(B, Some(DIRTY)).await.unwrap(),
        ExecutionGate::ReconcileRequired
    );
    assert!(
        controls
            .acknowledge_reconciled(B, DIRTY, "rules-1")
            .await
            .is_err()
    );
    assert!(
        !controls
            .resume_mode(B, &Disk::new(db.clone(), &dir), 10)
            .await
            .unwrap()
            .complete
    );
    controls
        .acknowledge_reconciled(B, DIRTY, "rules-2")
        .await
        .unwrap();
    controls
        .acknowledge_reconciled(B, COLD, "rules-2")
        .await
        .unwrap();
    assert_eq!(
        controls.execution_gate(B, Some(DIRTY)).await.unwrap(),
        ExecutionGate::Ready
    );
    assert!(
        controls
            .resume_mode(B, &Disk::new(db.clone(), &dir), 10)
            .await
            .unwrap()
            .complete
    );
    preserved(&db, &dir).await;
}
#[tokio::test]
async fn generation_remote_version_and_pause_races_never_certify_complete() {
    for mutation in ["generation", "remote", "pause"] {
        let dir = Dir::new();
        let db = setup(&dir).await;
        let controls = BindingControls::new(db.clone());
        controls.change_mode(B, SyncMode::Full).await.unwrap();
        let mut disk = Disk::new(db.clone(), &dir);
        disk.mutate = Some(mutation);
        assert!(
            controls.resume_mode(B, &disk, 10).await.is_err(),
            "{mutation}"
        );
        assert!(!controls.mode_progress(B).await.unwrap().complete);
        preserved(&db, &dir).await;
    }
}
#[tokio::test]
async fn already_completed_full_is_invalidated_by_new_remote_or_local_facts() {
    let dir = Dir::new();
    let db = setup(&dir).await;
    let controls = BindingControls::new(db.clone());
    controls.change_mode(B, SyncMode::Full).await.unwrap();
    assert!(
        controls
            .resume_mode(B, &Disk::new(db.clone(), &dir), 10)
            .await
            .unwrap()
            .complete
    );
    sqlx::query("UPDATE local_entries SET generation=generation+1 WHERE entry_id=?")
        .bind(DIRTY.as_bytes().as_slice())
        .execute(db.pool())
        .await
        .unwrap();
    assert!(!controls.mode_progress(B).await.unwrap().complete);
    assert!(
        controls
            .resume_mode(B, &Disk::new(db.clone(), &dir), 10)
            .await
            .unwrap()
            .complete
    );
    sqlx::query("UPDATE remote_entries SET revision=revision+1 WHERE entry_id=?")
        .bind(COLD.as_bytes().as_slice())
        .execute(db.pool())
        .await
        .unwrap();
    assert!(!controls.mode_progress(B).await.unwrap().complete);
    assert!(
        controls
            .resume_mode(B, &Disk::new(db.clone(), &dir), 10)
            .await
            .is_err(),
        "旧clean基线不能确认新远端"
    );
}
#[tokio::test]
async fn unsafe_missing_dirty_content_and_invalid_rules_do_not_change_state() {
    let dir = Dir::new();
    let db = setup(&dir).await;
    let controls = BindingControls::new(db.clone());
    assert!(controls.change_rules(B, "bad", "[z-a]").await.is_err());
    assert_eq!(
        controls.execution_gate(B, Some(DIRTY)).await.unwrap(),
        ExecutionGate::Ready
    );
    sqlx::query("UPDATE local_entries SET residency='partial' WHERE entry_id=?")
        .bind(DIRTY.as_bytes().as_slice())
        .execute(db.pool())
        .await
        .unwrap();
    controls.change_mode(B, SyncMode::Full).await.unwrap();
    let disk = Disk::new(db.clone(), &dir);
    assert!(controls.resume_mode(B, &disk, 10).await.is_err());
    assert_eq!(disk.calls.load(Ordering::Relaxed), 0);
    preserved(&db, &dir).await;
}

#[tokio::test]
async fn excluded_descendants_block_parent_delete_and_rule_versions_cannot_be_reused() {
    let dir = Dir::new();
    let db = setup(&dir).await;
    let controls = BindingControls::new(db.clone());
    controls.change_rules(B, "v1", "draft.txt").await.unwrap();
    assert_eq!(
        controls.execution_gate(B, Some(ROOT)).await.unwrap(),
        ExecutionGate::ReconcileRequired
    );
    // Caller must not start a recursive DeleteEntry for this parent; gate has no delete side effect.
    assert_eq!(
        controls.execution_gate(B, Some(COLD)).await.unwrap(),
        ExecutionGate::Ready
    );
    controls.change_rules(B, "v2", "").await.unwrap();
    assert_eq!(
        controls.execution_gate(B, Some(ROOT)).await.unwrap(),
        ExecutionGate::ReconcileRequired
    );
    assert!(controls.change_rules(B, "v1", "").await.is_err());
    assert!(controls.change_rules(B, "initial", "").await.is_err());
    assert!(
        controls
            .acknowledge_reconciled(B, DIRTY, "v1")
            .await
            .is_err()
    );
    controls
        .acknowledge_reconciled(B, DIRTY, "v2")
        .await
        .unwrap();
    assert_eq!(
        controls.execution_gate(B, Some(ROOT)).await.unwrap(),
        ExecutionGate::Ready
    );
    preserved(&db, &dir).await;
}

#[tokio::test]
async fn mode_process_boundary_child() {
    let Ok(path) = std::env::var("VAULTSYNCD_S061_CHILD_DIR") else {
        return;
    };
    let stage = std::env::var("VAULTSYNCD_S061_CHILD_STAGE").unwrap();
    let dir = Dir(PathBuf::from(path));
    let db = Arc::new(Database::open(dir.0.join("state.sqlite")).await.unwrap());
    let controls = BindingControls::new(db.clone());
    controls.change_mode(B, SyncMode::Full).await.unwrap();
    if stage == "101" {
        std::process::exit(101);
    }
    let budget = if stage == "102" { 1 } else { 10 };
    controls
        .resume_mode(B, &Disk::new(db.clone(), &dir), budget)
        .await
        .unwrap();
    std::process::exit(if stage == "102" { 102 } else { 103 });
}
#[tokio::test]
async fn independent_process_exit_recovers_mode_intent_file_progress_and_completion() {
    for stage in [101, 102, 103] {
        let dir = Dir::new();
        let db = setup(&dir).await;
        drop(db);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("bindings::control::tests::mode_process_boundary_child")
            .arg("--nocapture")
            .env("VAULTSYNCD_S061_CHILD_DIR", &dir.0)
            .env("VAULTSYNCD_S061_CHILD_STAGE", stage.to_string())
            .output()
            .unwrap();
        assert_eq!(
            status.status.code(),
            Some(stage),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
        let db = Arc::new(Database::open(dir.0.join("state.sqlite")).await.unwrap());
        let controls = BindingControls::new(db.clone());
        let before = controls.mode_progress(B).await.unwrap();
        assert_eq!(
            before.verified,
            match stage {
                101 => 0,
                102 => 1,
                _ => 2,
            }
        );
        let disk = Disk::new(db.clone(), &dir);
        assert!(controls.resume_mode(B, &disk, 10).await.unwrap().complete);
        assert_eq!(disk.calls.load(Ordering::Relaxed), 2 - before.verified);
        preserved(&db, &dir).await;
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(integrity, "ok");
    }
}

#[tokio::test]
async fn dirty_complete_bytes_cannot_substitute_for_unapplied_new_remote_version() {
    let dir = Dir::new();
    let db = setup(&dir).await;
    let controls = BindingControls::new(db.clone());
    sqlx::query("UPDATE remote_entries SET revision=revision+1 WHERE entry_id=?")
        .bind(DIRTY.as_bytes().as_slice())
        .execute(db.pool())
        .await
        .unwrap();
    controls.change_mode(B, SyncMode::Full).await.unwrap();
    assert!(
        controls
            .resume_mode(B, &Disk::new(db.clone(), &dir), 10)
            .await
            .is_err()
    );
    assert!(!controls.mode_progress(B).await.unwrap().complete);
    preserved(&db, &dir).await;
}
