//! 一致备份prepare的应用装配；在线管理入口由S088接入，不提供运行中restore写库。
use crate::config::Config;
pub mod export;
pub mod http;
pub mod jobs;
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use vaultsyncd_domain::{
    ids::{BackupLeaseId, ServerId, UserId},
    version::UtcTimestamp,
};
use vaultsyncd_platform_linux_server::{
    backup::Arena,
    files::{
        ResolveMode, RestrictedDir,
        clone::{StageRequest, stage_copy},
        hash::{digest, hex},
        hash_file,
    },
};
use vaultsyncd_server_core::{backup::prepare as rules, objects::ContentDigest};
use vaultsyncd_server_store::{
    backup::{FrozenDatabase, online_backup},
    db::Database,
    leases::{self, NewBackupLease, SnapshotSeqMap},
};

/// 准备租约24小时；导出与verify/release由调用者安排，不内置周期备份。
pub const LEASE_SECONDS: i64 = 86400;
/// 准备完成的固定导出源；不代表已导出到独立故障域并通过verify。
#[derive(Debug)]
pub struct Prepared {
    /// 持久备份身份，亦为对应租约身份。
    pub id: BackupLeaseId,
    /// 只读btrfs快照。
    pub snapshot: PathBuf,
    /// 固定清单，形状见api/backup-manifest.schema.json。
    pub manifest: Value,
    /// 清单SHA-256。
    pub manifest_hash: [u8; 32],
}
fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
/// 必须从持有服务状态锁的在线进程调用；owner必须是唯一active owner。所有逻辑内容动作与GC在同一独占门下静止。
pub async fn prepare(
    database: &Database,
    config: &Config,
    owner: UserId,
    id: BackupLeaseId,
    now: UtcTimestamp,
) -> io::Result<Prepared> {
    let _objects = database.object_maintenance().await;
    let owners: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT id FROM users WHERE status='active' LIMIT 2")
            .fetch_all(database.pool())
            .await
            .map_err(err)?;
    if owners != vec![owner.as_bytes().to_vec()] {
        return Err(err("backup requires unique active owner"));
    }
    let arena = Arena::open(&config.state_dir.join("backups"))?;
    recover_locked(database, &arena, now).await?;
    // 已开始的对象动作已退出读门；在屏障内继续同一操作的意图收尾。
    crate::replay::registered(database, &config.state_dir)
        .await
        .map_err(err)?;
    let pending:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM fs_intents WHERE phase<>'finalized')+(SELECT count(*) FROM operations WHERE state='pending')+(SELECT count(*) FROM objects WHERE gc_state<>'live')").fetch_one(database.pool()).await.map_err(err)?;
    if pending != 0 {
        return Err(err(
            "unfinished intent/operation/GC must be recovered before backup",
        ));
    }
    let expires = UtcTimestamp::from_unix_seconds(
        now.as_unix_seconds()
            .checked_add(LEASE_SECONDS)
            .ok_or_else(|| err("backup lease time overflow"))?,
    )
    .map_err(err)?;
    let mut tx = database.begin_write().await.map_err(err)?;
    sqlx::query(
        "INSERT INTO store_metadata(key,value) VALUES('server_id',?) ON CONFLICT DO NOTHING",
    )
    .bind(ServerId::from_bytes(crate::uploads::random_bytes()).to_hex())
    .execute(&mut *tx)
    .await
    .map_err(err)?;
    sqlx::query("INSERT INTO backup_preparations(id,owner_id,phase,created_at,expires_at) VALUES(?,?,'preparing',?,?)")
        .bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).bind(now.to_db_integer()).bind(expires.to_db_integer()).execute(&mut *tx).await.map_err(err)?;
    tx.commit().await.map_err(err)?;
    crate::test_faults::checkpoint("backup_preparing");
    let result = prepare_locked(database, config, &arena, owner, id, now, expires).await;
    if let Err(original) = result {
        let detail = original.to_string();
        let mut tx = database.begin_write().await.map_err(err)?;
        sqlx::query("UPDATE backup_preparations SET phase='failed',detail=? WHERE id=?")
            .bind(&detail)
            .bind(id.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        tx.commit().await.map_err(err)?;
        if let Err(cleanup) = recover_locked(database, &arena, now).await {
            return Err(err(format!("{detail}; cleanup pending: {cleanup}")));
        }
        return Err(original);
    }
    result
}

