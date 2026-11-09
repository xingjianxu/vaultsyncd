//! SQLite Online Backup API：固定读取快照，包含WAL，不复制活动数据库裸文件。
use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};
use std::{
    fs, io,
    path::Path,
    time::{Duration, Instant},
};
use vaultsyncd_domain::{ids::VaultId, version::VaultSeq};
use vaultsyncd_server_core::backup::prepare::{FrozenObject, FrozenVault, Plan};

fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
/// 一致导出时读取的附加元数据；全部来自同一源连接快照。
pub struct FrozenDatabase {
    /// 核心可验证的对象/Vault清单。
    pub plan: Plan,
    /// Vault原始映射，包含恢复所需身份/epoch，JSON对象列表由应用层编码。
    pub mappings: Vec<(String, String, String, String, String, String)>,
    /// ServerId，必须由应用层在屏障内先行持久化。
    pub server_id: String,
    /// 源schema版本。
    pub schema: i64,
    /// 导出数据库中的逻辑表及行数，使用十进制文本避免浮点损失。
    pub table_counts: std::collections::BTreeMap<String, String>,
}
/// 源只读事务固定快照；目标必须不存在且以0600独占创建。目标会话/续传/临时租约失效，保留逻辑历史。
/// 调用方需在对象独占门下执行，并将阻塞工作放入专用阻塞线程。
pub fn online_backup(source: &Path, destination: &Path) -> io::Result<FrozenDatabase> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(destination)?.sync_all()?;
    let src = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(err)?;
    src.busy_timeout(Duration::from_secs(10)).map_err(err)?;
    src.execute_batch("BEGIN").map_err(err)?;
    let server_id: String = src
        .query_row(
            "SELECT value FROM store_metadata WHERE key='server_id'",
            [],
            |r| r.get(0),
        )
        .map_err(err)?;
    let schema: i64 = src
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(err)?;
    let pending:i64=src.query_row("SELECT (SELECT count(*) FROM fs_intents WHERE phase<>'finalized')+(SELECT count(*) FROM operations WHERE state='pending')+(SELECT count(*) FROM objects WHERE gc_state<>'live')+(SELECT count(*) FROM entries e WHERE e.content_version_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM versions v WHERE v.id=e.content_version_id AND v.vault_id=e.vault_id AND v.entry_id=e.id))+(SELECT count(*) FROM trash_entries t WHERE t.content_version_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM versions v WHERE v.id=t.content_version_id AND v.vault_id=t.vault_id AND v.entry_id=t.entry_id))",[],|r|r.get(0)).map_err(err)?;
    if pending != 0 {
        return Err(err("unfinished intents/operations/GC prevent backup"));
    }
    let mut vaults = Vec::new();
    let mut mappings = Vec::new();
    let mut query=src.prepare("SELECT lower(hex(id)),seq,absolute_path,lower(hex(owner_id)),epoch,lower(hex(volume_identity)),lower(hex(vault_file_identity)) FROM vaults ORDER BY id").map_err(err)?;
    for row in query
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })
        .map_err(err)?
    {
        let (id, seq, path, owner, epoch, volume, identity) = row.map_err(err)?;
        vaults.push(FrozenVault {
            vault: VaultId::from_hex(&id).map_err(err)?,
            seq: VaultSeq::from_db_integer(seq).map_err(err)?,
        });
        mappings.push((id, path, owner, epoch.to_string(), volume, identity));
    }
    let mut objects = Vec::new();
    let mut query = src
        .prepare(
            "SELECT lower(hex(vault_id)),hash,size,storage_path FROM objects ORDER BY vault_id,hash",
        )
        .map_err(err)?;
    for row in query
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(err)?
    {
        let (vault, hash, size, storage_path) = row.map_err(err)?;
        objects.push(FrozenObject {
            vault: VaultId::from_hex(&vault).map_err(err)?,
            hash: hash.try_into().map_err(|_| err("invalid object hash"))?,
            size: u64::try_from(size).map_err(err)?,
            storage_path,
        });
    }
    let plan = Plan { vaults, objects };
    plan.validate().map_err(|e| err(format!("{e:?}")))?;
    let mut table_counts = std::collections::BTreeMap::new();
    for table in [
        "users",
        "vaults",
        "entries",
        "versions",
        "trash_entries",
        "tombstones",
        "changes",
        "operations",
    ] {
        let count: i64 = src
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .map_err(err)?;
        table_counts.insert(table.to_owned(), count.to_string());
    }
    let mut dst = Connection::open(destination).map_err(err)?;
    {
        let backup = Backup::new(&src, &mut dst).map_err(err)?;
        let started = Instant::now();
        loop {
            match backup.step(256).map_err(err)? {
                StepResult::Done => break,
                StepResult::More => {}
                StepResult::Busy | StepResult::Locked => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => return Err(err("unknown SQLite backup state")),
            }
            if started.elapsed() > Duration::from_secs(60) {
                return Err(err("SQLite backup deadline exceeded"));
            }
        }
    }
    src.execute_batch("ROLLBACK").map_err(err)?;
    dst.execute_batch("PRAGMA foreign_keys=ON; BEGIN; UPDATE sessions SET replaced_by=NULL; DELETE FROM sessions; DELETE FROM snapshots; DELETE FROM upload_parts; DELETE FROM uploads; DELETE FROM object_leases; DELETE FROM backup_leases; DELETE FROM backup_jobs; DELETE FROM backup_preparations; DELETE FROM gc_candidates; DELETE FROM version_lease_requests; COMMIT; PRAGMA journal_mode=DELETE;").map_err(err)?;
    let check: String = dst
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(err)?;
    if check != "ok" {
        return Err(err("backup database quick_check failed"));
    }
    let fk: Option<i64> = dst
        .prepare("PRAGMA foreign_key_check")
        .map_err(err)?
        .query([])
        .map_err(err)?
        .next()
        .map_err(err)?
        .map(|_| 1);
    if fk.is_some() {
        return Err(err("backup foreign_key_check failed"));
    }
    drop(dst);
    fs::File::open(destination)?.sync_all()?;
    Ok(FrozenDatabase {
        plan,
        mappings,
        server_id,
        schema,
        table_counts,
    })
}

