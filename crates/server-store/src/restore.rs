//! 受离线独占锁保护的候选DB恢复：读取核心命名空间、事务更新映射/epoch。
//! 调用者负责StateLock、目标句柄及普通工作文件；本层不接受任意SQL。
use rusqlite::{Connection, OpenFlags};
use std::{collections::BTreeMap, io, path::Path};
use vaultsyncd_domain::ids::{EntryId, RootId};
use vaultsyncd_server_core::backup::{
    export::Index,
    restore::{self, Entry, Work},
};
fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
/// 固定候选数据库推导的工作计划，不包含密码或会话材料。
pub struct Plan {
    /// 核心验证后的活跃普通工作项，父目录先于子项。
    pub work: Vec<Work>,
    /// 各Root原恢复代际。
    pub epochs: BTreeMap<RootId, u64>,
}
/// 核对DB与固定清单后，只读提取活跃命名空间并交核心规划。
pub fn plan(path: &Path, index: &Index) -> io::Result<Plan> {
    crate::backup::verify_database(path, index)?;
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(err)?;
    let mut epochs = BTreeMap::new();
    let mut q = db
        .prepare("SELECT lower(hex(id)),epoch FROM roots ORDER BY id")
        .map_err(err)?;
    for row in q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(err)?
    {
        let (id, epoch) = row.map_err(err)?;
        epochs.insert(
            RootId::from_hex(&id).map_err(err)?,
            u64::try_from(epoch).map_err(err)?,
        );
    }
    let mut q=db.prepare("SELECT lower(hex(e.id)),lower(hex(e.root_id)),CASE WHEN e.parent_id IS NULL THEN NULL ELSE lower(hex(e.parent_id)) END,e.name,e.kind,v.object_hash,v.size FROM entries e LEFT JOIN versions v ON v.id=e.content_version_id AND v.root_id=e.root_id AND v.entry_id=e.id WHERE e.deleted_at IS NULL ORDER BY e.id").map_err(err)?;
    let mut entries = Vec::new();
    for row in q
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<Vec<u8>>>(5)?,
                r.get::<_, Option<i64>>(6)?,
            ))
        })
        .map_err(err)?
    {
        let (id, root, parent, name, kind, hash, size) = row.map_err(err)?;
        if !["directory", "file"].contains(&kind.as_str()) {
            return Err(err("unsupported restored entry kind"));
        }
        let content = match (hash, size) {
            (Some(h), Some(n)) => Some((
                h.try_into().map_err(|_| err("invalid restored digest"))?,
                u64::try_from(n).map_err(err)?,
            )),
            (None, None) => None,
            _ => return Err(err("incomplete restored version")),
        };
        entries.push(Entry {
            id: EntryId::from_hex(&id).map_err(err)?,
            root: RootId::from_hex(&root).map_err(err)?,
            parent: parent
                .as_deref()
                .map(EntryId::from_hex)
                .transpose()
                .map_err(err)?,
            name,
            directory: kind == "directory",
            content,
        });
    }
    Ok(Plan {
        work: restore::namespace(index, entries).map_err(err)?,
        epochs,
    })
}
/// 每个Root的新物理映射，来源必须是应用入口已验证的实际目录句柄。
pub struct Remap {
    /// 固定逻辑Root身份。
    pub root: RootId,
    /// 规范绝对新路径。
    pub path: String,
    /// 新btrfs FSID文本字节，与Root注册编码一致。
    pub volume: Vec<u8>,
    /// 实际RootSnapshot编码。
    pub identity: Vec<u8>,
}
/// 所有文件已完成后，在同一短事务改变Root映射与epoch，ServerId和历史不变。
/// epoch由OS随机源生成且不得复用任何旧值，避免同一旧备份重复恢复分配相同代际。
pub fn apply(path: &Path, index: &Index, roots: &[Remap], epoch: u64) -> io::Result<()> {
    let original = plan(path, index)?;
    if epoch > i64::MAX as u64
        || original.epochs.values().any(|old| *old == epoch)
        || roots.len() != original.epochs.len()
    {
        return Err(err("restore requires a fresh epoch and complete mappings"));
    }
    let mut db = Connection::open(path).map_err(err)?;
    db.execute_batch("PRAGMA foreign_keys=ON;PRAGMA synchronous=FULL;")
        .map_err(err)?;
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(err)?;
    let pending:i64=tx.query_row("SELECT (SELECT count(*) FROM fs_intents WHERE phase<>'finalized')+(SELECT count(*) FROM operations WHERE state='pending')",[],|r|r.get(0)).map_err(err)?;
    if pending != 0 {
        return Err(err("unfinished backup cannot be restored"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for root in roots {
        if !original.epochs.contains_key(&root.root)
            || !seen.insert(root.root)
            || !restore::canonical_path(Path::new(&root.path))
            || root.volume.is_empty()
            || root.identity.is_empty()
        {
            return Err(err("invalid restored root mapping"));
        }
        let n=tx.execute("UPDATE roots SET absolute_path=?,volume_identity=?,root_file_identity=?,health='healthy',epoch=? WHERE id=?",rusqlite::params![root.path,root.volume,root.identity,epoch as i64,root.root.as_bytes().as_slice()]).map_err(err)?;
        if n != 1 {
            return Err(err("restore Root mapping missing"));
        }
    }
    tx.execute_batch("UPDATE sessions SET replaced_by=NULL;DELETE FROM sessions;")
        .map_err(err)?;
    for (key, value) in [
        ("restore_backup_id", index.id.to_hex()),
        ("restore_epoch", epoch.to_string()),
        (
            "restore_manifest_hash",
            vaultsyncd_server_core::objects::ContentDigest::from_bytes(index.manifest_hash)
                .to_hex(),
        ),
    ] {
        tx.execute("INSERT INTO store_metadata(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[key,&value]).map_err(err)?;
    }
    tx.commit().map_err(err)?;
    drop(db);
    crate::backup::verify_database(path, index)?;
    std::fs::File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use vaultsyncd_domain::ids::{BackupLeaseId, ServerId};
    #[tokio::test]
    async fn offline_mapping_preserves_logical_state_and_requires_fresh_epoch() {
        let dir = TestDir::new("s089-remap");
        let db = open_database(&dir).await;
        let owner = seed_owner(&db, 1).await;
        let (root, _) = register_root(&db, owner, 2).await;
        sqlx::query("INSERT INTO store_metadata VALUES('server_id',?)")
            .bind("abababababababababababababababab")
            .execute(db.pool())
            .await
            .unwrap();
        let path = dir.path.join("copy.sqlite");
        let frozen = crate::backup::online_backup(db.path(), &path).unwrap();
        let index = Index {
            id: BackupLeaseId::from_bytes([1; 16]),
            server: ServerId::from_hex(&frozen.server_id).unwrap(),
            manifest_hash: [1; 32],
            items: ["database.sqlite", "config.json"]
                .iter()
                .map(|p| vaultsyncd_server_core::backup::export::Item {
                    path: p.to_string(),
                    hash: [0; 32],
                    size: 0,
                })
                .collect(),
            root_seqs: frozen
                .plan
                .roots
                .iter()
                .map(|r| (r.root, r.seq.get()))
                .collect(),
            schema_version: frozen.schema,
            table_counts: frozen
                .table_counts
                .iter()
                .map(|(k, v)| (k.clone(), v.parse().unwrap()))
                .collect(),
        };
        let plan = plan(&path, &index).unwrap();
        assert!(plan.work.is_empty());
        let roots = [Remap {
            root,
            path: "/restored/work".into(),
            volume: b"new-fsid".to_vec(),
            identity: b"actual-handle-proof".to_vec(),
        }];
        assert!(apply(&path, &index, &roots, *plan.epochs.get(&root).unwrap()).is_err());
        apply(&path, &index, &roots, 42).unwrap();
        crate::backup::verify_database(&path, &index).unwrap();
        let copy = Connection::open(&path).unwrap();
        assert_eq!(
            copy.query_row("SELECT epoch FROM roots", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            42
        );
        assert_eq!(
            copy.query_row("SELECT absolute_path FROM roots", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "/restored/work"
        );
        assert_eq!(
            copy.query_row(
                "SELECT value FROM store_metadata WHERE key='server_id'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            frozen.server_id
        );
        db.close().await;
    }
}