async fn prepare_locked(
    database: &Database,
    config: &Config,
    arena: &Arena,
    owner: UserId,
    id: BackupLeaseId,
    now: UtcTimestamp,
    expires: UtcTimestamp,
) -> io::Result<Prepared> {
    let prepared = arena.create(&id.to_hex())?;
    let source = database.path().to_path_buf();
    let output = prepared.join("database.sqlite");
    let frozen = tokio::task::spawn_blocking(move || online_backup(&source, &output))
        .await
        .map_err(err)??;
    ServerId::from_hex(&frozen.server_id).map_err(err)?;
    let map = if frozen.plan.roots.is_empty() {
        None
    } else {
        Some(
            SnapshotSeqMap::new(frozen.plan.roots.iter().map(|r| (r.root, r.seq)).collect())
                .map_err(err)?,
        )
    };
    let mut tx = database.begin_write().await.map_err(err)?;
    if let Some(map) = &map {
        leases::insert_backup_lease(
            &mut tx,
            &NewBackupLease {
                id,
                snapshot_seq_map: map.clone(),
                expires_at: expires,
                manifest_ref: format!("backups/{}/snapshot/manifest.json", id.to_hex()),
            },
            now,
        )
        .await
        .map_err(err)?;
    }
    sqlx::query("UPDATE backup_preparations SET snapshot_seq_map=? WHERE id=? AND owner_id=? AND phase='preparing'")
        .bind(map.as_ref().map_or_else(||"{}".into(),SnapshotSeqMap::to_json)).bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).execute(&mut *tx).await.map_err(err)?;
    tx.commit().await.map_err(err)?;
    crate::test_faults::checkpoint("backup_leased");
    let manifest = copy_manifest(config, &prepared, id, now, expires, &frozen)?;
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(err)?;
    let manifest_hash = digest(&bytes);
    write_private(&prepared.join("manifest.json"), &bytes)?;
    fs::File::open(&prepared)?.sync_all()?;
    let snapshot = arena.freeze(&id.to_hex())?;
    crate::test_faults::checkpoint("backup_frozen");
    // 最后一次实际读回快照，不凭成功退出推断清单已固定。
    if fs::read(snapshot.join("manifest.json"))? != bytes {
        return Err(err("snapshot manifest mismatch"));
    }
    let mut conn = database.pool().acquire().await.map_err(err)?;
    let lease_active = if map.is_none() {
        true
    } else {
        leases::backup_lease(&mut conn, id)
            .await
            .map_err(err)?
            .is_some_and(|l| l.expires_at > crate::uploads::server_now())
    };
    drop(conn);
    rules::ready(
        &frozen.plan,
        0,
        true,
        arena.is_read_only(&id.to_hex())?,
        lease_active,
    )
    .map_err(|e| err(format!("{e:?}")))?;
    let mut tx = database.begin_write().await.map_err(err)?;
    sqlx::query("UPDATE backup_preparations SET phase='ready',manifest_hash=? WHERE id=? AND phase='preparing'")
        .bind(manifest_hash.as_slice()).bind(id.as_bytes().as_slice()).execute(&mut *tx).await.map_err(err)?;
    tx.commit().await.map_err(err)?;
    crate::test_faults::checkpoint("backup_ready");
    Ok(Prepared {
        id,
        snapshot,
        manifest,
        manifest_hash,
    })
}

