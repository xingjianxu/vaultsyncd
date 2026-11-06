use super::*;
pub(super) fn copy_op() -> OpId {
    OpId::from_bytes([0x71; 16])
}
struct Fixed;
impl VersionedContent for Fixed {
    fn read_range<'a>(
        &'a self,
        pin: &'a VersionPin,
        offset: u64,
        length: u64,
    ) -> PortFuture<'a, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            assert_eq!(pin.digest, digest_of(EDIT));
            Ok(EDIT[offset as usize..(offset + length) as usize].to_vec())
        })
    }
}
async fn prepared() -> Rig {
    let rig = Rig::new().await;
    let db = rig.db().await;
    let runner = ControlledApply::new(db.clone(), rig.local());
    let mut copy = rig.spec();
    copy.op = copy_op();
    copy.target = RelativePath::parse("copy.txt").unwrap();
    copy.baseline_before = None;
    copy.baseline_after.entry = EntryId::from_bytes([0x72; 16]);
    copy.baseline_after.name = "copy.txt".into();
    copy.baseline_after.content_version = Some(VersionId::from_bytes([0x74; 16]));
    copy.pin = VersionPin::new(
        binding().to_string(),
        copy.baseline_after.entry.to_string(),
        copy.baseline_after.content_version.unwrap().to_string(),
        digest_of(EDIT),
        EDIT.len() as u64,
    )
    .unwrap();
    copy.expected = ContentEvidence::Absent;
    runner
        .apply(
            &copy,
            &Fixed,
            &DiskTemp(rig.path.join("stage/copy.part").to_str().unwrap().into()),
            &ClientStorePorts::new(db.clone()),
            &DownloadOptions::new(5),
        )
        .await
        .unwrap();
    persist(&rig.path.join("work/file.txt"), EDIT);
    let actual = recovery::inspect(&*rig.local(), &RelativePath::parse("file.txt").unwrap())
        .await
        .unwrap();
    fs::write(
        rig.path.join("expected.json"),
        serde_json::to_vec(&actual).unwrap(),
    )
    .unwrap();
    let receipt = store::receipt(&actual).unwrap();
    let mut tx = db.begin_write().await.unwrap();
    entries::put_local(
        &mut tx,
        binding(),
        &LocalEntry {
            entry: entry(),
            generation: receipt.generation,
            dirty: Dirty::Dirty,
            residency: Residency::Materialized,
            fingerprint: Some(digest_of(EDIT).to_vec()),
            file_identity: receipt.file_identity,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    db.pool().close().await;
    rig
}
#[tokio::test]
async fn confirmed_copy_requires_exact_completed_distinct_binding_content_and_generation() {
    for defect in [
        "unknown",
        "incomplete",
        "binding",
        "same_entry",
        "digest",
        "new_dirty",
    ] {
        let rig = prepared().await;
        let db = rig.db().await;
        let spec = rig.spec();
        let runner = ControlledApply::new(db.clone(), rig.local());
        assert_eq!(
            runner
                .specification(copy_op())
                .await
                .unwrap()
                .unwrap()
                .pin
                .digest,
            digest_of(EDIT)
        );
        let mut copy = copy_op();
        if defect == "unknown" {
            copy = OpId::from_bytes([0x73; 16]);
        } else if defect == "incomplete" {
            sqlx::query("UPDATE controlled_applies SET completed=0 WHERE op_id=?")
                .bind(copy_op().as_bytes().as_slice())
                .execute(db.pool())
                .await
                .unwrap();
        } else if defect == "new_dirty" {
            persist(
                &rig.path.join("work/file.txt"),
                b"later unconfirmed dirty bytes",
            );
        } else {
            let text: String =
                sqlx::query_scalar("SELECT journal FROM controlled_applies WHERE op_id=?")
                    .bind(copy_op().as_bytes().as_slice())
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
            let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
            match defect {
                "binding" => value["binding"] = serde_json::json!(vec![9u8; 16]),
                "same_entry" => value["after"]["entry"] = serde_json::json!(entry().as_bytes()),
                "digest" => value["digest"] = serde_json::json!(vec![0u8; 32]),
                _ => unreachable!(),
            };
            sqlx::query("UPDATE controlled_applies SET journal=? WHERE op_id=?")
                .bind(value.to_string())
                .bind(copy_op().as_bytes().as_slice())
                .execute(db.pool())
                .await
                .unwrap();
        }
        assert!(
            runner
                .apply_after_confirmed_copy(
                    &spec,
                    &Remote {
                        path: rig.path.join("server/version-two"),
                        wrong: false
                    },
                    &DiskTemp(rig.path.join("stage/head.part").to_str().unwrap().into()),
                    &ClientStorePorts::new(db.clone()),
                    &DownloadOptions::new(5),
                    copy
                )
                .await
                .is_err(),
            "{defect}"
        );
        assert_eq!(
            fs::read(rig.path.join("work/file.txt")).unwrap(),
            if defect == "new_dirty" {
                b"later unconfirmed dirty bytes".as_slice()
            } else {
                EDIT
            }
        );
        let dirty: String = sqlx::query_scalar("SELECT dirty FROM local_entries WHERE entry_id=?")
            .bind(entry().as_bytes().as_slice())
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(dirty, "dirty");
        db.pool().close().await;
    }
}
#[tokio::test]
async fn confirmed_copy_dirty_restore_survives_independent_process_interruptions() {
    for (crash, code) in [("intent", 81), ("replaced", 82), ("database", 83)] {
        let rig = prepared().await;
        child(&rig, "confirmed", crash, code);
        if crash != "database" {
            child(&rig, "recover", "none", 0);
        }
        rig.assert_state(NEW, 2, 0).await;
        assert_eq!(fs::read(rig.path.join("work/copy.txt")).unwrap(), EDIT);
    }
}
