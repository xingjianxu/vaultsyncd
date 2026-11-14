//! S234 discovery persistence fences and batched baseline metadata.
use super::*;
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use vaultsyncd_client_store::bindings::{NewBinding, SyncMode, insert_binding};
use vaultsyncd_domain::{
    ids::{DeviceId, ServerId, VaultId},
    version::ServerEpoch,
};
use vaultsyncd_platform_windows::files::{CloudMetadata, FileIdentity};

struct Rig {
    db: Database,
    root: PathBuf,
    binding: BindingId,
    row: SnapshotEntry,
    observed: Evidence,
}

impl Rig {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "s234-work-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let db = Database::open(root.join("client.sqlite")).await.unwrap();
        let binding = BindingId::from_bytes([234; 16]);
        let observed = Evidence {
            generation: 5,
            volume: 42,
            file: [9; 16],
            digest: [7; 32],
            size: 6,
        };
        let row = SnapshotEntry {
            entry_id: entry(1).to_string(),
            parent_id: Some(entry(0).to_string()),
            name: "file.txt".into(),
            kind: WireKind::File,
            revision: "1".into(),
            version_id: Some(VersionId::from_bytes([3; 16]).to_string()),
            hash: Some(vaultsyncd_transfer::hash::to_hex(&observed.digest)),
            size: Some(observed.size.to_string()),
            modified_at: None,
        };
        let mut tx = db.begin_write().await.unwrap();
        insert_binding(
            &mut tx,
            &NewBinding {
                id: binding,
                server: ServerId::from_bytes([1; 16]),
                device: DeviceId::from_bytes([2; 16]),
                vault: VaultId::from_bytes([4; 16]),
                local_path: root.join("vault").to_string_lossy().into_owned(),
                cache_path: root.join("cache").to_string_lossy().into_owned(),
                mode: SyncMode::OnDemand,
                rules_version: "1".into(),
                server_epoch: ServerEpoch::new(1).unwrap(),
            },
        )
        .await
        .unwrap();
        entries::put_baseline(
            &mut tx,
            binding,
            &BaselineEntry {
                entry: entry(0),
                parent: None,
                name: String::new(),
                kind: EntryKind::Directory,
                revision: Revision::new(1).unwrap(),
                content_version: None,
                deleted: false,
            },
        )
        .await
        .unwrap();
        entries::put_baseline(&mut tx, binding, &baseline(&row).unwrap())
            .await
            .unwrap();
        sqlx::query("INSERT INTO agent_versions VALUES(?,?,?,?,?)")
            .bind(binding.as_bytes().as_slice())
            .bind(entry(1).as_bytes().as_slice())
            .bind([3u8; 16].as_slice())
            .bind(observed.digest.as_slice())
            .bind(observed.size as i64)
            .execute(&mut *tx)
            .await
            .unwrap();
        entries::put_local(&mut tx, binding, &local_state(&observed, Dirty::Dirty))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        Self {
            db,
            root,
            binding,
            row,
            observed,
        }
    }
    async fn local(&self) -> LocalEntry {
        let mut conn = self.db.pool().acquire().await.unwrap();
        entries::local_entry(&mut conn, self.binding, entry(1))
            .await
            .unwrap()
            .unwrap()
    }
    async fn close(self) {
        self.db.close().await;
        for attempt in 0..100 {
            match std::fs::remove_dir_all(&self.root) {
                Ok(()) => return,
                Err(_) if attempt < 99 => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(e) => panic!("cannot remove closed test state: {e}"),
            }
        }
    }
}
fn entry(n: u128) -> EntryId {
    EntryId::from_bytes(n.to_be_bytes())
}
fn local_state(e: &Evidence, dirty: Dirty) -> LocalEntry {
    LocalEntry {
        entry: entry(1),
        generation: Generation::new(e.generation).unwrap(),
        dirty,
        residency: Residency::Materialized,
        fingerprint: Some(e.digest.to_vec()),
        file_identity: Some(e.identity()),
    }
}

#[tokio::test]
async fn metadata_join_pages_keep_root_missing_versions_and_deleted_boundaries() {
    let rig = Rig::new().await;
    let mut tx = rig.db.begin_write().await.unwrap();
    for n in 2..=301u128 {
        let id = entry(n);
        let version = VersionId::from_bytes(n.to_be_bytes());
        entries::put_baseline(
            &mut tx,
            rig.binding,
            &BaselineEntry {
                entry: id,
                parent: Some(entry(0)),
                name: format!("file-{n}"),
                kind: EntryKind::File,
                revision: Revision::new(n as u64).unwrap(),
                content_version: Some(version),
                deleted: n == 301,
            },
        )
        .await
        .unwrap();
        if n != 300 {
            sqlx::query("INSERT INTO agent_versions VALUES(?,?,?,?,?)")
                .bind(rig.binding.as_bytes().as_slice())
                .bind(id.as_bytes().as_slice())
                .bind(version.as_bytes().as_slice())
                .bind([n as u8; 32].as_slice())
                .bind(n as i64)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
    }
    tx.commit().await.unwrap();
    let rows = metadata_rows(&rig.db, rig.binding).await.unwrap();
    assert_eq!(rows.len(), 301);
    assert_eq!(rows[&entry(0).to_string()].parent_id, None);
    assert_eq!(rows[&entry(299).to_string()].size.as_deref(), Some("299"));
    assert_eq!(rows[&entry(300).to_string()].hash, None);
    assert_eq!(rows[&entry(300).to_string()].size, None);
    assert!(!rows.contains_key(&entry(301).to_string()));
    assert_eq!(paths(&rows).unwrap()[&entry(299).to_string()], "file-299");
    rig.close().await;
}

#[tokio::test]
async fn identical_clean_state_does_not_wait_for_a_writer() {
    let rig = Rig::new().await;
    record_clean_observation(&rig.db, rig.binding, &rig.row, &rig.observed, "1")
        .await
        .unwrap();
    let writer = rig.db.begin_write().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        record_clean_observation(&rig.db, rig.binding, &rig.row, &rig.observed, "1"),
    )
    .await
    .expect("unchanged local baseline must not start a write transaction")
    .unwrap();
    writer.rollback().await.unwrap();
    assert_eq!(rig.local().await, local_state(&rig.observed, Dirty::Clean));
    rig.close().await;
}

