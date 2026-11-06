//! GC 标记、重新核对与不可逆删除授权；只做短事务，不执行文件 I/O。
use super::Error;
use sqlx::{Row, Sqlite, Transaction};
use vaultsyncd_domain::{
    ids::{RootId, UserId},
    version::UtcTimestamp,
};
use vaultsyncd_server_core::gc::{
    mark::{ObjectKey, mark},
    sweep::{Decision, decide},
};

/// 已持久化的候选；路径必须在文件适配层再次验证，不信任数据库中的任意文本。
#[derive(Clone, Debug)]
pub struct Candidate {
    /// Root 内内容地址。
    pub object: ObjectKey,
    /// 对象区规范相对路径。
    pub storage_path: String,
    /// 首次无引用标记时间。
    pub marked_at: UtcTimestamp,
    /// 已授权删除，重启只能继续同一内容地址的删除。
    pub deleting: bool,
}

/// 只新增无引用候选，已有引用撤销 marked 候选；deleting 不得复活。
pub async fn mark_root(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    root: RootId,
    now: UtcTimestamp,
) -> Result<(), Error> {
    let protected = mark(now, super::references(tx, owner, root, now).await?);
    let objects: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT hash FROM objects WHERE root_id=? AND gc_state='live'")
            .bind(root.as_bytes().as_slice())
            .fetch_all(&mut **tx)
            .await?;
    for bytes in objects {
        let hash = super::hash(bytes)?;
        if protected.contains_key(&ObjectKey { root, hash }) {
            sqlx::query(
                "DELETE FROM gc_candidates WHERE root_id=? AND object_hash=? AND phase='marked'",
            )
            .bind(root.as_bytes().as_slice())
            .bind(hash.as_slice())
            .execute(&mut **tx)
            .await?;
        } else {
            sqlx::query("INSERT INTO gc_candidates(root_id,object_hash,marked_at,phase) VALUES(?,?,?,'marked') ON CONFLICT DO NOTHING")
                .bind(root.as_bytes().as_slice()).bind(hash.as_slice()).bind(now.to_db_integer()).execute(&mut **tx).await?;
        }
    }
    Ok(())
}

/// 每轮最多处理 100 个候选；deleting 优先，保证崩溃后继续。
pub async fn candidates(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    root: RootId,
) -> Result<Vec<Candidate>, Error> {
    let rows=sqlx::query("SELECT o.hash,o.storage_path,c.marked_at,c.phase FROM gc_candidates c JOIN objects o ON o.root_id=c.root_id AND o.hash=c.object_hash JOIN roots r ON r.id=o.root_id WHERE r.id=? AND r.owner_id=? ORDER BY c.phase,c.marked_at,o.hash LIMIT 100")
        .bind(root.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).fetch_all(&mut **tx).await?;
    rows.into_iter()
        .map(|r| {
            Ok(Candidate {
                object: ObjectKey {
                    root,
                    hash: super::hash(r.try_get("hash")?)?,
                },
                storage_path: r.try_get("storage_path")?,
                marked_at: super::time(r.try_get("marked_at")?)?,
                deleting: r.try_get::<String, _>("phase")? == "deleting",
            })
        })
        .collect()
}