fn copy_manifest(
    config: &Config,
    prepared: &Path,
    id: BackupLeaseId,
    now: UtcTimestamp,
    expires: UtcTimestamp,
    frozen: &FrozenDatabase,
) -> io::Result<Value> {
    let settings = json!({"server":{"listen":config.listen.to_string()},"state":{"original_directory":config.state_dir},"roots":{"allowed_bases":config.allowed_root_bases.as_ref().map(|b|b.paths().map(|p|p.display().to_string()).collect::<Vec<_>>())},"log":{"level":config.log.level.to_string(),"format":config.log.format.as_str()}});
    write_private(
        &prepared.join("config.json"),
        &serde_json::to_vec_pretty(&settings).map_err(err)?,
    )?;
    let source = if frozen.plan.objects.is_empty() {
        None
    } else {
        Some(
            RestrictedDir::open(&config.state_dir.join("objects"), ResolveMode::Auto)
                .map_err(err)?,
        )
    };
    let mut objects = Vec::new();
    for o in &frozen.plan.objects {
        let source = source
            .as_ref()
            .ok_or_else(|| err("object directory missing"))?;
        let opened = source.open_read(Path::new(&o.storage_path)).map_err(err)?;
        if opened.identity.nlink != 1 {
            return Err(err("object is not an independent immutable file"));
        }
        let directory = prepared.join("objects").join(o.root.to_hex());
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        let target = RestrictedDir::open(&directory, ResolveMode::Auto).map_err(err)?;
        let name = ContentDigest::from_bytes(o.hash).to_hex();
        let staged = stage_copy(StageRequest {
            source: &opened.file,
            source_identity: &opened.identity,
            target_dir: &target,
            staging_name: &name,
        })
        .map_err(err)?;
        if staged.sha256 != o.hash || staged.bytes != o.size {
            return Err(err("backup object length/hash mismatch"));
        }
        staged
            .file
            .set_permissions(fs::Permissions::from_mode(0o400))?;
        staged.file.sync_all()?;
        target.fsync_dir().map_err(err)?;
        objects.push(json!({"root_id":o.root.to_hex(),"sha256":name,"size":o.size.to_string(),"storage_path":o.storage_path,"archive_path":format!("objects/{}/{}",o.root.to_hex(),name)}));
    }
    if prepared.join("objects").try_exists()? {
        fs::File::open(prepared.join("objects"))?.sync_all()?;
    }
    let mut files = Vec::new();
    for name in ["database.sqlite", "config.json"] {
        let file = fs::File::open(prepared.join(name))?;
        let (hash, size) = hash_file(&file)?;
        files.push(json!({"path":name,"sha256":hex(&hash),"size":size.to_string()}));
    }
    let roots:Vec<_>=frozen.mappings.iter().zip(&frozen.plan.roots).map(|((root,path,owner,epoch,volume,identity),r)|json!({"root_id":root,"seq":r.seq.to_decimal(),"original_path":path,"owner_id":owner,"epoch":epoch,"volume_identity":volume,"root_file_identity":identity})).collect();
    let contract = include_str!("../../../api/openapi.yaml");
    let protocol = contract
        .lines()
        .find_map(|l| l.trim().strip_prefix("version:"))
        .ok_or_else(|| err("API contract version missing"))?
        .trim()
        .trim_matches('"');
    Ok(
        json!({"format_version":1,"state":"prepared","backup_id":id.to_hex(),"server_id":frozen.server_id,"schema_version":frozen.schema,"protocol_contract_version":protocol,"protocol_contract_sha256":hex(&digest(contract.as_bytes())),"product_version":env!("CARGO_PKG_VERSION"),"created_at":now.as_unix_seconds().to_string(),"expires_at":expires.as_unix_seconds().to_string(),"database_contents":frozen.table_counts,"scope":"confirmed_logical_state","external_uncollected_writes":"excluded","sessions":"invalidated","roots":roots,"objects":objects,"files":files}),
    )
}

/// 启动/失败恢复只处理持久preparing/failed未清理ID；不枚举猜删其他目录。
pub async fn recover_incomplete(
    database: &Database,
    state_dir: &Path,
    now: UtcTimestamp,
) -> io::Result<usize> {
    let _objects = database.object_maintenance().await;
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM backup_preparations WHERE phase='preparing' OR (phase='failed' AND cleaned_at IS NULL)").fetch_one(database.pool()).await.map_err(err)?;
    if count == 0 {
        return Ok(0);
    }
    let arena = Arena::open(&state_dir.join("backups"))?;
    recover_locked(database, &arena, now).await
}
async fn recover_locked(
    database: &Database,
    arena: &Arena,
    now: UtcTimestamp,
) -> io::Result<usize> {
    let rows=sqlx::query("SELECT id FROM backup_preparations WHERE phase='preparing' OR (phase='failed' AND cleaned_at IS NULL)").fetch_all(database.pool()).await.map_err(err)?;
    let mut count = 0;
    for row in rows {
        let bytes: Vec<u8> = row.try_get("id").map_err(err)?;
        let id = BackupLeaseId::from_bytes(
            bytes
                .try_into()
                .map_err(|_| err("invalid backup identity"))?,
        );
        let mut tx = database.begin_write().await.map_err(err)?;
        sqlx::query("UPDATE backup_preparations SET phase='failed',detail=coalesce(detail,'interrupted preparation') WHERE id=?")
            .bind(id.as_bytes().as_slice()).execute(&mut *tx).await.map_err(err)?;
        tx.commit().await.map_err(err)?;
        arena.remove(&id.to_hex())?;
        let mut tx = database.begin_write().await.map_err(err)?;
        leases::release_backup_lease(&mut tx, id)
            .await
            .map_err(err)?;
        sqlx::query("UPDATE backup_preparations SET cleaned_at=? WHERE id=?")
            .bind(now.to_db_integer())
            .bind(id.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        tx.commit().await.map_err(err)?;
        count += 1;
    }
    Ok(count)
}
