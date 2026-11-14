//! S087 原生btrfs验收入口。调用前由外部测试工具创建带user_subvol_rm_allowed的独立测试卷。
//! 本程序不sudo、不挂载、不格式化；参数必须是本轮专属测试卷中的空目录。
use sqlx::{Connection, Row};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use vaultsyncd_domain::{
    ids::{BackupLeaseId, EntryId, TxId, UserId, VaultId, VersionId},
    version::{Revision, ServerEpoch, UtcTimestamp},
};
use vaultsyncd_platform_linux_server::{
    backup::Arena,
    files::{
        hash::{digest, hex},
        hash_file,
    },
};
use vaultsyncd_server::{
    backup,
    config::{Config, LogConfig, LogFormat},
};
use vaultsyncd_server_core::objects::{ContentDigest, ObjectLayout};
use vaultsyncd_server_store::{
    changes,
    db::Database,
    entries::{self, EntryKind, NewEntry},
    leases::{self, NewBackupLease, SnapshotSeqMap},
    vaults::{self, NewVault},
    versions::{self, NewObject, NewVersion, VersionOrigin},
};

fn at(t: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_seconds(t).unwrap()
}
fn now() -> UtcTimestamp {
    at(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64)
}
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
struct Lab {
    db: Database,
    config: Config,
    owner: UserId,
    vault: VaultId,
    parent: EntryId,
    base: PathBuf,
}
impl Lab {
    async fn new(base: &Path, label: &str) -> Result<Self> {
        let base = base.join(label);
        fs::DirBuilder::new().mode(0o700).create(&base)?;
        let state = base.join("state");
        fs::DirBuilder::new().mode(0o700).create(&state)?;
        fs::create_dir(base.join("work"))?;
        let config = Config {
            office: None,
            listen: "127.0.0.1:0".parse()?,
            database: state.join("server.sqlite"),
            state_dir: state,
            allowed_vault_bases: None,
            log: LogConfig {
                level: tracing::Level::WARN,
                format: LogFormat::Text,
            },
        };
        let db = Database::open(&config.database).await?;
        let owner = UserId::from_bytes([1; 16]);
        let vault = VaultId::from_bytes([2; 16]);
        let parent = EntryId::from_bytes([3; 16]);
        let mut tx = db.begin_write().await?;
        sqlx::query("INSERT INTO users VALUES(?,'backup-owner','test-only','active')")
            .bind(owner.as_bytes().as_slice())
            .execute(&mut *tx)
            .await?;
        vaults::insert_vault(
            &mut tx,
            &NewVault {
                id: vault,
                entry: parent,
                owner,
                absolute_path: base.join("work").display().to_string(),
                volume_identity: vec![1],
                vault_file_identity: vec![2],
                epoch: ServerEpoch::ZERO,
            },
        )
        .await?;
        sqlx::query("INSERT INTO sessions VALUES(?,?,'native-test',?,?,1,100,200,NULL,NULL)")
            .bind([4u8; 16].as_slice())
            .bind(owner.as_bytes().as_slice())
            .bind([5u8; 32].as_slice())
            .bind([6u8; 32].as_slice())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Self {
            db,
            config,
            owner,
            vault,
            parent,
            base,
        })
    }
    async fn object(&self, bytes: &[u8]) -> Result<[u8; 32]> {
        let hash = digest(bytes);
        let path = self.object_path(hash);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, bytes)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400))?;
        fs::File::open(&path)?.sync_all()?;
        let mut tx = self.db.begin_write().await?;
        versions::record_object(
            &mut tx,
            self.owner,
            &NewObject {
                vault: self.vault,
                hash,
                size: bytes.len() as u64,
                storage_path: ObjectLayout::relative_object_path(
                    self.vault,
                    &ContentDigest::from_bytes(hash),
                )
                .display()
                .to_string(),
                verified_at: now(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(hash)
    }
    fn object_path(&self, hash: [u8; 32]) -> PathBuf {
        ObjectLayout::for_vault(self.config.state_dir.join("objects"), self.vault)
            .object_path(&ContentDigest::from_bytes(hash))
    }
    async fn seed(&self) -> Result<([u8; 32], [u8; 32])> {
        let old = self.object(b"confirmed-history").await?;
        let current = self.object(b"confirmed-current").await?;
        let trash = self.object(b"recoverable-trash").await?;
        let entry = EntryId::from_bytes([7; 16]);
        let mut tx = self.db.begin_write().await?;
        entries::insert_entry(
            &mut tx,
            self.owner,
            &NewEntry {
                id: entry,
                vault: self.vault,
                parent: self.parent,
                name: "current.txt".into(),
                name_key: "current.txt".into(),
                kind: EntryKind::File,
                revision: Revision::new(1)?,
                content_version: None,
            },
            now(),
        )
        .await?;
        for (n, hash) in [(8u8, old), (9u8, current)] {
            let batch = changes::append_change(
                &mut tx,
                self.owner,
                self.vault,
                TxId::from_bytes([n; 16]),
                "[{}]",
                now(),
            )
            .await?;
            versions::record_version(
                &mut tx,
                self.owner,
                &NewVersion {
                    id: VersionId::from_bytes([n; 16]),
                    vault: self.vault,
                    entry,
                    object_hash: hash,
                    size: 17,
                    created_seq: batch.seq,
                    origin: VersionOrigin::Client,
                    at: now(),
                },
            )
            .await?;
        }
        // 过期历史仍在DB中时，备份也包含它；GC必须在备份租约期间保留它。
        sqlx::query("UPDATE versions SET superseded_at=? WHERE id=?")
            .bind(now().as_unix_seconds() - 31 * 86400)
            .bind([8u8; 16].as_slice())
            .execute(&mut *tx)
            .await?;
        let deleted = EntryId::from_bytes([10; 16]);
        entries::insert_entry(
            &mut tx,
            self.owner,
            &NewEntry {
                id: deleted,
                vault: self.vault,
                parent: self.parent,
                name: "deleted.txt".into(),
                name_key: "deleted.txt".into(),
                kind: EntryKind::File,
                revision: Revision::new(1)?,
                content_version: None,
            },
            now(),
        )
        .await?;
        versions::record_version(
            &mut tx,
            self.owner,
            &NewVersion {
                id: VersionId::from_bytes([11; 16]),
                vault: self.vault,
                entry: deleted,
                object_hash: trash,
                size: 17,
                created_seq: vaultsyncd_domain::version::VaultSeq::new(2)?,
                origin: VersionOrigin::Client,
                at: now(),
            },
        )
        .await?;
        let deleted_at = now().as_unix_seconds() - 1000;
        let expiry = deleted_at + 30 * 86400;
        sqlx::query("UPDATE entries SET deleted_at=? WHERE id=?")
            .bind(deleted_at)
            .bind(deleted.as_bytes().as_slice())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO trash_entries VALUES(?,?,?,'deleted.txt','file',1,?,?,?)")
            .bind(deleted.as_bytes().as_slice())
            .bind(self.vault.as_bytes().as_slice())
            .bind(self.parent.as_bytes().as_slice())
            .bind([11u8; 16].as_slice())
            .bind(deleted_at)
            .bind(expiry)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO tombstones VALUES(?,?,'deleted.txt','file',?,?)")
            .bind(self.vault.as_bytes().as_slice())
            .bind(deleted.as_bytes().as_slice())
            .bind(deleted_at)
            .bind(expiry)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        fs::write(self.base.join("work/current.txt"), b"confirmed-current")?;
        Ok((old, current))
    }
    async fn cleanup(self, ids: &[BackupLeaseId]) -> Result<()> {
        let arena = Arena::open(&self.config.state_dir.join("backups"))?;
        for id in ids {
            let mut tx = self.db.begin_write().await?;
            sqlx::query("UPDATE backup_preparations SET phase='releasing' WHERE id=?")
                .bind(id.as_bytes().as_slice())
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            arena.remove(&id.to_hex())?;
            let mut tx = self.db.begin_write().await?;
            leases::release_backup_lease(&mut tx, *id).await?;
            sqlx::query("UPDATE backup_preparations SET phase='released' WHERE id=?")
                .bind(id.as_bytes().as_slice())
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        self.db.close().await;
        fs::remove_dir_all(self.base)?;
        Ok(())
    }
}

async fn continuous_write_and_lease(base: &Path) -> Result<()> {
    let lab = Lab::new(base, "consistent").await?;
    let (old, current) = lab.seed().await?;
    let orphan = lab.object(b"unreferenced-but-registered").await?;
    vaultsyncd_server::gc::run_once(
        &lab.db,
        &lab.config.state_dir,
        at(now().as_unix_seconds() - 7200),
    )
    .await?;
    let stopped = Arc::new(AtomicBool::new(false));
    let writes = Arc::new(AtomicU64::new(0));
    let stop = stopped.clone();
    let count = writes.clone();
    let path = lab.base.join("work/current.txt");
    let writer = std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let n = count.fetch_add(1, Ordering::AcqRel);
            fs::write(&path, format!("uncollected-{n}")).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let id = BackupLeaseId::from_bytes([20; 16]);
    let activity = lab.db.object_activity().await;
    let db = lab.db.clone();
    let config = lab.config.clone();
    let owner = lab.owner;
    let mut preparation =
        tokio::spawn(async move { backup::prepare(&db, &config, owner, id, now()).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut preparation)
            .await
            .is_err(),
        "prepare必须等待现有对象动作完成"
    );
    drop(activity);
    let result = preparation.await?;
    stopped.store(true, Ordering::Release);
    writer.join().unwrap();
    let result = result?;
    assert!(writes.load(Ordering::Acquire) > 1);
    assert_eq!(result.manifest["state"], "prepared");
    assert_eq!(result.manifest["scope"], "confirmed_logical_state");
    assert_eq!(result.manifest["vaults"][0]["seq"], "2");
    assert!(fs::write(result.snapshot.join("forbidden"), b"must fail").is_err());
    for o in result.manifest["objects"].as_array().unwrap() {
        let file = fs::File::open(result.snapshot.join(o["archive_path"].as_str().unwrap()))?;
        let (hash, size) = hash_file(&file)?;
        assert_eq!(hex(&hash), o["sha256"].as_str().unwrap());
        assert_eq!(size.to_string(), o["size"].as_str().unwrap());
    }
    for f in result.manifest["files"].as_array().unwrap() {
        let (hash, size) = hash_file(&fs::File::open(
            result.snapshot.join(f["path"].as_str().unwrap()),
        )?)?;
        assert_eq!(hex(&hash), f["sha256"].as_str().unwrap());
        assert_eq!(size.to_string(), f["size"].as_str().unwrap());
    }
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(result.snapshot.join("database.sqlite"))
            .read_only(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete),
    )
    .await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(count, 0);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM versions")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(count, 3);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM trash_entries")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(count, 1);
    assert_eq!(result.manifest["database_contents"]["trash_entries"], "1");
    let seq: i64 = sqlx::query_scalar("SELECT seq FROM vaults")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(seq, 2);
    db.close().await?;
    let report = vaultsyncd_server::gc::run_once(
        &lab.db,
        &lab.config.state_dir,
        at(now().as_unix_seconds() + 7200),
    )
    .await?;
    assert_eq!(report.objects, 0);
    assert!(lab.object_path(old).exists());
    assert!(lab.object_path(orphan).exists());
    assert_eq!(
        fs::read(result.snapshot.join(format!(
            "objects/{}/{}",
            lab.vault.to_hex(),
            hex(&current)
        )))?,
        b"confirmed-current"
    );
    assert_eq!(
        fs::metadata(result.snapshot.join("database.sqlite"))?
            .permissions()
            .mode()
            & 0o077,
        0
    );
    println!(
        "PASS continuous-external-writes={} confirmed-seq=2 WAL+history+session-policy+hashes+readonly+lease-GC",
        writes.load(Ordering::Acquire)
    );
    lab.cleanup(&[id]).await
}