#[tokio::test]
async fn equal_bytes_preserve_pending_intent_and_unconfirmed_states() {
    let rig = Rig::new().await;
    for protected in ["pending", "intent", "snapshotting", "awaiting_commit"] {
        let mut tx = rig.db.begin_write().await.unwrap();
        let dirty = match protected {
            "snapshotting" => Dirty::Snapshotting,
            "awaiting_commit" => Dirty::AwaitingCommit,
            _ => Dirty::Dirty,
        };
        entries::put_local(&mut tx, rig.binding, &local_state(&rig.observed, dirty))
            .await
            .unwrap();
        if protected == "pending" {
            sqlx::query("INSERT INTO pending_ops(op_id,binding_id,kind,entry_id) VALUES(?,?,'upload_content',?)").bind([6u8;16].as_slice()).bind(rig.binding.as_bytes().as_slice()).bind(entry(1).as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
        } else if protected == "intent" {
            sqlx::query("INSERT INTO local_intents(binding_id,kind,entry_id,target_path,generation) VALUES(?,'replace',?,'file.txt',5)").bind(rig.binding.as_bytes().as_slice()).bind(entry(1).as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
        let writer = rig.db.begin_write().await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            record_clean_observation(&rig.db, rig.binding, &rig.row, &rig.observed, "1"),
        )
        .await
        .expect("pending state requires no write lock")
        .unwrap();
        writer.rollback().await.unwrap();
        assert_eq!(rig.local().await.dirty, dirty);
        let mut tx = rig.db.begin_write().await.unwrap();
        let pending = sqlx::query("DELETE FROM pending_ops")
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        let intents = sqlx::query("DELETE FROM local_intents")
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(pending, u64::from(protected == "pending"));
        assert_eq!(intents, u64::from(protected == "intent"));
        tx.commit().await.unwrap();
    }
    rig.close().await;
}

#[tokio::test]
async fn stale_baseline_or_generation_cannot_clean_a_concurrent_update() {
    for change in ["baseline", "generation", "resync"] {
        let rig = Rig::new().await;
        let mut writer = rig.db.begin_write().await.unwrap();
        match change {
            "baseline" => {
                sqlx::query("UPDATE baseline_entries SET revision=revision+1 WHERE binding_id=? AND entry_id=?").bind(rig.binding.as_bytes().as_slice()).bind(entry(1).as_bytes().as_slice()).execute(&mut *writer).await.unwrap();
            }
            "generation" => {
                sqlx::query("UPDATE local_entries SET generation=generation+1 WHERE binding_id=? AND entry_id=?").bind(rig.binding.as_bytes().as_slice()).bind(entry(1).as_bytes().as_slice()).execute(&mut *writer).await.unwrap();
            }
            _ => {
                sqlx::query("INSERT INTO binding_resyncs(binding_id,journal) VALUES(?,'{}')")
                    .bind(rig.binding.as_bytes().as_slice())
                    .execute(&mut *writer)
                    .await
                    .unwrap();
            }
        }
        {
            let pending =
                record_clean_observation(&rig.db, rig.binding, &rig.row, &rig.observed, "1");
            tokio::pin!(pending);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut pending)
                    .await
                    .is_err()
            );
            writer.commit().await.unwrap();
            assert_eq!(pending.await.unwrap_err().code(), Some("LOCAL_CHANGED"));
        }
        assert_eq!(
            rig.local().await.dirty,
            Dirty::Dirty,
            "{change} cannot be overwritten by the audit"
        );
        rig.close().await;
    }
}

#[test]
fn only_resident_files_enter_content_observation() {
    let mut meta = EntryMetadata {
        identity: FileIdentity {
            volume: 1,
            file: [2; 16],
        },
        size: 100,
        allocated: 0,
        directory: false,
        attributes: 0,
        reparse_tag: 0x9000_001a,
        links: 1,
        created: 0,
        modified: 0,
        cloud: Some(CloudMetadata {
            on_disk: 0,
            modified: 0,
            pin_state: 0,
            in_sync_state: 0,
            identity: Vec::new(),
        }),
    };
    assert!(!resident_file(&meta));
    meta.cloud.as_mut().unwrap().on_disk = 99;
    assert!(!resident_file(&meta));
    meta.cloud.as_mut().unwrap().on_disk = 100;
    assert!(resident_file(&meta));
    meta.directory = true;
    assert!(!resident_file(&meta));
}
