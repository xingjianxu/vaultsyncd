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
use vaultsyncd_domain::{ids::RootId, version::RootSeq};
use vaultsyncd_server_core::backup::prepare::{FrozenObject, FrozenRoot, Plan};

fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
/// 一致导出时读取的附加元数据；全部来自同一源连接快照。
pub struct FrozenDatabase {
    /// 核心可验证的对象/Root清单。
    pub plan: Plan,
    /// Root原始映射，包含恢复所需身份/epoch，JSON对象列表由应用层编码。
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
    let pending:i64=src.query_row("SELECT (SELECT count(*) FROM fs_intents WHERE phase<>'finalized')+(SELECT count(*) FROM operations WHERE state='pending')+(SELECT count(*) FROM objects WHERE gc_state<>'live')+(SELECT count(*) FROM entries e WHERE e.content_version_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM versions v WHERE v.id=e.content_version_id AND v.root_id=e.root_id AND v.entry_id=e.id))+(SELECT count(*) FROM trash_entries t WHERE t.content_version_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM versions v WHERE v.id=t.content_version_id AND v.root_id=t.root_id AND v.entry_id=t.entry_id))",[],|r|r.get(0)).map_err(err)?;
    if pending != 0 {
        return Err(err("unfinished intents/operations/GC prevent backup"));
    }
    let mut roots = Vec::new();
    let mut mappings = Vec::new();
    let mut query=src.prepare("SELECT lower(hex(id)),seq,absolute_path,lower(hex(owner_id)),epoch,lower(hex(volume_identity)),lower(hex(root_file_identity)) FROM roots ORDER BY id").map_err(err)?;
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
        roots.push(FrozenRoot {
            root: RootId::from_hex(&id).map_err(err)?,
            seq: RootSeq::from_db_integer(seq).map_err(err)?,
        });
        mappings.push((id, path, owner, epoch.to_string(), volume, identity));
    }
    let mut objects = Vec::new();
    let mut query = src
        .prepare(
            "SELECT lower(hex(root_id)),hash,size,storage_path FROM objects ORDER BY root_id,hash",
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
        let (root, hash, size, storage_path) = row.map_err(err)?;
        objects.push(FrozenObject {
            root: RootId::from_hex(&root).map_err(err)?,
            hash: hash.try_into().map_err(|_| err("invalid object hash"))?,
            size: u64::try_from(size).map_err(err)?,
            storage_path,
        });
    }
    let plan = Plan { roots, objects };
    plan.validate().map_err(|e| err(format!("{e:?}")))?;
    let mut table_counts = std::collections::BTreeMap::new();
    for table in [
        "users",
        "roots",
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
    dst.execute_batch("PRAGMA foreign_keys=ON; BEGIN; UPDATE sessions SET replaced_by=NULL; DELETE FROM sessions; DELETE FROM snapshots; DELETE FROM upload_parts; DELETE FROM uploads; DELETE FROM object_leases; DELETE FROM backup_leases; DELETE FROM backup_preparations; DELETE FROM gc_candidates; DELETE FROM version_lease_requests; COMMIT; PRAGMA journal_mode=DELETE;").map_err(err)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    #[tokio::test]
    async fn online_backup_contains_committed_wal_and_discards_live_sessions() {
        let dir = TestDir::new("s087-online");
        let db = open_database(&dir).await;
        let owner = seed_owner(&db, 1).await;
        let (root, _) = register_root(&db, owner, 2).await;
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
            root,
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
        assert_eq!(frozen.plan.roots[0].seq.get(), 1);
        let copied = Connection::open(&target).unwrap();
        assert_eq!(
            copied
                .query_row("SELECT count(*) FROM sessions", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            copied
                .query_row("SELECT seq FROM roots", [], |r| r.get::<_, i64>(0))
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