async fn failures_and_nested(base: &Path) -> Result<()> {
    let lab = Lab::new(base, "failure").await?;
    let (old, current) = lab.seed().await?;
    fs::remove_file(lab.object_path(old))?;
    let id = BackupLeaseId::from_bytes([21; 16]);
    assert!(
        backup::prepare(&lab.db, &lab.config, lab.owner, id, now())
            .await
            .is_err()
    );
    let row =
        sqlx::query("SELECT phase,cleaned_at,manifest_hash FROM backup_preparations WHERE id=?")
            .bind(id.as_bytes().as_slice())
            .fetch_one(lab.db.pool())
            .await?;
    assert_eq!(row.try_get::<String, _>("phase")?, "failed");
    assert!(row.try_get::<Option<i64>, _>("cleaned_at")?.is_some());
    assert!(
        row.try_get::<Option<Vec<u8>>, _>("manifest_hash")?
            .is_none()
    );
    let leases: i64 = sqlx::query_scalar("SELECT count(*) FROM backup_leases")
        .fetch_one(lab.db.pool())
        .await?;
    assert_eq!(leases, 0);
    assert!(lab.object_path(current).exists());
    let arena = Arena::open(&lab.config.state_dir.join("backups"))?;
    let nested_id = BackupLeaseId::from_bytes([22; 16]);
    let prepared = arena.create(&nested_id.to_hex())?;
    let nested = prepared.join("nested");
    assert!(
        std::process::Command::new("btrfs")
            .args(["subvolume", "create"])
            .arg(&nested)
            .status()?
            .success()
    );
    assert!(arena.freeze(&nested_id.to_hex()).is_err());
    assert!(!arena.snapshot_path(&nested_id.to_hex())?.exists());
    assert!(arena.remove(&nested_id.to_hex()).is_err());
    assert!(
        std::process::Command::new("btrfs")
            .args(["subvolume", "delete"])
            .arg(&nested)
            .status()?
            .success()
    );
    arena.remove(&nested_id.to_hex())?;
    assert!(arena.remove("../../outside").is_err());
    println!(
        "PASS missing-object never-ready failure-cleanup nested-subvolume+path-escape rejected"
    );
    lab.cleanup(&[]).await
}

