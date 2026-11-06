//! S085：从调用方事务的一致快照枚举引用，不修改对象状态、不删除任何行或文件。
use sqlx::{Row, Sqlite, Transaction};
use vaultsyncd_domain::{
    ids::{RootId, UserId},
    version::UtcTimestamp,
};
use vaultsyncd_server_core::gc::mark::{ObjectKey, Reason, Reference, retained_until};

pub mod sweep;

/// 引用枚举失败，失败时不得开始回收。
#[derive(Debug)]
pub enum Error {
    /// 数据库读取失败。
    Storage(sqlx::Error),
    /// 备份租约事实无效。
    Lease(crate::leases::LeaseError),
    /// Root 不属于调用者。
    OutOfScope,
    /// 数据库事实不符合引用契约。
    InvalidFact,
}
impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Storage(e)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
fn time(t: i64) -> Result<UtcTimestamp, Error> {
    UtcTimestamp::from_db_integer(t).map_err(|_| Error::InvalidFact)
}
fn hash(bytes: Vec<u8>) -> Result<[u8; 32], Error> {
    bytes.try_into().map_err(|_| Error::InvalidFact)
}

/// Root/owner scope 必须匹配。调用方必须保留事务直到完成本次枚举；结果只供标记，
/// 不允许将它跨事务缓存后直接授权删除。租约统一按 expires_at > now 有效。
pub async fn references(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    root: RootId,
    now: UtcTimestamp,
) -> Result<Vec<Reference>, Error> {
    let authorized: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM roots WHERE id=? AND owner_id=?")
            .bind(root.as_bytes().as_slice())
            .bind(owner.as_bytes().as_slice())
            .fetch_optional(&mut **tx)
            .await?;
    if authorized.is_none() {
        return Err(Error::OutOfScope);
    }
    let dangling: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM entries e WHERE e.root_id=? AND e.content_version_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM versions v WHERE v.id=e.content_version_id AND v.root_id=e.root_id AND v.entry_id=e.id)) OR EXISTS(SELECT 1 FROM trash_entries t WHERE t.root_id=? AND t.content_version_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM versions v WHERE v.id=t.content_version_id AND v.root_id=t.root_id AND v.entry_id=t.entry_id))")
        .bind(root.as_bytes().as_slice()).bind(root.as_bytes().as_slice()).fetch_one(&mut **tx).await?;
    if dangling {
        return Err(Error::InvalidFact);
    }
    let mut refs = Vec::new();
    let rows = sqlx::query("SELECT v.object_hash,v.superseded_at,e.deleted_at,e.content_version_id=v.id AS current_pointer FROM versions v JOIN entries e ON e.root_id=v.root_id AND e.id=v.entry_id WHERE v.root_id=? ORDER BY v.id")
        .bind(root.as_bytes().as_slice()).fetch_all(&mut **tx).await?;
    for row in rows {
        let object = ObjectKey {
            root,
            hash: hash(row.try_get("object_hash")?)?,
        };
        let superseded: Option<i64> = row.try_get("superseded_at")?;
        let deleted: Option<i64> = row.try_get("deleted_at")?;
        let pointer: Option<bool> = row.try_get("current_pointer")?;
        // 两份当前证据不一致也保守保护，绝不凭其中一份猜测可以删除。
        if deleted.is_none() && (superseded.is_none() || pointer == Some(true)) {
            refs.push(Reference {
                object,
                reason: Reason::Current,
                until: None,
            });
        }
        if let Some(start) = superseded.or(deleted) {
            refs.push(Reference {
                object,
                reason: Reason::History,
                until: Some(retained_until(time(start)?)),
            });
        }
    }
    let rows = sqlx::query("SELECT v.object_hash,t.deleted_at,t.expires_at FROM trash_entries t JOIN versions v ON v.id=t.content_version_id AND v.root_id=t.root_id WHERE t.root_id=? ORDER BY t.entry_id")
        .bind(root.as_bytes().as_slice()).fetch_all(&mut **tx).await?;
    for row in rows {
        let until = time(row.try_get("expires_at")?)?
            .max(retained_until(time(row.try_get("deleted_at")?)?));
        refs.push(Reference {
            object: ObjectKey {
                root,
                hash: hash(row.try_get("object_hash")?)?,
            },
            reason: Reason::Trash,
            until: Some(until),
        });
    }
    let rows = sqlx::query("SELECT object_hash AS hash,purpose,expires_at FROM object_leases WHERE root_id=? UNION ALL SELECT hash,'upload',expires_at FROM uploads WHERE root_id=? AND state IN ('open','complete','finalized')")
        .bind(root.as_bytes().as_slice()).bind(root.as_bytes().as_slice()).fetch_all(&mut **tx).await?;
    for row in rows {
        let purpose: String = row.try_get("purpose")?;
        let reason = match purpose.as_str() {
            "download" => Reason::Download,
            "commit" => Reason::Commit,
            "upload" => Reason::Upload,
            _ => return Err(Error::InvalidFact),
        };
        refs.push(Reference {
            object: ObjectKey {
                root,
                hash: hash(row.try_get("hash")?)?,
            },
            reason,
            until: Some(time(row.try_get("expires_at")?)?),
        });
    }
    let unfinished: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM operations WHERE root_id=? AND state='pending') OR EXISTS(SELECT 1 FROM fs_intents WHERE root_id=? AND phase<>'finalized')")
        .bind(root.as_bytes().as_slice()).bind(root.as_bytes().as_slice()).fetch_one(&mut **tx).await?;
    let backups = crate::leases::active_backup_leases(tx, now)
        .await
        .map_err(Error::Lease)?;
    let backup_expiry = backups
        .iter()
        .filter(|l| l.snapshot_seq_map.entries().iter().any(|(r, _)| *r == root))
        .map(|l| l.expires_at)
        .max();
    // 未完成操作的恢复编码可能引用任意旧对象；备份尚无冻结对象清单。两者都保守保护 Root 全部对象。
    if unfinished || backup_expiry.is_some() {
        let objects: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT hash FROM objects WHERE root_id=? ORDER BY hash")
                .bind(root.as_bytes().as_slice())
                .fetch_all(&mut **tx)
                .await?;
        for bytes in objects {
            let object = ObjectKey {
                root,
                hash: hash(bytes)?,
            };
            if unfinished {
                refs.push(Reference {
                    object,
                    reason: Reason::UnfinishedOperation,
                    until: None,
                });
            }
            if let Some(until) = backup_expiry {
                refs.push(Reference {
                    object,
                    reason: Reason::Backup,
                    until: Some(until),
                });
            }
        }
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use vaultsyncd_server_core::gc::mark::{RETENTION_SECONDS, mark};

    #[tokio::test]
    async fn persisted_versions_use_supersede_and_delete_times_and_current_is_ageless() {
        let dir = TestDir::new("s085-versions");
        let db = open_database(&dir).await;
        let owner = seed_owner(&db, 1).await;
        let (root, parent) = register_root(&db, owner, 2).await;
        let file = seed_file(&db, owner, root, parent, 3, "data").await;
        let first = seed_object(&db, owner, root, 11, 1).await;
        let current = seed_object(&db, owner, root, 12, 1).await;
        let mut tx = db.begin_write().await.unwrap();
        sqlx::query("INSERT INTO changes VALUES (?,1,?,'[{}]',1)")
            .bind(root.as_bytes().as_slice())
            .bind([4u8; 16].as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        for (id, digest, superseded) in [(5u8, first, Some(100i64)), (6, current, None)] {
            sqlx::query("INSERT INTO versions VALUES (?,?,?,?,1,1,?,'client')")
                .bind([id; 16].as_slice())
                .bind(root.as_bytes().as_slice())
                .bind(file.as_bytes().as_slice())
                .bind(digest.as_slice())
                .bind(superseded)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE entries SET content_version_id=? WHERE id=?")
            .bind([6u8; 16].as_slice())
            .bind(file.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        // 真实落库后重开；verified_at 与业务时间无关。
        drop(db);
        let db = open_database(&dir).await;
        let mut tx = db.begin_write().await.unwrap();
        let before = mark(
            at(100 + RETENTION_SECONDS - 1),
            references(&mut tx, owner, root, at(100 + RETENTION_SECONDS - 1))
                .await
                .unwrap(),
        );
        assert!(before.contains_key(&ObjectKey { root, hash: first }));
        let now = at(100 + RETENTION_SECONDS);
        let marks = mark(now, references(&mut tx, owner, root, now).await.unwrap());
        assert!(!marks.contains_key(&ObjectKey { root, hash: first }));
        assert_eq!(
            marks[&ObjectKey {
                root,
                hash: current
            }],
            std::collections::BTreeSet::from([Reason::Current])
        );
        sqlx::query("UPDATE entries SET deleted_at=? WHERE id=?")
            .bind(now.to_db_integer())
            .bind(file.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO trash_entries VALUES (?,?,?,\"data\",\"file\",1,?,?,?)")
            .bind(file.as_bytes().as_slice())
            .bind(root.as_bytes().as_slice())
            .bind(parent.as_bytes().as_slice())
            .bind([6u8; 16].as_slice())
            .bind(now.to_db_integer())
            .bind(now.to_db_integer())
            .execute(&mut *tx)
            .await
            .unwrap();
        let later = at(now.as_unix_seconds() + RETENTION_SECONDS - 1);
        let marks = mark(
            later,
            references(&mut tx, owner, root, later).await.unwrap(),
        );
        assert!(
            marks[&ObjectKey {
                root,
                hash: current
            }]
                .contains(&Reason::History)
        );
        let expired = at(later.as_unix_seconds() + 1);
        assert!(
            mark(
                expired,
                references(&mut tx, owner, root, expired).await.unwrap()
            )
            .is_empty()
        );
    }

    #[tokio::test]
    async fn persisted_leases_uploads_and_unfinished_work_protect_unreferenced_objects() {
        let dir = TestDir::new("s085-leases");
        let db = open_database(&dir).await;
        let owner = seed_owner(&db, 1).await;
        let (root, _) = register_root(&db, owner, 2).await;
        let hash = seed_object(&db, owner, root, 11, 1).await;
        let mut tx = db.begin_write().await.unwrap();
        for (id, purpose) in [(3u8, "download"), (4, "commit")] {
            sqlx::query("INSERT INTO object_leases VALUES (?,?,?,?,20)")
                .bind([id; 16].as_slice())
                .bind(root.as_bytes().as_slice())
                .bind(hash.as_slice())
                .bind(purpose)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO uploads VALUES (?,?,?,1,?,1,20,'finalized')")
            .bind([5u8; 16].as_slice())
            .bind(root.as_bytes().as_slice())
            .bind([6u8; 16].as_slice())
            .bind(hash.as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        let map = crate::leases::SnapshotSeqMap::new(vec![(
            root,
            vaultsyncd_domain::version::RootSeq::ZERO,
        )])
        .unwrap();
        sqlx::query("INSERT INTO backup_leases VALUES (?, ?,20,'manifest')")
            .bind([7u8; 16].as_slice())
            .bind(map.to_json())
            .execute(&mut *tx)
            .await
            .unwrap();
        let marks = mark(
            at(19),
            references(&mut tx, owner, root, at(19)).await.unwrap(),
        );
        assert_eq!(
            marks[&ObjectKey { root, hash }],
            std::collections::BTreeSet::from([
                Reason::Download,
                Reason::Commit,
                Reason::Upload,
                Reason::Backup
            ])
        );
        assert!(
            mark(
                at(20),
                references(&mut tx, owner, root, at(20)).await.unwrap()
            )
            .is_empty()
        );
        sqlx::query("INSERT INTO fs_intents VALUES (?,?,'put_content','data','temporary','{}','{}','needs_recovery',1,1)").bind([8u8;16].as_slice()).bind(root.as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
        let marks = mark(
            at(100),
            references(&mut tx, owner, root, at(100)).await.unwrap(),
        );
        assert!(marks[&ObjectKey { root, hash }].contains(&Reason::UnfinishedOperation));
        sqlx::query("UPDATE fs_intents SET phase='finalized'")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO operations VALUES (?,?,?,?, 'pending',NULL,NULL,NULL)")
            .bind(root.as_bytes().as_slice())
            .bind([9u8; 16].as_slice())
            .bind([10u8; 16].as_slice())
            .bind([0u8; 32].as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(
            mark(
                at(100),
                references(&mut tx, owner, root, at(100)).await.unwrap()
            )[&ObjectKey { root, hash }]
                .contains(&Reason::UnfinishedOperation)
        );
        assert!(matches!(
            references(&mut tx, user(99), root, at(1)).await,
            Err(Error::OutOfScope)
        ));
    }

    #[tokio::test]
    async fn missing_version_evidence_fails_closed() {
        let dir = TestDir::new("s085-invalid");
        let db = open_database(&dir).await;
        let owner = seed_owner(&db, 1).await;
        let (root, parent) = register_root(&db, owner, 2).await;
        let file = seed_file(&db, owner, root, parent, 3, "broken").await;
        let mut tx = db.begin_write().await.unwrap();
        sqlx::query("UPDATE entries SET content_version_id=? WHERE id=?")
            .bind([9u8; 16].as_slice())
            .bind(file.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(matches!(
            references(&mut tx, owner, root, at(1)).await,
            Err(Error::InvalidFact)
        ));
    }
}
