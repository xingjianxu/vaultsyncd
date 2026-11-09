use super::*;
use crate::engine::events::{
    ApplyResult, ChangeSource, DecodeError, EventApplier, EventDecoder, EventEnvelope,
    EventReceiver,
};
use vaultsyncd_client_store::events::IncomingBatch;
use vaultsyncd_domain::version::VaultSeq;
use vaultsyncd_protocol::generated::ChangeList;

struct NeverEvents;
impl ChangeSource for NeverEvents {
    fn pull(
        &self,
        _: VaultId,
        _: VaultSeq,
        _: u32,
    ) -> PortFuture<'_, Result<ChangeList, PortError>> {
        Box::pin(async { panic!("重建未完成不得拉取旧游标事件") })
    }
}
impl EventDecoder for NeverEvents {
    fn decode(&self, _: BindingId, _: &EventEnvelope) -> Result<IncomingBatch, DecodeError> {
        panic!("旧 epoch 事件不得解码重放")
    }
}
impl EventApplier for NeverEvents {
    fn apply(
        &self,
        _: BindingId,
        _: &[RemoteEntry],
        _: &[NewPendingOp],
    ) -> PortFuture<'_, Result<ApplyResult, PortError>> {
        Box::pin(async { panic!("旧 Delete 不得进入本地应用") })
    }
}