/// 必须在独占对象门下调用，并在本事务提交后才允许 unlink。引用复核与 collectable 同事务。
pub async fn authorize_delete(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    candidate: &Candidate,
    now: UtcTimestamp,
) -> Result<bool, Error> {
    let key = candidate.object;
    let phase:Option<(String,i64)>=sqlx::query_as("SELECT c.phase,c.marked_at FROM gc_candidates c JOIN roots r ON r.id=c.root_id WHERE c.root_id=? AND c.object_hash=? AND r.owner_id=?")
        .bind(key.root.as_bytes().as_slice()).bind(key.hash.as_slice()).bind(owner.as_bytes().as_slice()).fetch_optional(&mut **tx).await?;
    let Some((phase, marked_at)) = phase else {
        return Ok(false);
    };
    if phase == "deleting" {
        return Ok(true);
    }
    let protected = mark(now, super::references(tx, owner, key.root, now).await?);
    match decide(super::time(marked_at)?, now, protected.contains_key(&key)) {
        Decision::Keep => {
            sqlx::query("DELETE FROM gc_candidates WHERE root_id=? AND object_hash=?")
                .bind(key.root.as_bytes().as_slice())
                .bind(key.hash.as_slice())
                .execute(&mut **tx)
                .await?;
            return Ok(false);
        }
        Decision::Wait => return Ok(false),
        Decision::Delete => {}
    }
    // references 已验证所有版本/回收站/租约均过期。先移除过期元数据，再封闭新引用。
    sqlx::query("DELETE FROM object_leases WHERE root_id=? AND object_hash=? AND expires_at<=?")
        .bind(key.root.as_bytes().as_slice())
        .bind(key.hash.as_slice())
        .bind(now.to_db_integer())
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM trash_entries WHERE root_id=? AND content_version_id IN (SELECT id FROM versions WHERE root_id=? AND object_hash=?)")
        .bind(key.root.as_bytes().as_slice()).bind(key.root.as_bytes().as_slice()).bind(key.hash.as_slice()).execute(&mut **tx).await?;
    sqlx::query("UPDATE entries SET content_version_id=NULL WHERE root_id=? AND deleted_at IS NOT NULL AND content_version_id IN (SELECT id FROM versions WHERE root_id=? AND object_hash=?)")
        .bind(key.root.as_bytes().as_slice()).bind(key.root.as_bytes().as_slice()).bind(key.hash.as_slice()).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM versions WHERE root_id=? AND object_hash=?")
        .bind(key.root.as_bytes().as_slice())
        .bind(key.hash.as_slice())
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE objects SET gc_state='collectable' WHERE root_id=? AND hash=?")
        .bind(key.root.as_bytes().as_slice())
        .bind(key.hash.as_slice())
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE gc_candidates SET phase='deleting' WHERE root_id=? AND object_hash=?")
        .bind(key.root.as_bytes().as_slice())
        .bind(key.hash.as_slice())
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

/// 文件 unlink 和父目录 fsync 成功后收尾；事务失败时 deleting 证据保留，可重复。
pub async fn finish_delete(
    tx: &mut Transaction<'_, Sqlite>,
    candidate: &Candidate,
) -> Result<(), Error> {
    sqlx::query("DELETE FROM objects WHERE root_id=? AND hash=? AND gc_state='collectable' AND EXISTS(SELECT 1 FROM gc_candidates c WHERE c.root_id=objects.root_id AND c.object_hash=objects.hash AND c.phase='deleting')")
        .bind(candidate.object.root.as_bytes().as_slice()).bind(candidate.object.hash.as_slice()).execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        leases::{self, NewObjectLease, ObjectLeasePurpose},
        test_support::*,
    };
    use vaultsyncd_server_core::gc::sweep::GRACE_SECONDS;

    #[tokio::test]
    async fn a_new_lease_between_mark_and_recheck_cancels_the_candidate() {
        let dir = TestDir::new("s086-recheck");
        let db = open_database(&dir).await;
        let owner = seed_owner(&db, 1).await;
        let (root, _) = register_root(&db, owner, 2).await;
        let hash = seed_object(&db, owner, root, 3, 1).await;
        let mut tx = db.begin_write().await.unwrap();
        mark_root(&mut tx, owner, root, at(100)).await.unwrap();
        let c = candidates(&mut tx, owner, root).await.unwrap().remove(0);
        assert!(!authorize_delete(&mut tx, owner, &c, at(100)).await.unwrap());
        tx.commit().await.unwrap();
        let mut tx = db.begin_write().await.unwrap();
        leases::lease_object(
            &mut tx,
            owner,
            &NewObjectLease {
                id: object_lease(4),
                root,
                object_hash: hash,
                purpose: ObjectLeasePurpose::Commit,
                expires_at: at(100 + GRACE_SECONDS + 50),
            },
            at(101),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let mut tx = db.begin_write().await.unwrap();
        assert!(
            !authorize_delete(&mut tx, owner, &c, at(100 + GRACE_SECONDS))
                .await
                .unwrap()
        );
        assert!(candidates(&mut tx, owner, root).await.unwrap().is_empty());
        assert!(
            crate::versions::object_in_scope(&mut tx, owner, root, &hash)
                .await
                .unwrap()
                .is_some()
        );
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn deleting_is_durable_and_rejects_new_references() {
        let dir = TestDir::new("s086-delete");
        let db = open_database(&dir).await;
        let owner = seed_owner(&db, 1).await;
        let (root, _) = register_root(&db, owner, 2).await;
        let hash = seed_object(&db, owner, root, 3, 1).await;
        let mut tx = db.begin_write().await.unwrap();
        mark_root(&mut tx, owner, root, at(100)).await.unwrap();
        let c = candidates(&mut tx, owner, root).await.unwrap().remove(0);
        assert!(
            authorize_delete(&mut tx, owner, &c, at(100 + GRACE_SECONDS))
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
        db.close().await;
        let db = open_database(&dir).await;
        let mut tx = db.begin_write().await.unwrap();
        let c = candidates(&mut tx, owner, root).await.unwrap().remove(0);
        assert!(c.deleting);
        assert!(
            leases::lease_object(
                &mut tx,
                owner,
                &NewObjectLease {
                    id: object_lease(4),
                    root,
                    object_hash: hash,
                    purpose: ObjectLeasePurpose::Download,
                    expires_at: at(10000)
                },
                at(4000)
            )
            .await
            .is_err()
        );
        assert!(
            crate::versions::record_object(
                &mut tx,
                owner,
                &crate::versions::NewObject {
                    root,
                    hash,
                    size: 1,
                    storage_path: "/srv/objects/03".into(),
                    verified_at: at(4000)
                }
            )
            .await
            .is_err()
        );
        assert!(
            sqlx::query("UPDATE objects SET gc_state='live' WHERE root_id=?")
                .bind(root.as_bytes().as_slice())
                .execute(&mut *tx)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("INSERT INTO backup_leases VALUES(?,?,10000,'manifest')")
                .bind([9u8; 16].as_slice())
                .bind(format!("{{\"{}\":0}}", root.to_hex()))
                .execute(&mut *tx)
                .await
                .is_err()
        );
        assert!(
            authorize_delete(&mut tx, owner, &c, at(4000))
                .await
                .unwrap()
        );
        finish_delete(&mut tx, &c).await.unwrap();
        assert!(
            crate::versions::object_in_scope(&mut tx, owner, root, &hash)
                .await
                .unwrap()
                .is_none()
        );
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn commit_after_mark_protects_old_content_while_expired_history_is_pruned() {
        use crate::{
            changes,
            versions::{self, NewVersion, VersionOrigin},
        };
        use vaultsyncd_domain::ids::VersionId;
        let dir = TestDir::new("s086-commit");
        let db = open_database(&dir).await;
        let owner = seed_owner(&db, 1).await;
        let (root, parent) = register_root(&db, owner, 2).await;
        let entry = seed_file(&db, owner, root, parent, 3, "file").await;
        let old = seed_object(&db, owner, root, 4, 1).await;
        let current = seed_object(&db, owner, root, 5, 1).await;
        let mut tx = db.begin_write().await.unwrap();
        mark_root(&mut tx, owner, root, at(100)).await.unwrap();
        let c = candidates(&mut tx, owner, root)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.object.hash == old)
            .unwrap();
        let batch = changes::append_change(
            &mut tx,
            owner,
            root,
            crate::test_support::tx(8),
            "[{}]",
            at(101),
        )
        .await
        .unwrap();
        versions::record_version(
            &mut tx,
            owner,
            &NewVersion {
                id: VersionId::from_bytes([9; 16]),
                root,
                entry,
                object_hash: old,
                size: 1,
                created_seq: batch.seq,
                origin: VersionOrigin::Client,
                at: at(101),
            },
        )
        .await
        .unwrap();
        assert!(
            !authorize_delete(&mut tx, owner, &c, at(100 + GRACE_SECONDS))
                .await
                .unwrap()
        );
        versions::record_version(
            &mut tx,
            owner,
            &NewVersion {
                id: VersionId::from_bytes([10; 16]),
                root,
                entry,
                object_hash: current,
                size: 1,
                created_seq: batch.seq,
                origin: VersionOrigin::Client,
                at: at(200),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let after_retention = 200 + vaultsyncd_server_core::gc::mark::RETENTION_SECONDS;
        let mut tx = db.begin_write().await.unwrap();
        mark_root(&mut tx, owner, root, at(after_retention))
            .await
            .unwrap();
        let cs = candidates(&mut tx, owner, root).await.unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].object.hash, old);
        assert!(
            authorize_delete(&mut tx, owner, &cs[0], at(after_retention + GRACE_SECONDS))
                .await
                .unwrap()
        );
        finish_delete(&mut tx, &cs[0]).await.unwrap();
        assert_eq!(
            versions::current_version(&mut tx, owner, root, entry)
                .await
                .unwrap()
                .unwrap()
                .object_hash,
            current
        );
        assert_eq!(
            versions::version_history(&mut tx, owner, root, entry, 100)
                .await
                .unwrap()
                .len(),
            1
        );
        tx.commit().await.unwrap();
    }
}
