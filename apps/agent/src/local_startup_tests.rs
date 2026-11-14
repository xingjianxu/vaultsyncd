//! S234: bounded native work and immutable evidence without redundant write locks.
use super::*;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use vaultsyncd_client_store::bindings::{NewBinding, SyncMode, insert_binding};
use vaultsyncd_domain::{
    ids::{DeviceId, ServerId, VaultId},
    version::ServerEpoch,
};

struct Rig {
    root: PathBuf,
    db: Arc<Database>,
    binding: BindingId,
}

impl Rig {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "s234-local-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let vault = root.join("vault");
        let cache = root.join("cache");
        std::fs::create_dir(&vault).unwrap();
        std::fs::create_dir(&cache).unwrap();
        let db = Arc::new(Database::open(root.join("client.sqlite")).await.unwrap());
        let binding = BindingId::from_bytes([234; 16]);
        let mut tx = db.begin_write().await.unwrap();
        insert_binding(
            &mut tx,
            &NewBinding {
                id: binding,
                server: ServerId::from_bytes([1; 16]),
                device: DeviceId::from_bytes([2; 16]),
                vault: VaultId::from_bytes([3; 16]),
                local_path: vault.to_string_lossy().into_owned(),
                cache_path: cache.to_string_lossy().into_owned(),
                mode: SyncMode::Full,
                rules_version: "1".into(),
                server_epoch: ServerEpoch::new(1).unwrap(),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        Self { root, db, binding }
    }

    async fn close(self) {
        self.db.pool().close().await;
        drop(self.db);
        for attempt in 0..100 {
            match std::fs::remove_dir_all(&self.root) {
                Ok(()) => return,
                Err(_) if attempt < 99 => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(e) => panic!("cannot remove closed test state: {e}"),
            }
        }
    }
}

fn sample_evidence() -> Evidence {
    Evidence {
        generation: 7,
        volume: 42,
        file: [3; 16],
        digest: [4; 32],
        size: 100,
    }
}

#[tokio::test]
async fn unchanged_evidence_and_immutable_conflict_do_not_acquire_write_lock() {
    let rig = Rig::new().await;
    let observed = sample_evidence();
    persist_evidence(&rig.db, rig.binding, "file.txt", &observed)
        .await
        .unwrap();
    let writer = rig.db.begin_write().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        persist_evidence(&rig.db, rig.binding, "file.txt", &observed),
    )
    .await
    .expect("unchanged observation must not wait for the SQLite writer")
    .unwrap();
    let mut conflict = observed.clone();
    conflict.digest[0] ^= 1;
    let failure = tokio::time::timeout(
        Duration::from_secs(1),
        persist_evidence(&rig.db, rig.binding, "file.txt", &conflict),
    )
    .await
    .expect("immutable contradiction must not wait for the SQLite writer")
    .unwrap_err();
    assert_eq!(failure.code(), Some("LOCAL_CHANGED"));
    writer.rollback().await.unwrap();
    let rows: Vec<String> = sqlx::query_scalar("SELECT evidence FROM agent_evidence")
        .fetch_all(rig.db.pool())
        .await
        .unwrap();
    assert_eq!(rows, vec![serde_json::to_string(&observed).unwrap()]);
    rig.close().await;
}

#[tokio::test]
async fn racing_insert_rechecks_identical_and_conflicting_evidence() {
    let rig = Rig::new().await;
    for conflicting in [false, true] {
        let path = if conflicting {
            "conflict.txt"
        } else {
            "same.txt"
        };
        let observed = sample_evidence();
        let mut winner = observed.clone();
        if conflicting {
            winner.digest[0] ^= 1;
        }
        let text = serde_json::to_string(&winner).unwrap();
        let mut writer = rig.db.begin_write().await.unwrap();
        sqlx::query("INSERT INTO agent_evidence VALUES(?,?,?,?)")
            .bind(rig.binding.as_bytes().as_slice())
            .bind(path)
            .bind(winner.generation as i64)
            .bind(&text)
            .execute(&mut *writer)
            .await
            .unwrap();
        let pending = persist_evidence(&rig.db, rig.binding, path, &observed);
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut pending)
                .await
                .is_err(),
            "uncommitted evidence must not be accepted"
        );
        writer.commit().await.unwrap();
        let outcome = pending.await;
        if conflicting {
            assert_eq!(outcome.unwrap_err().code(), Some("LOCAL_CHANGED"));
        } else {
            outcome.unwrap();
        }
        let stored: String = sqlx::query_scalar("SELECT evidence FROM agent_evidence WHERE path=?")
            .bind(path)
            .fetch_one(rig.db.pool())
            .await
            .unwrap();
        assert_eq!(stored, text, "the first committed evidence stays immutable");
    }
    rig.close().await;
}