#[tokio::test]
async fn identical_missing_clean_content_has_distinct_expiry_and_backup_restore_results() {
    let expired = Rig::new().await;
    let db = Arc::new(expired.open().await);
    let retired = expired
        .resync(db)
        .run(binding(), ResyncReason::CursorExpired)
        .await
        .unwrap();
    assert!(retired.complete);
    assert_eq!(retired.items[0].disposition, Disposition::RetiredClean);
    let restored = Rig::new().await;
    restored.server(2, 3, false);
    let db = Arc::new(restored.open().await);
    let candidate = restored
        .resync(db.clone())
        .run(binding(), ResyncReason::ServerRollback { new_epoch: 2 })
        .await
        .unwrap();
    assert!(candidate.prepared && !candidate.complete);
    assert_eq!(
        candidate.items[0].disposition,
        Disposition::RecoveryCandidate
    );
    assert_preserved(&db, &restored, ORIGINAL).await;
    // 整份候选日志跨进程续接；没有原 reason/内存快照也不会把它重新首次导入。
    drop(db);
    child(&restored, "resume", 0);
    let db = Arc::new(restored.open().await);
    assert_eq!(
        restored
            .resync(db.clone())
            .latest_report(binding())
            .await
            .unwrap()
            .unwrap(),
        candidate
    );
    let receiver = EventReceiver::new(
        db.clone(),
        VaultId::from_bytes([9; 16]),
        binding(),
        Arc::new(NeverEvents),
        Arc::new(NeverEvents),
        Arc::new(NeverEvents),
        100,
    )
    .unwrap();
    assert!(receiver.resume_pending().await.is_err());
    assert!(receiver.run_cycle(None).await.is_err());
    let mut conn = db.pool().acquire().await.unwrap();
    assert_eq!(
        events::cursors(&mut conn, binding())
            .await
            .unwrap()
            .received
            .get(),
        10
    );
    assert!(
        events::received_batch(&mut conn, binding(), VaultSeq::new(10).unwrap())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn metadata_only_and_legacy_intents_are_not_assumed_safe() {
    for kind in ["placeholder", "intent"] {
        let rig = Rig::new().await;
        rig.server(2, 3, true);
        let db = Arc::new(rig.open().await);
        if kind == "placeholder" {
            sqlx::query("UPDATE local_entries SET residency = 'metadata_only'")
                .execute(db.pool())
                .await
                .unwrap();
        } else {
            let mut tx = db.begin_write().await.unwrap();
            vaultsyncd_client_store::intents::begin_intent(
                &mut tx,
                &vaultsyncd_client_store::intents::NewLocalIntent {
                    binding: binding(),
                    kind: vaultsyncd_client_store::intents::LocalIntentKind::Replace,
                    entry: Some(file_entry()),
                    target_path: "file.txt".to_owned(),
                    source_path: Some("staged.part".to_owned()),
                    content_hash: Some(digest_of(ORIGINAL).to_vec()),
                    generation: None,
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }
        let report = rig
            .resync(db.clone())
            .run(binding(), ResyncReason::ServerRollback { new_epoch: 2 })
            .await
            .unwrap();
        assert!(!report.complete);
        assert!(ensure_unblocked(&db, binding()).await.is_err());
        assert_preserved(&db, &rig, ORIGINAL).await;
    }
}

#[tokio::test]
async fn absence_is_verified_without_replaying_delete_and_publish_cas_protects_new_dirty() {
    let rig = Rig::new().await;
    fs::remove_file(rig.0.join("work/file.txt")).unwrap();
    let db = Arc::new(rig.open().await);
    // 阻止 run 自动 finish，以便真实地在分类与发布之间写入新代际。
    let mut journal = store::start(&db, binding(), ResyncReason::CursorExpired)
        .await
        .unwrap();
    let service = rig.resync(db.clone());
    let mut conn = db.pool().acquire().await.unwrap();
    let bound = bindings::binding(&mut conn, binding())
        .await
        .unwrap()
        .unwrap();
    drop(conn);
    let seed = DiskSnapshot(rig.0.clone())
        .begin(bound.vault)
        .await
        .unwrap();
    journal.snapshot = Some(seed.metadata);
    journal.entries = vec![vault_snapshot()];
    journal.snapshot_complete = true;
    journal.items = Some(service.classify(&bound, &journal).await.unwrap());
    assert_eq!(
        journal.items.as_ref().unwrap()[0].disposition,
        Disposition::RetiredAbsent
    );
    store::save(&db, &mut journal).await.unwrap();
    sqlx::query("UPDATE local_entries SET dirty='awaiting_commit',generation=8")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(store::finish(&db, &mut journal).await.is_err());
    let mut conn = db.pool().acquire().await.unwrap();
    assert_eq!(
        events::cursors(&mut conn, binding())
            .await
            .unwrap()
            .received
            .get(),
        10
    );
    assert_eq!(
        entries::local_entry(&mut conn, binding(), file_entry())
            .await
            .unwrap()
            .unwrap()
            .dirty,
        Dirty::AwaitingCommit
    );
    assert!(queue::pending_op(&mut conn, op()).await.unwrap().is_some());
}

#[tokio::test]
async fn a_fresh_binding_cannot_use_resync_and_first_import_cannot_bypass_active_resync() {
    let rig = Rig::new().await;
    let db = Arc::new(rig.open().await);
    let mut tx = db.begin_write().await.unwrap();
    sqlx::query("DELETE FROM baseline_entries")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        rig.resync(db.clone())
            .run(binding(), ResyncReason::CursorExpired)
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM binding_resyncs")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    // FirstImport::run 经 ImportStore 调用同一 ensure_unblocked；围栏在 SQLite 中，并非只在 GUI 做禁用。
    let old = Rig::new().await;
    persist(&old.0.join("offline"), b"offline");
    let db = Arc::new(old.open().await);
    assert!(
        old.resync(db.clone())
            .run(binding(), ResyncReason::CursorExpired)
            .await
            .is_err()
    );
    assert!(ensure_unblocked(&db, binding()).await.is_err());
}

#[tokio::test]
async fn historical_tombstone_and_new_identity_at_same_path_are_not_collapsed() {
    let rig = Rig::new().await;
    rig.server(7, 20, true);
    let db = Arc::new(rig.open().await);
    let mut tx = db.begin_write().await.unwrap();
    let historical = SavedBase {
        entry: EntryId::from_bytes([99; 16]).to_string(),
        parent: Some(vault_entry().to_string()),
        name: "file.txt".to_owned(),
        kind: WireKind::File,
        revision: 2,
        version: Some(VersionId::from_bytes([98; 16]).to_string()),
        deleted: true,
    }
    .baseline()
    .unwrap();
    entries::put_baseline(&mut tx, binding(), &historical)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // 同路径实际字节不能证明属于已删除的旧身份，因此仅旧身份进入 Review，不能自动复活。
    let report = rig
        .resync(db.clone())
        .run(binding(), ResyncReason::CursorExpired)
        .await
        .unwrap();
    assert!(report.prepared && !report.complete);
    assert!(report.items.iter().any(|i| i.old_entry.as_deref()
        == Some(&historical.entry.to_string())
        && i.disposition == Disposition::Review));
    assert!(report.items.iter().any(
        |i| i.old_entry.as_deref() == Some(&file_entry().to_string())
            && i.disposition == Disposition::Current
    ));
    assert_preserved(&db, &rig, ORIGINAL).await;
}

#[tokio::test]
async fn malformed_snapshot_files_and_native_name_aliases_do_not_publish() {
    for fault in ["hash", "count", "alias"] {
        let rig = Rig::new().await;
        rig.server(7, 20, true);
        if fault == "alias" {
            persist(&rig.0.join("work/FILE.txt"), ORIGINAL)
        } else if fault == "hash" {
            let mut file = file_snapshot();
            file.hash = None;
            persist(
                &rig.0.join("entries.json"),
                &serde_json::to_vec(&vec![vault_snapshot(), file]).unwrap(),
            );
        } else {
            let mut meta: Snapshot =
                serde_json::from_slice(&fs::read(rig.0.join("snapshot.json")).unwrap()).unwrap();
            meta.entry_count = "3".to_owned();
            persist(
                &rig.0.join("snapshot.json"),
                &serde_json::to_vec(&meta).unwrap(),
            );
        }
        let db = Arc::new(rig.open().await);
        assert!(
            rig.resync(db.clone())
                .run(binding(), ResyncReason::CursorExpired)
                .await
                .is_err()
        );
        assert_preserved(&db, &rig, ORIGINAL).await;
        assert!(ensure_unblocked(&db, binding()).await.is_err());
    }
}