/// 对导出副本进行只读逻辑核对；不打开在线库，不迁移/写入归档数据库。
pub fn verify_database(
    path: &Path,
    index: &vaultsyncd_server_core::backup::export::Index,
) -> io::Result<()> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(err)?;
    let quick: String = db
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(err)?;
    let app: i32 = db
        .query_row("PRAGMA application_id", [], |r| r.get(0))
        .map_err(err)?;
    let schema: i64 = db
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(err)?;
    if quick != "ok"
        || app != crate::db::SERVER_APPLICATION_ID
        || schema != index.schema_version
        || schema > crate::db::target_schema_version()
    {
        return Err(err("backup database identity/schema/quick_check mismatch"));
    }
    let server: String = db
        .query_row(
            "SELECT value FROM store_metadata WHERE key='server_id'",
            [],
            |r| r.get(0),
        )
        .map_err(err)?;
    if vaultsyncd_domain::ids::ServerId::from_hex(&server).map_err(err)? != index.server {
        return Err(err("backup ServerId mismatch"));
    }
    let sessions: i64 = db
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .map_err(err)?;
    if sessions != 0 {
        return Err(err("backup still contains usable sessions"));
    }
    let mut q = db.prepare("PRAGMA foreign_key_check").map_err(err)?;
    if q.query([]).map_err(err)?.next().map_err(err)?.is_some() {
        return Err(err("backup foreign key failure"));
    }
    for (table, expected) in &index.table_counts {
        if ![
            "users",
            "vaults",
            "entries",
            "versions",
            "trash_entries",
            "tombstones",
            "changes",
            "operations",
        ]
        .contains(&table.as_str())
        {
            return Err(err("unrecognized metadata table"));
        }
        let count: i64 = db
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .map_err(err)?;
        if count as u64 != *expected {
            return Err(err("backup logical table count mismatch"));
        }
    }
    let mut vaults = std::collections::BTreeMap::new();
    let mut q = db
        .prepare("SELECT lower(hex(id)),seq FROM vaults")
        .map_err(err)?;
    for r in q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(err)?
    {
        let (id, seq) = r.map_err(err)?;
        vaults.insert(
            VaultId::from_hex(&id).map_err(err)?,
            u64::try_from(seq).map_err(err)?,
        );
    }
    if vaults != index.vault_seqs {
        return Err(err("backup Vault sequence map mismatch"));
    }
    let mut objects = 0;
    let mut storage_paths = std::collections::BTreeSet::new();
    let mut q = db
        .prepare("SELECT lower(hex(vault_id)),hash,size,storage_path FROM objects")
        .map_err(err)?;
    for r in q
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(err)?
    {
        let (vault, hash, size, path) = r.map_err(err)?;
        let hash: [u8; 32] = hash
            .try_into()
            .map_err(|_| err("invalid backup object hash"))?;
        let vault = VaultId::from_hex(&vault).map_err(err)?;
        let digest = vaultsyncd_server_core::objects::ContentDigest::from_bytes(hash);
        let archive = format!("objects/{}/{}", vault.to_hex(), digest.to_hex());
        let item = index
            .item(&archive)
            .ok_or_else(|| err("database object missing from manifest"))?;
        if item.hash != hash
            || item.size != u64::try_from(size).map_err(err)?
            || !index.vault_seqs.contains_key(&vault)
            || !vaultsyncd_server_core::objects::ObjectLayout::valid_storage_path(
                vault, &digest, &path,
            )
            || index.object_storage_path(&archive) != Some(path.as_str())
            || !storage_paths.insert(path)
        {
            return Err(err("backup object metadata mismatch"));
        }
        objects += 1;
    }
    if objects + 2 != index.items.len() || objects != index.object_storage_paths.len() {
        return Err(err("manifest objects missing from database"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    #[tokio::test]
    async fn scoped_same_digest_backup_preserves_exact_database_paths_and_rejects_foreign_layouts()
    {
        use std::collections::BTreeMap;
        use vaultsyncd_domain::ids::{BackupLeaseId, ServerId};
        use vaultsyncd_server_core::{
            backup::export::{Index, Item},
            objects::{ContentDigest, ObjectLayout},
        };
        for vault_count in [1, 3] {
            let dir = TestDir::new(&format!("s136-backup-{vault_count}"));
            let db = open_database(&dir).await;
            let owner = seed_owner(&db, 1).await;
            sqlx::query("INSERT INTO store_metadata VALUES('server_id',?)")
                .bind("abababababababababababababababab")
                .execute(db.pool())
                .await
                .unwrap();
            let digest = ContentDigest::from_bytes([0xab; 32]);
            for n in 0..vault_count {
                let (vault, _) = register_vault(&db, owner, 2 + n).await;
                let storage_path = ObjectLayout::relative_object_path(vault, &digest);
                let mut tx = db.begin_write().await.unwrap();
                crate::versions::record_object(
                    &mut tx,
                    owner,
                    &crate::versions::NewObject {
                        vault,
                        hash: *digest.as_bytes(),
                        size: 7,
                        storage_path: storage_path.to_str().unwrap().to_owned(),
                        verified_at: at(1),
                    },
                )
                .await
                .unwrap();
                tx.commit().await.unwrap();
            }
            let target = dir.path.join("backup.sqlite");
            let frozen = online_backup(db.path(), &target).unwrap();
            assert_eq!(frozen.plan.objects.len(), vault_count as usize);
            let mut index = Index {
                id: BackupLeaseId::from_bytes([4; 16]),
                server: ServerId::from_hex(&frozen.server_id).unwrap(),
                manifest_hash: [0; 32],
                items: ["database.sqlite", "config.json"]
                    .map(|path| Item {
                        path: path.into(),
                        hash: [0; 32],
                        size: 0,
                    })
                    .to_vec(),
                vault_seqs: frozen
                    .plan
                    .vaults
                    .iter()
                    .map(|r| (r.vault, r.seq.get()))
                    .collect(),
                schema_version: frozen.schema,
                table_counts: frozen
                    .table_counts
                    .iter()
                    .map(|(k, v)| (k.clone(), v.parse().unwrap()))
                    .collect(),
                object_storage_paths: BTreeMap::new(),
            };
            for object in &frozen.plan.objects {
                let path = format!("objects/{}/{}", object.vault.to_hex(), digest.to_hex());
                index
                    .object_storage_paths
                    .insert(path.clone(), object.storage_path.clone());
                index.items.push(Item {
                    path,
                    hash: object.hash,
                    size: object.size,
                });
            }
            verify_database(&target, &index).unwrap();
            // Restore keeps exactly the same object metadata.
            let restore_plan = crate::restore::plan(&target, &index).unwrap();
            assert!(restore_plan.work.is_empty());
            let mappings: Vec<_> = frozen
                .plan
                .vaults
                .iter()
                .map(|r| crate::restore::Remap {
                    vault: r.vault,
                    path: format!("/restored/{}", r.vault.to_hex()),
                    volume: b"volume".to_vec(),
                    identity: b"identity".to_vec(),
                })
                .collect();
            crate::restore::apply(&target, &index, &mappings, 42).unwrap();
            verify_database(&target, &index).unwrap();
            let frozen_object = &frozen.plan.objects[0];
            let archive = format!(
                "objects/{}/{}",
                frozen_object.vault.to_hex(),
                digest.to_hex()
            );
            // Even an otherwise valid alternate layout may not silently differ from the manifest.
            index.object_storage_paths.insert(
                archive.clone(),
                ObjectLayout::relative_object_path(VaultId::from_bytes([8; 16]), &digest)
                    .to_str()
                    .unwrap()
                    .to_owned(),
            );
            assert!(verify_database(&target, &index).is_err());
            index
                .object_storage_paths
                .insert(archive, frozen_object.storage_path.clone());
            // 已登记路径不可原地重写（即使同一份字节）；这是必须保留的不可变约束。
            let copied = Connection::open(&target).unwrap();
            let invalid = format!("./{}", frozen_object.storage_path);
            assert!(
                copied
                    .execute(
                        "UPDATE objects SET storage_path=? WHERE vault_id=?",
                        rusqlite::params![invalid, frozen_object.vault.as_bytes().as_slice()]
                    )
                    .is_err()
            );
            drop(copied);
            // 外来/损坏备份若已含非规范路径，验证必须拒绝；INSERT不受不可变触发器限制。
            let forged = dir.path.join(format!("forged-{vault_count}.sqlite"));
            std::fs::copy(&target, &forged).unwrap();
            let forged_db = Connection::open(&forged).unwrap();
            let forged_digest = ContentDigest::from_bytes([0xcd; 32]);
            let forged_path = format!(
                "./{}",
                ObjectLayout::relative_object_path(frozen_object.vault, &forged_digest).display()
            );
            forged_db.execute("INSERT INTO objects(vault_id,hash,size,storage_path,verified_at,gc_state) VALUES(?1,?2,7,?3,1,'live')",
                rusqlite::params![frozen_object.vault.as_bytes().as_slice(), forged_digest.as_bytes().as_slice(), forged_path]).unwrap();
            let forged_archive = format!(
                "objects/{}/{}",
                frozen_object.vault.to_hex(),
                forged_digest.to_hex()
            );
            index
                .object_storage_paths
                .insert(forged_archive.clone(), forged_path);
            index.items.push(Item {
                path: forged_archive,
                hash: *forged_digest.as_bytes(),
                size: 7,
            });
            assert!(verify_database(&forged, &index).is_err());
            index.object_storage_paths.remove(&format!(
                "objects/{}/{}",
                frozen_object.vault.to_hex(),
                forged_digest.to_hex()
            ));
            // 另一Vault前缀的物理位置不得冒充本Vault对象；在线备份同样必须拒绝。
            let mut tx = db.begin_write().await.unwrap();
            sqlx::query("INSERT INTO objects(vault_id,hash,size,storage_path,verified_at,gc_state) VALUES(?1,?2,7,?3,1,'live')")
                .bind(frozen_object.vault.as_bytes().as_slice()).bind(forged_digest.as_bytes().as_slice())
                .bind(ObjectLayout::relative_object_path(VaultId::from_bytes([9;16]), &forged_digest).to_str().unwrap())
                .execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            assert!(online_backup(db.path(), &dir.path.join("foreign.sqlite")).is_err());
            db.close().await;
        }
    }
    #[tokio::test]
    async fn online_backup_contains_committed_wal_and_discards_live_sessions() {
        let dir = TestDir::new("s087-online");
        let db = open_database(&dir).await;
        let owner = seed_owner(&db, 1).await;
        let (vault, _) = register_vault(&db, owner, 2).await;
        let mut tx = db.begin_write().await.unwrap();
        sqlx::query("INSERT INTO store_metadata VALUES('server_id',?)")
            .bind("abababababababababababababababab")
            .execute(&mut *tx)
            .await
            .unwrap();
        for marker in [4u8, 5] {
            sqlx::query("INSERT INTO sessions VALUES(?,?,'test',?,?,1,100,200,NULL,NULL)")
                .bind([marker; 16].as_slice())
                .bind(owner.as_bytes().as_slice())
                .bind([marker; 32].as_slice())
                .bind([marker + 1; 32].as_slice())
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE sessions SET replaced_by=? WHERE id=?")
            .bind([5u8; 16].as_slice())
            .bind([4u8; 16].as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        crate::changes::append_change(
            &mut tx,
            owner,
            vault,
            crate::test_support::tx(6),
            "[{}]",
            at(10),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert!(
            fs::metadata(format!("{}-wal", db.path().display()))
                .unwrap()
                .len()
                > 0
        );
        let target = dir.path.join("backup.sqlite");
        let frozen = online_backup(db.path(), &target).unwrap();
        assert_eq!(frozen.plan.vaults[0].seq.get(), 1);
        let copied = Connection::open(&target).unwrap();
        assert_eq!(
            copied
                .query_row("SELECT count(*) FROM sessions", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            copied
                .query_row("SELECT seq FROM vaults", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let original: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(original, 2);
        assert!(
            online_backup(db.path(), &target).is_err(),
            "不得覆盖既有备份"
        );
    }
}
