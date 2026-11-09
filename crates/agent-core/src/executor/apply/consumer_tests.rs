use super::*;
use crate::bindings::import::ImportInstall;
use crate::engine::events::{
    ApplyResult, ChangeSource, DecodeError, EventApplier, EventDecoder, EventEnvelope,
    EventReceiver,
};
use vaultsyncd_client_store::entries::RemoteEntry;
use vaultsyncd_client_store::{events, queue};
use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::TxId;
use vaultsyncd_domain::version::VaultSeq;
use vaultsyncd_protocol::generated::{ChangeList, SnapshotEntry};

async fn import_spec(rig: &Rig) -> ApplySpec {
    fs::remove_file(rig.path.join("work/file.txt")).unwrap();
    let db = rig.db().await;
    let mut tx = db.begin_write().await.unwrap();
    entries::remove_baseline(&mut tx, binding(), entry())
        .await
        .unwrap();
    entries::remove_local(&mut tx, binding(), entry())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    db.pool().close().await;
    let request = ImportInstall {
        op: op(),
        binding: binding(),
        path: RelativePath::parse("file.txt").unwrap(),
        mode: SyncMode::Full,
        remote: SnapshotEntry {
            modified_at: None,
            entry_id: entry().to_string(),
            parent_id: base(2).parent.map(|p| p.to_string()),
            name: "file.txt".to_owned(),
            kind: vaultsyncd_protocol::generated::EntryKind::File,
            revision: "2".to_owned(),
            version_id: base(2).content_version.map(|v| v.to_string()),
            hash: Some(to_hex(&digest_of(NEW))),
            size: Some(NEW.len().to_string()),
        },
    };
    let result = ApplySpec::for_import_file(&request).unwrap();
    let mut unsupported = request.clone();
    unsupported.mode = SyncMode::OnDemand;
    assert!(ApplySpec::for_import_file(&unsupported).is_err());
    result
}
async fn install(
    rig: &Rig,
    spec: &ApplySpec,
    local: Arc<DiskLocal>,
) -> Result<ApplyReceipt, PortError> {
    let db = rig.db().await;
    let result = ControlledApply::new(db.clone(), local)
        .apply(
            spec,
            &Remote {
                path: rig.path.join("server/version-two"),
                wrong: false,
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

#[tokio::test]
async fn first_full_install_uses_same_journal_and_never_overwrites_a_second_create() {
    let rig = Rig::new().await;
    let spec = import_spec(&rig).await;
    install(&rig, &spec, rig.local()).await.unwrap();
    rig.assert_state(NEW, 2, 0).await;
    install(&rig, &spec, rig.local()).await.unwrap();
    persist(&rig.path.join("work/file.txt"), EDIT);
    assert!(install(&rig, &spec, rig.local()).await.is_err());
    rig.assert_state(EDIT, 2, 0).await;

    let rig = Rig::new().await;
    let spec = import_spec(&rig).await;
    let local = Arc::new(DiskLocal {
        race: true,
        ..DiskLocal::new(&rig.path)
    });
    assert!(install(&rig, &spec, local).await.is_err());
    assert_eq!(fs::read(rig.path.join("work/file.txt")).unwrap(), EDIT);
    let db = rig.db().await;
    let mut conn = db.pool().acquire().await.unwrap();
    assert!(
        entries::baseline_entry(&mut conn, binding(), entry())
            .await
            .unwrap()
            .is_none()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM local_intents")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(count, 1);
    drop(conn);
    db.pool().close().await;
}

struct NoPull;
impl ChangeSource for NoPull {
    fn pull(
        &self,
        _: VaultId,
        _: VaultSeq,
        _: u32,
    ) -> PortFuture<'_, Result<ChangeList, PortError>> {
        Box::pin(async { Err(io("只恢复已持久化事件，不请求最新 head")) })
    }
}
struct Decoder;
fn remote() -> RemoteEntry {
    let b = base(2);
    RemoteEntry {
        entry: b.entry,
        parent: b.parent,
        name: b.name,
        kind: b.kind,
        revision: b.revision,
        content_version: b.content_version,
        deleted: b.deleted,
    }
}
fn task() -> queue::NewPendingOp {
    queue::NewPendingOp {
        op: op(),
        binding: binding(),
        kind: "apply_remote".to_owned(),
        entry: Some(entry()),
        baseline_revision: Some(base(1).revision),
        baseline_version: base(1).content_version,
        depends_on: None,
        pinned_version: base(2).content_version,
    }
}
impl EventDecoder for Decoder {
    fn decode(
        &self,
        binding: BindingId,
        envelope: &EventEnvelope,
    ) -> Result<events::IncomingBatch, DecodeError> {
        Ok(events::IncomingBatch {
            binding,
            seq: envelope.seq,
            tx: envelope.tx,
            events_json: envelope.events_json.clone(),
            changes: vec![remote()],
            tasks: vec![task()],
        })
    }
}
struct Applier {
    path: PathBuf,
    db: Arc<Database>,
    obsolete: bool,
}
impl EventApplier for Applier {
    fn apply(
        &self,
        _: BindingId,
        _: &[RemoteEntry],
        tasks: &[queue::NewPendingOp],
    ) -> PortFuture<'_, Result<ApplyResult, PortError>> {
        let tasks = tasks.to_vec();
        Box::pin(async move {
            if !self.obsolete {
                let runner =
                    ControlledApply::new(self.db.clone(), Arc::new(DiskLocal::new(&self.path)));
                if runner
                    .apply(
                        &spec(&self.path),
                        &Remote {
                            path: self.path.join("server/version-two"),
                            wrong: false,
                        },
                        &DiskTemp(
                            self.path
                                .join("stage/remote.part")
                                .to_str()
                                .unwrap()
                                .to_owned(),
                        ),
                        &ClientStorePorts::new(self.db.clone()),
                        &DownloadOptions::new(5),
                    )
                    .await
                    .is_err()
                {
                    return Ok(ApplyResult::Blocked(ErrorCode::LocalChanged));
                }
            }
            let mut tx = self.db.begin_write().await.map_err(io)?;
            for task in tasks {
                queue::complete(&mut tx, task.op).await.map_err(io)?;
            }
            tx.commit().await.map_err(io)?;
            Ok(ApplyResult::Applied)
        })
    }
}
async fn receive(db: &Database) {
    let mut tx = db.begin_write().await.unwrap();
    events::receive_batch(
        &mut tx,
        &events::IncomingBatch {
            binding: binding(),
            seq: VaultSeq::new(1).unwrap(),
            tx: TxId::from_bytes([1; 16]),
            events_json: "[{\"test\":\"fixed-version\"}]".to_owned(),
            changes: vec![remote()],
            tasks: vec![task()],
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn event_replay_after_publish_only_advances_applied_cursor_after_baseline_and_preserves_blocked_task()
 {
    for edited in [false, true] {
        let rig = Rig::new().await;
        let db = rig.db().await;
        receive(&db).await;
        db.pool().close().await;
        child(&rig, "apply", "replaced", 82);
        if edited {
            persist(&rig.path.join("work/file.txt"), EDIT);
        }
        let db = rig.db().await;
        let receiver = EventReceiver::new(
            db.clone(),
            VaultId::from_bytes([3; 16]),
            binding(),
            Arc::new(NoPull),
            Arc::new(Decoder),
            Arc::new(Applier {
                path: rig.path.clone(),
                db: db.clone(),
                obsolete: false,
            }),
            10,
        )
        .unwrap();
        let report = receiver.resume_pending().await.unwrap();
        assert_eq!(report.applied, usize::from(!edited));
        assert_eq!(report.blocked, usize::from(edited));
        let mut conn = db.pool().acquire().await.unwrap();
        let cursors = events::cursors(&mut conn, binding()).await.unwrap();
        assert_eq!(cursors.received.get(), 1);
        assert_eq!(cursors.applied.get(), u64::from(!edited));
        assert_eq!(
            queue::pending_ops_for_binding(&mut conn, binding())
                .await
                .unwrap()
                .len(),
            usize::from(edited)
        );
        drop(conn);
        drop(receiver);
        db.pool().close().await;
        rig.assert_state(
            if edited { EDIT } else { NEW },
            if edited { 1 } else { 2 },
            i64::from(edited),
        )
        .await;
    }
}

#[tokio::test]
async fn obsolete_event_cannot_regress_a_newer_confirmed_baseline() {
    let rig = Rig::new().await;
    let db = rig.db().await;
    receive(&db).await;
    let mut tx = db.begin_write().await.unwrap();
    entries::put_baseline(&mut tx, binding(), &base(3))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let receiver = EventReceiver::new(
        db.clone(),
        VaultId::from_bytes([3; 16]),
        binding(),
        Arc::new(NoPull),
        Arc::new(Decoder),
        Arc::new(Applier {
            path: rig.path.clone(),
            db: db.clone(),
            obsolete: true,
        }),
        10,
    )
    .unwrap();
    assert_eq!(receiver.resume_pending().await.unwrap().applied, 1);
    drop(receiver);
    db.pool().close().await;
    rig.assert_state(OLD, 3, 0).await;
}

#[tokio::test]
async fn unowned_legacy_intent_makes_startup_recovery_explicitly_unknown() {
    let rig = Rig::new().await;
    let db = rig.db().await;
    let mut tx = db.begin_write().await.unwrap();
    vaultsyncd_client_store::intents::begin_intent(
        &mut tx,
        &vaultsyncd_client_store::intents::NewLocalIntent {
            binding: binding(),
            kind: vaultsyncd_client_store::intents::LocalIntentKind::Release,
            entry: Some(entry()),
            target_path: "file.txt".to_owned(),
            source_path: None,
            content_hash: None,
            generation: Some(Generation::new(7).unwrap()),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(
        ControlledApply::new(db.clone(), rig.local())
            .recover(binding())
            .await
            .is_err()
    );
    db.pool().close().await;
    rig.assert_state(OLD, 1, 1).await;
}

#[tokio::test]
async fn staged_tampering_and_op_reuse_are_rejected_without_erasing_evidence() {
    let rig = Rig::new().await;
    child(&rig, "apply", "intent", 81);
    persist(
        &rig.path.join("stage/remote.part"),
        b"tampered after streaming verification",
    );
    let db = rig.db().await;
    let runner = ControlledApply::new(db.clone(), rig.local());
    assert!(runner.recover(binding()).await.unwrap()[0].1.is_err());
    let mut reused = rig.spec();
    reused.pin.digest = [0x59; 32];
    assert!(
        runner
            .apply(
                &reused,
                &Remote {
                    path: rig.path.join("server/version-two"),
                    wrong: false
                },
                &DiskTemp(
                    rig.path
                        .join("stage/remote.part")
                        .to_str()
                        .unwrap()
                        .to_owned()
                ),
                &ClientStorePorts::new(db.clone()),
                &DownloadOptions::new(5)
            )
            .await
            .is_err()
    );
    drop(runner);
    db.pool().close().await;
    rig.assert_state(OLD, 1, 1).await;
}