async fn crash_child(state: &Path) -> Result<()> {
    let db = Database::open(state.join("server.sqlite")).await?;
    let id = BackupLeaseId::from_bytes([23; 16]);
    let owner = UserId::from_bytes([1; 16]);
    let mut tx = db.begin_write().await?;
    sqlx::query("INSERT INTO store_metadata VALUES('server_id','abababababababababababababababab') ON CONFLICT DO NOTHING").execute(&mut *tx).await?;
    sqlx::query("INSERT INTO backup_preparations(id,owner_id,phase,created_at,expires_at) VALUES(?,?,'preparing',?,?)").bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).bind(now().to_db_integer()).bind(now().as_unix_seconds()+86400).execute(&mut *tx).await?;
    leases::insert_backup_lease(
        &mut tx,
        &NewBackupLease {
            id,
            snapshot_seq_map: SnapshotSeqMap::new(vec![(
                VaultId::from_bytes([2; 16]),
                vaultsyncd_domain::version::VaultSeq::new(2)?,
            )])?,
            expires_at: at(now().as_unix_seconds() + 86400),
            manifest_ref: format!("backups/{}/snapshot/manifest.json", id.to_hex()),
        },
        now(),
    )
    .await?;
    tx.commit().await?;
    let arena = Arena::open(&state.join("backups"))?;
    let prepared = arena.create(&id.to_hex())?;
    vaultsyncd_server_store::backup::online_backup(db.path(), &prepared.join("database.sqlite"))?;
    fs::write(state.join("crash-ready"), b"ready")?;
    loop {
        std::thread::park()
    }
}
async fn interrupted_cleanup(base: &Path) -> Result<()> {
    let lab = Lab::new(base, "crash").await?;
    lab.seed().await?;
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .arg("--child")
        .arg(&lab.config.state_dir)
        .spawn()?;
    for _ in 0..300 {
        if lab.config.state_dir.join("crash-ready").exists() {
            break;
        }
        if child.try_wait()?.is_some() {
            panic!("backup child failed")
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if !lab.config.state_dir.join("crash-ready").exists() {
        child.kill()?;
        child.wait()?;
        panic!("backup crash boundary missing")
    }
    child.kill()?;
    assert!(!child.wait()?.success());
    let reopened = Database::open(&lab.config.database).await?;
    assert_eq!(
        backup::recover_incomplete(&reopened, &lab.config.state_dir, now()).await?,
        1
    );
    assert_eq!(
        backup::recover_incomplete(&reopened, &lab.config.state_dir, now()).await?,
        0
    );
    let phase: String = sqlx::query_scalar("SELECT phase FROM backup_preparations")
        .fetch_one(reopened.pool())
        .await?;
    assert_eq!(phase, "failed");
    let leases: i64 = sqlx::query_scalar("SELECT count(*) FROM backup_leases")
        .fetch_one(reopened.pool())
        .await?;
    assert_eq!(leases, 0);
    reopened.close().await;
    println!(
        "PASS SIGKILL after Online Backup leaves preparing; restart cleans only matching ID and releases lease"
    );
    lab.cleanup(&[]).await
}

async fn unfinished_operation(base: &Path) -> Result<()> {
    let lab = Lab::new(base, "pending").await?;
    let (old, current) = lab.seed().await?;
    let mut tx = lab.db.begin_write().await?;
    sqlx::query("INSERT INTO operations(vault_id,device_id,op_id,request_digest,state) VALUES(?,?,?,?,'pending')")
        .bind(lab.vault.as_bytes().as_slice()).bind([31u8;16].as_slice()).bind([32u8;16].as_slice()).bind([33u8;32].as_slice()).execute(&mut *tx).await?;
    tx.commit().await?;
    assert!(
        backup::prepare(
            &lab.db,
            &lab.config,
            lab.owner,
            BackupLeaseId::from_bytes([24; 16]),
            now()
        )
        .await
        .is_err()
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM backup_preparations WHERE phase='ready'")
            .fetch_one(lab.db.pool())
            .await?;
    assert_eq!(count, 0);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM operations WHERE state='pending'")
        .fetch_one(lab.db.pool())
        .await?;
    assert_eq!(count, 1);
    assert!(lab.object_path(old).exists() && lab.object_path(current).exists());
    println!("PASS unfinished operation is preserved and prevents a false ready snapshot");
    lab.cleanup(&[]).await
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|s| s == "--child") {
        return crash_child(Path::new(args.get(2).ok_or("missing child state")?)).await;
    }
    let base = PathBuf::from(
        args.get(1)
            .ok_or("expected empty run directory inside prepared btrfs test mount")?,
    );
    if base.exists() {
        return Err("test run directory must not exist".into());
    }
    fs::DirBuilder::new().mode(0o700).create(&base)?;
    continuous_write_and_lease(&base).await?;
    failures_and_nested(&base).await?;
    interrupted_cleanup(&base).await?;
    unfinished_operation(&base).await?;
    fs::remove_dir(base)?;
    println!("S087 native prepare acceptance: PASS");
    Ok(())
}
