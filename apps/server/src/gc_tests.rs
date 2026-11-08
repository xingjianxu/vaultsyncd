use super::*;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
};
use vaultsyncd_domain::{
    ids::{EntryId, ObjectLeaseId, RootId, TxId, UserId, VersionId},
    version::{Revision, ServerEpoch},
};
use vaultsyncd_server_store::{
    changes,
    entries::{self, EntryKind, NewEntry},
    leases::{self, NewObjectLease, ObjectLeasePurpose},
    versions::{self, NewObject, NewVersion, VersionOrigin},
};

struct Lab {
    state: PathBuf,
    db: Database,
    owner: UserId,
    root: RootId,
    parent: EntryId,
}
fn at(t: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_seconds(t).unwrap()
}
impl Lab {
    async fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let base = std::env::var_os("VAULTSYNCD_GC_TEST_BASE")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let state = base.join(format!(
            "vaultsyncd-s086-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&state).unwrap();
        let db = Database::open(state.join("server.sqlite3")).await.unwrap();
        let owner = UserId::from_bytes([1; 16]);
        let root = RootId::from_bytes([2; 16]);
        let parent = EntryId::from_bytes([3; 16]);
        let mut tx = db.begin_write().await.unwrap();
        sqlx::query("INSERT INTO users(id,login,password_hash,status) VALUES(?,'gc-owner','test-only','active')").bind(owner.as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
        roots::insert_root(
            &mut tx,
            &roots::NewRoot {
                id: root,
                entry: parent,
                owner,
                absolute_path: state.join("work").display().to_string(),
                volume_identity: vec![1],
                root_file_identity: vec![2],
                epoch: ServerEpoch::ZERO,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        Self {
            state,
            db,
            owner,
            root,
            parent,
        }
    }
    async fn object(&self, bytes: &[u8]) -> [u8; 32] {
        let mut hasher = vaultsyncd_platform_linux_server::files::hash::Sha256::new();
        hasher.update(bytes);
        let hash = hasher.finish();
        let path = self.path(hash);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        std::fs::File::open(&path).unwrap().sync_all().unwrap();
        let mut tx = self.db.begin_write().await.unwrap();
        versions::record_object(
            &mut tx,
            self.owner,
            &NewObject {
                root: self.root,
                hash,
                size: bytes.len() as u64,
                storage_path: ObjectLayout::relative_object_path(&ContentDigest::from_bytes(hash))
                    .display()
                    .to_string(),
                verified_at: at(1),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        hash
    }
    fn path(&self, hash: [u8; 32]) -> PathBuf {
        ObjectLayout::new(self.state.join("objects")).object_path(&ContentDigest::from_bytes(hash))
    }
    async fn current(&self, hash: [u8; 32], size: u64) {
        let mut tx = self.db.begin_write().await.unwrap();
        let entry = EntryId::from_bytes([4; 16]);
        entries::insert_entry(
            &mut tx,
            self.owner,
            &NewEntry {
                id: entry,
                root: self.root,
                parent: self.parent,
                name: "current".into(),
                name_key: "current".into(),
                kind: EntryKind::File,
                revision: Revision::new(1).unwrap(),
                content_version: None,
            },
            at(1_700_000_100),
        )
        .await
        .unwrap();
        let batch = changes::append_change(
            &mut tx,
            self.owner,
            self.root,
            TxId::from_bytes([5; 16]),
            "[{}]",
            at(101),
        )
        .await
        .unwrap();
        versions::record_version(
            &mut tx,
            self.owner,
            &NewVersion {
                id: VersionId::from_bytes([6; 16]),
                root: self.root,
                entry,
                object_hash: hash,
                size,
                created_seq: batch.seq,
                at: at(101),
                origin: VersionOrigin::Client,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
}
impl Drop for Lab {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

#[tokio::test]
async fn real_files_require_grace_and_a_new_lease_cancels_deletion() {
    let lab = Lab::new("lease").await;
    let orphan = lab.object(b"orphan bytes").await;
    let current = lab.object(b"current bytes").await;
    lab.current(current, 13).await;
    assert_eq!(
        run_once(&lab.db, &lab.state, at(100)).await.unwrap(),
        Report::default()
    );
    assert_eq!(
        run_once(&lab.db, &lab.state, at(3699))
            .await
            .unwrap()
            .objects,
        0
    );
    let mut tx = lab.db.begin_write().await.unwrap();
    leases::lease_object(
        &mut tx,
        lab.owner,
        &NewObjectLease {
            id: ObjectLeaseId::from_bytes([8; 16]),
            root: lab.root,
            object_hash: orphan,
            purpose: ObjectLeasePurpose::Download,
            expires_at: at(8000),
        },
        at(3699),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        run_once(&lab.db, &lab.state, at(3700))
            .await
            .unwrap()
            .objects,
        0
    );
    assert!(lab.path(orphan).exists());
    assert_eq!(
        run_once(&lab.db, &lab.state, at(8000))
            .await
            .unwrap()
            .objects,
        0
    );
    assert_eq!(
        run_once(&lab.db, &lab.state, at(11600))
            .await
            .unwrap()
            .objects,
        1
    );
    assert!(!lab.path(orphan).exists());
    assert_eq!(std::fs::read(lab.path(current)).unwrap(), b"current bytes");
}

#[tokio::test]
async fn publication_and_new_version_hold_the_same_gate_as_gc() {
    let lab = Lab::new("interleave").await;
    let hash = lab.object(b"new committed bytes").await;
    run_once(&lab.db, &lab.state, at(100)).await.unwrap();
    let publishing = lab.db.object_activity().await;
    let db = lab.db.clone();
    let state = lab.state.clone();
    let mut gc = tokio::spawn(async move { run_once(&db, &state, at(3700)).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut gc)
            .await
            .is_err()
    );
    lab.current(hash, 19).await;
    drop(publishing);
    assert_eq!(gc.await.unwrap().unwrap().objects, 0);
    assert_eq!(
        std::fs::read(lab.path(hash)).unwrap(),
        b"new committed bytes"
    );
}

#[tokio::test]
async fn changed_object_bytes_are_preserved() {
    let lab = Lab::new("bad-object").await;
    let hash = lab.object(b"original").await;
    run_once(&lab.db, &lab.state, at(100)).await.unwrap();
    std::fs::write(lab.path(hash), b"unexpected user bytes").unwrap();
    assert!(run_once(&lab.db, &lab.state, at(3700)).await.is_err());
    assert_eq!(
        std::fs::read(lab.path(hash)).unwrap(),
        b"unexpected user bytes"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM objects WHERE gc_state='live'")
        .fetch_one(lab.db.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn crash_child() {
    let Some(state) = std::env::var_os("VAULTSYNCD_S086_CRASH_DIR") else {
        return;
    };
    let state = PathBuf::from(state);
    let phase = std::env::var("VAULTSYNCD_S086_CRASH_PHASE").unwrap();
    let db = Database::open(state.join("server.sqlite3")).await.unwrap();
    let owner = UserId::from_bytes([1; 16]);
    let root = RootId::from_bytes([2; 16]);
    let _exclusive = db.object_maintenance().await;
    let mut tx = db.begin_write().await.unwrap();
    sweep::mark_root(&mut tx, owner, root, at(100))
        .await
        .unwrap();
    let c = sweep::candidates(&mut tx, owner, root)
        .await
        .unwrap()
        .remove(0);
    tx.commit().await.unwrap();
    if phase != "marked" {
        let mut tx = db.begin_write().await.unwrap();
        assert!(
            sweep::authorize_delete(&mut tx, owner, &c, at(3700))
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
    }
    if phase == "unlinked" {
        unlink_object(
            &object_directory(&state, &c).unwrap(),
            &ContentDigest::from_bytes(c.object.hash).to_hex(),
        )
        .unwrap();
    }
    std::fs::write(state.join("kill-ready"), b"ready").unwrap();
    loop {
        std::thread::park()
    }
}

#[tokio::test]
async fn symlinked_object_parent_cannot_delete_outside_bytes() {
    let lab = Lab::new("symlink").await;
    let hash = lab.object(b"old object").await;
    run_once(&lab.db, &lab.state, at(100)).await.unwrap();
    let path = lab.path(hash);
    let outside = lab.state.join("outside-gc-scope");
    std::fs::rename(path.parent().unwrap(), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, path.parent().unwrap()).unwrap();
    assert!(run_once(&lab.db, &lab.state, at(3700)).await.is_err());
    assert_eq!(
        std::fs::read(outside.join(path.file_name().unwrap())).unwrap(),
        b"old object"
    );
    let state: String = sqlx::query_scalar("SELECT gc_state FROM objects WHERE hash=?")
        .bind(hash.as_slice())
        .fetch_one(lab.db.pool())
        .await
        .unwrap();
    assert_eq!(state, "live");
}

#[tokio::test]
async fn expired_incomplete_uploads_are_separate_from_finalized_or_active_content() {
    let lab = Lab::new("uploads").await;
    let mut tx = lab.db.begin_write().await.unwrap();
    let mut paths = Vec::new();
    for (marker, state, expires) in [
        (20u8, "open", 1000),
        (21, "complete", 5000),
        (22, "finalized", 1000),
    ] {
        let id = vaultsyncd_domain::ids::UploadId::from_bytes([marker; 16]);
        sqlx::query("INSERT INTO uploads VALUES(?,?,?,1,?,1,?,?)")
            .bind(id.as_bytes().as_slice())
            .bind(lab.root.as_bytes().as_slice())
            .bind([19u8; 16].as_slice())
            .bind([30u8; 32].as_slice())
            .bind(expires)
            .bind(state)
            .execute(&mut *tx)
            .await
            .unwrap();
        let relative = format!("uploads/{}/{:020}", id.to_hex(), 0);
        let path = lab.state.join(&relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"x").unwrap();
        sqlx::query("INSERT INTO upload_parts VALUES(?,0,1,?,?)")
            .bind(id.as_bytes().as_slice())
            .bind([30u8; 32].as_slice())
            .bind(relative)
            .execute(&mut *tx)
            .await
            .unwrap();
        std::fs::write(
            path.parent().unwrap().join(".00000000000000000000.123.tmp"),
            b"interrupted partial block",
        )
        .unwrap();
        let staging = lab.state.join("objects/staging");
        std::fs::create_dir_all(&staging).unwrap();
        for name in [
            format!(".assemble-{}-123", id.to_hex()),
            format!("finalize-{}-request", id.to_hex()),
        ] {
            std::fs::write(staging.join(name), b"interrupted assembly").unwrap();
        }
        paths.push(path);
    }
    tx.commit().await.unwrap();
    assert_eq!(
        run_once(&lab.db, &lab.state, at(1000)).await.unwrap().parts,
        1
    );
    let expired = vaultsyncd_domain::ids::UploadId::from_bytes([20; 16]).to_hex();
    assert!(
        !lab.state
            .join(format!("objects/staging/.assemble-{expired}-123"))
            .exists()
    );
    assert!(!paths[0].exists());
    assert!(
        !paths[0]
            .parent()
            .unwrap()
            .join(".00000000000000000000.123.tmp")
            .exists()
    );
    assert!(paths[1].exists());
    assert!(paths[2].exists());
    assert_eq!(
        run_once(&lab.db, &lab.state, at(1001)).await.unwrap().parts,
        0
    );
}

#[tokio::test]
async fn sqlite_full_cannot_shorten_retention_or_delete_confirmed_bytes() {
    let lab = Lab::new("sqlite-full").await;
    let current = lab.object(b"must remain under pressure").await;
    lab.current(current, 26).await;
    for n in 0..180 {
        lab.object(format!("orphan-{n}").as_bytes()).await;
    }
    // 真实 SQLite 页上限触发 SQLITE_FULL，不填充宿主卷，也不伪造存储错误。
    let mut connections = Vec::new();
    for _ in 0..4 {
        let mut c = lab.db.pool().acquire().await.unwrap();
        let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&mut *c)
            .await
            .unwrap();
        sqlx::query(&format!("PRAGMA max_page_count={}", pages + 4))
            .execute(&mut *c)
            .await
            .unwrap();
        connections.push(c);
    }
    let c = &mut connections[0];
    sqlx::query("CREATE TABLE pressure(data BLOB)")
        .execute(&mut **c)
        .await
        .unwrap();
    let full = loop {
        match sqlx::query("INSERT INTO pressure VALUES(zeroblob(3000))")
            .execute(&mut **c)
            .await
        {
            Ok(_) => {}
            Err(e) => break e,
        }
    };
    assert!(full.to_string().contains("full"), "{full}");
    drop(connections);
    assert!(run_once(&lab.db, &lab.state, at(100)).await.is_err());
    assert_eq!(
        std::fs::read(lab.path(current)).unwrap(),
        b"must remain under pressure"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM objects")
        .fetch_one(lab.db.pool())
        .await
        .unwrap();
    assert_eq!(count, 181);
}

#[tokio::test]
async fn sigkill_at_all_durable_boundaries_resumes_without_deleting_current_data() {
    for phase in ["marked", "deleting", "unlinked"] {
        let lab = Lab::new(phase).await;
        let hash = lab.object(b"obsolete object").await;
        let current = lab.object(b"confirmed current").await;
        lab.current(current, 17).await;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "gc::tests::crash_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("VAULTSYNCD_S086_CRASH_DIR", &lab.state)
            .env("VAULTSYNCD_S086_CRASH_PHASE", phase)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let ready = lab.state.join("kill-ready");
        for _ in 0..300 {
            if ready.exists() {
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "GC child failed before {phase}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if !ready.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("GC boundary was not reached: {phase}")
        }
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        let restarted = Database::open(lab.state.join("server.sqlite3"))
            .await
            .unwrap();
        assert_eq!(
            run_once(&restarted, &lab.state, at(3700))
                .await
                .unwrap()
                .objects,
            1
        );
        assert!(!lab.path(hash).exists());
        assert_eq!(
            std::fs::read(lab.path(current)).unwrap(),
            b"confirmed current"
        );
        assert_eq!(
            run_once(&restarted, &lab.state, at(3701))
                .await
                .unwrap()
                .objects,
            0
        );
        restarted.close().await;
    }
}

/// S136/ADR-020：两 Root 同摘要各有独立物理对象；GC 一个 Root 的孤儿对象不得
/// 删除另一个 Root 的字节或对象行。
#[tokio::test]
async fn same_digest_objects_are_gc_isolated_between_roots() {
    use vaultsyncd_server_store::roots;
    let lab = Lab::new("scoped-gc").await;
    let second_root = RootId::from_bytes([9; 16]);
    let mut tx = lab.db.begin_write().await.unwrap();
    roots::insert_root(
        &mut tx,
        &roots::NewRoot {
            id: second_root,
            entry: EntryId::from_bytes([10; 16]),
            owner: lab.owner,
            absolute_path: lab.state.join("work-second").display().to_string(),
            volume_identity: vec![3],
            root_file_identity: vec![4],
            epoch: ServerEpoch::ZERO,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let bytes = b"same digest, two roots";
    let hash = lab.object(bytes).await;
    let digest = ContentDigest::from_bytes(hash);
    let second_path = lab
        .state
        .join("objects")
        .join(ObjectLayout::scoped_relative_object_path(
            second_root,
            &digest,
        ));
    std::fs::create_dir_all(second_path.parent().unwrap()).unwrap();
    std::fs::write(&second_path, bytes).unwrap();
    let mut tx = lab.db.begin_write().await.unwrap();
    versions::record_object(
        &mut tx,
        lab.owner,
        &NewObject {
            root: second_root,
            hash,
            size: bytes.len() as u64,
            storage_path: ObjectLayout::scoped_relative_object_path(second_root, &digest)
                .display()
                .to_string(),
            verified_at: at(1),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // 两个对象都没有当前版本引用；先标记，再越过宽限期。
    run_once(&lab.db, &lab.state, at(100)).await.unwrap();
    let report = run_once(&lab.db, &lab.state, at(11600)).await.unwrap();
    assert_eq!(report.objects, 2, "两 Root 的独立对象各自可回收");
    assert!(!lab.path(hash).exists());
    assert!(!second_path.exists());
    assert_ne!(lab.path(hash), second_path, "不同 Root 不得共用物理路径");

    // 只让第二个 Root 的对象重新存活，第一 Root 不再产生该摘要，重放必须幂等。
    std::fs::create_dir_all(second_path.parent().unwrap()).unwrap();
    std::fs::write(&second_path, bytes).unwrap();
    let mut tx = lab.db.begin_write().await.unwrap();
    versions::record_object(
        &mut tx,
        lab.owner,
        &NewObject {
            root: second_root,
            hash,
            size: bytes.len() as u64,
            storage_path: ObjectLayout::scoped_relative_object_path(second_root, &digest)
                .display()
                .to_string(),
            verified_at: at(2),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let entry = EntryId::from_bytes([12; 16]);
    let mut tx = lab.db.begin_write().await.unwrap();
    entries::insert_entry(
        &mut tx,
        lab.owner,
        &NewEntry {
            id: entry,
            root: second_root,
            parent: EntryId::from_bytes([10; 16]),
            name: "second".into(),
            name_key: "second".into(),
            kind: EntryKind::File,
            revision: Revision::new(1).unwrap(),
            content_version: None,
        },
        at(20_000),
    )
    .await
    .unwrap();
    let batch = changes::append_change(
        &mut tx,
        lab.owner,
        second_root,
        TxId::from_bytes([13; 16]),
        "[{}]",
        at(20_000),
    )
    .await
    .unwrap();
    versions::record_version(
        &mut tx,
        lab.owner,
        &NewVersion {
            id: VersionId::from_bytes([11; 16]),
            root: second_root,
            entry,
            object_hash: hash,
            size: bytes.len() as u64,
            created_seq: batch.seq,
            at: at(20_000),
            origin: VersionOrigin::Client,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        run_once(&lab.db, &lab.state, at(50_000))
            .await
            .unwrap()
            .objects,
        0
    );
    assert_eq!(std::fs::read(&second_path).unwrap(), bytes);
    let rows: Vec<(Vec<u8>, String)> =
        sqlx::query_as("SELECT root_id, storage_path FROM objects WHERE hash = ? ORDER BY root_id")
            .bind(hash.as_slice())
            .fetch_all(lab.db.pool())
            .await
            .unwrap();
    assert_eq!(rows.len(), 1, "另一 Root 的对象行不得被牵连");
    assert_eq!(rows[0].0, second_root.as_bytes().to_vec());
}
