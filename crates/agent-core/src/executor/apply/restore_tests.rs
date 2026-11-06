//! A server-confirmed trash restore may replace an absent tombstoned entry, never reused bytes.
use super::*;
#[tokio::test]
async fn tombstone_restore_requires_absence_and_preserves_the_deleted_cas_in_its_journal() {
    let rig = Rig::new().await;
    let db = rig.db().await;
    let mut tombstone = base(1);
    tombstone.deleted = true;
    let mut tx = db.begin_write().await.unwrap();
    entries::put_baseline(&mut tx, binding(), &tombstone)
        .await
        .unwrap();
    entries::remove_local(&mut tx, binding(), entry())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    fs::remove_file(rig.path.join("work/file.txt")).unwrap();
    let mut spec = rig.spec();
    spec.baseline_before = Some(tombstone.clone());
    spec.expected = ContentEvidence::Absent;
    let temp = DiskTemp(rig.path.join("stage/restore.part").to_str().unwrap().into());
    ControlledApply::new(db.clone(), rig.local())
        .apply(
            &spec,
            &Remote {
                path: rig.path.join("server/version-two"),
                wrong: false,
            },
            &temp,
            &ClientStorePorts::new(db.clone()),
            &DownloadOptions::new(5),
        )
        .await
        .unwrap();
    assert_eq!(fs::read(rig.path.join("work/file.txt")).unwrap(), NEW);
    // Reopen/decode must not silently turn the stored tombstone into an active pre-state.
    let results = ControlledApply::new(db.clone(), rig.local())
        .recover(binding())
        .await
        .unwrap();
    assert!(results.is_empty());
    let text: String = sqlx::query_scalar("SELECT journal FROM controlled_applies WHERE op_id=?")
        .bind(op().as_bytes().as_slice())
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["before"]["deleted"],
        true
    );
    let mut reused = spec;
    reused.op = OpId::from_bytes([0x73; 16]);
    reused.expected = ContentEvidence::File {
        generation: 1,
        identity: vec![1],
        digest: digest_of(EDIT),
        size: EDIT.len() as u64,
    };
    assert!(store::validate(&reused).is_err());
    assert_eq!(reused.baseline_before, Some(tombstone));
    db.pool().close().await;
}