#[tokio::test]
async fn cancelling_waiter_does_not_release_running_blocking_job_permit() {
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, hold) = std::sync::mpsc::channel();
    let first = tokio::spawn(blocking(move || {
        started.send(()).map_err(error)?;
        hold.recv_timeout(Duration::from_secs(10)).map_err(error)?;
        Ok(())
    }));
    ready.await.unwrap();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());

    let (second_started, mut second_ready) = tokio::sync::oneshot::channel();
    let second = tokio::spawn(blocking(move || {
        second_started.send(()).map_err(error)?;
        Ok(())
    }));
    let blocked = tokio::time::timeout(Duration::from_millis(50), &mut second_ready)
        .await
        .is_err();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        blocked,
        "cancelling an await must not admit concurrent native work"
    );
}

#[tokio::test]
async fn evidence_observation_seeds_moved_identity_and_preserves_snapshot_contract() {
    let rig = Rig::new().await;
    let vault = rig.root.join("vault");
    let cache = rig.root.join("cache");
    std::fs::write(vault.join("before.txt"), b"before").unwrap();
    let local = Local::open(&vault, &cache, rig.db.clone(), rig.binding)
        .await
        .unwrap();
    let meta = local.vault.inspect("before.txt").unwrap();
    let mut hasher = vaultsyncd_transfer::hash::ContentHasher::new();
    hasher.update(b"before");
    let seed = Evidence {
        generation: 17,
        volume: meta.identity.volume,
        file: meta.identity.file,
        digest: hasher.finish(),
        size: meta.size,
    };
    sqlx::query("INSERT INTO local_entries(binding_id,entry_id,generation,dirty,residency,fingerprint,file_identity) VALUES(?,?,17,'dirty','materialized',?,?)")
        .bind(rig.binding.as_bytes().as_slice())
        .bind([9u8; 16].as_slice())
        .bind(seed.digest.as_slice())
        .bind(seed.identity())
        .execute(rig.db.pool())
        .await
        .unwrap();
    std::fs::rename(vault.join("before.txt"), vault.join("moved.txt")).unwrap();
    let observed = local.observe_evidence("moved.txt").await.unwrap().unwrap();
    assert_eq!(observed, seed);
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);

    std::fs::write(vault.join("moved.txt"), b"after!").unwrap();
    let changed = local.observe_evidence("moved.txt").await.unwrap().unwrap();
    assert_eq!(
        changed.generation, 18,
        "equal size never proves equal contents"
    );
    assert_ne!(changed.digest, seed.digest);
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);

    let snapshot = local.observe_snapshot("moved.txt").await.unwrap().unwrap();
    assert_eq!(snapshot.evidence(), &changed);
    assert_eq!(snapshot.read_at(0, 6).unwrap(), b"after!");
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
    let cancelled = local
        .observe_evidence_cancellable("cancelled.txt", Arc::new(AtomicBool::new(true)))
        .await
        .unwrap_err();
    assert!(cancelled.is_cancelled());
    std::fs::write(vault.join("cancelled.txt"), b"cancel this observation").unwrap();
    let permit = BLOCKING.acquire().await.unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let pending = local.observe_evidence_cancellable("cancelled.txt", cancel.clone());
        tokio::pin!(pending);
        let waited = tokio::time::timeout(Duration::from_millis(50), &mut pending)
            .await
            .is_err();
        cancel.store(true, Ordering::Release);
        drop(permit);
        assert!(
            waited,
            "native observation must enter the shared bounded queue"
        );
        assert!(pending.await.unwrap_err().is_cancelled());
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_evidence")
        .fetch_one(rig.db.pool())
        .await
        .unwrap();
    assert_eq!(
        count, 2,
        "cancelled and duplicate observations add no evidence"
    );
    drop(snapshot);
    drop(local);
    rig.close().await;
}
