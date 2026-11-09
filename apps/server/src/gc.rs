//! S086：同一对象门下的 GC，短事务与文件动作分开，deleting 证据跨重启保留。
use sqlx::Row;
use std::{io, path::Path};
use vaultsyncd_domain::version::UtcTimestamp;
use vaultsyncd_platform_linux_server::files::{ResolveMode, RestrictedDir, hash_file};
use vaultsyncd_server_core::objects::{ContentDigest, ObjectLayout};
use vaultsyncd_server_store::{
    db::Database,
    gc::sweep::{self, Candidate},
    vaults,
};

#[cfg(test)]
#[path = "gc_tests.rs"]
mod tests;

/// 一轮的实际删除计数（不是标记成功数）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// 对象文件已删除并完成数据库收尾。
    pub objects: usize,
    /// 过期未完成上传的块已删除并移除块行。
    pub parts: usize,
    /// 过期未完成会话内尚未登记的可重建临时块。
    pub temporary_files: usize,
}

fn failure(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// 在共享对象门下标记、宽限后复核、持久授权，再按 fd 锚定路径删对象并刷盘。
/// 不扫描用户 Vault、不读取 mtime、不做全盘垃圾猜测；任何失败中止本轮，证据留待重试。
pub async fn run_once(
    database: &Database,
    state_dir: &Path,
    now: UtcTimestamp,
) -> io::Result<Report> {
    crate::test_faults::checkpoint("gc_before_lock");
    let _objects = database.object_maintenance().await;
    let mut conn = database.pool().acquire().await.map_err(failure)?;
    let vaults = vaults::all_vaults(&mut conn).await.map_err(failure)?;
    drop(conn);
    // 先清过期上传的可重建临时名，避免发布后硬链接清理前崩溃留下 nlink=2 阻塞对象复核。
    let parts = clean_uploads(database, state_dir, now).await?;
    let temporary_files = clean_unregistered_parts(database, state_dir, now).await?;
    let mut report = Report {
        objects: 0,
        parts,
        temporary_files,
    };
    for vault in vaults {
        let mut tx = database.begin_write().await.map_err(failure)?;
        sweep::mark_vault(&mut tx, vault.owner, vault.id, now)
            .await
            .map_err(failure)?;
        let candidates = sweep::candidates(&mut tx, vault.owner, vault.id)
            .await
            .map_err(failure)?;
        tx.commit().await.map_err(failure)?;
        crate::test_faults::checkpoint("gc_marked");
        for candidate in candidates {
            if !candidate.deleting
                && vaultsyncd_server_core::gc::sweep::decide(candidate.marked_at, now, false)
                    == vaultsyncd_server_core::gc::sweep::Decision::Wait
            {
                continue;
            }
            // 先验证路径/已有文件，错误对象绝不以 GC 自动消失；deleting 的缺文件是可重放边界。
            let directory = object_directory(state_dir, &candidate)?;
            let name = ContentDigest::from_bytes(candidate.object.hash).to_hex();
            match directory.identity_at(Path::new(&name)).map_err(failure)? {
                Some(_) => {
                    let object = directory.open_read(Path::new(&name)).map_err(failure)?;
                    if object.identity.nlink != 1
                        || hash_file(&object.file).map_err(failure)?.0 != candidate.object.hash
                    {
                        return Err(failure("GC object identity/hash mismatch; preserved"));
                    }
                }
                None if candidate.deleting => {}
                None => return Err(failure("GC live object missing; metadata preserved")),
            }
            let mut tx = database.begin_write().await.map_err(failure)?;
            let authorized = sweep::authorize_delete(&mut tx, vault.owner, &candidate, now)
                .await
                .map_err(failure)?;
            tx.commit().await.map_err(failure)?;
            if !authorized {
                continue;
            }
            crate::test_faults::checkpoint("gc_deleting");
            unlink_object(&directory, &name)?;
            crate::test_faults::checkpoint("gc_unlinked");
            let mut tx = database.begin_write().await.map_err(failure)?;
            sweep::finish_delete(&mut tx, &candidate)
                .await
                .map_err(failure)?;
            tx.commit().await.map_err(failure)?;
            report.objects += 1;
        }
    }
    Ok(report)
}

fn object_directory(state: &Path, candidate: &Candidate) -> io::Result<RestrictedDir> {
    let digest = ContentDigest::from_bytes(candidate.object.hash);
    if !ObjectLayout::valid_storage_path(candidate.object.vault, &digest, &candidate.storage_path) {
        return Err(failure("GC storage path is not canonical; preserved"));
    }
    let expected = Path::new(&candidate.storage_path);
    let base = RestrictedDir::open(&state.join("objects"), ResolveMode::Auto).map_err(failure)?;
    base.child_dir(
        expected
            .parent()
            .ok_or_else(|| failure("GC object path missing parent"))?,
    )
    .map_err(failure)
}

fn unlink_object(directory: &RestrictedDir, name: &str) -> io::Result<()> {
    if directory
        .identity_at(Path::new(name))
        .map_err(failure)?
        .is_some()
    {
        directory.unlink(Path::new(name)).map_err(failure)?;
    }
    directory.fsync_dir().map_err(failure)
}

async fn clean_uploads(database: &Database, state: &Path, now: UtcTimestamp) -> io::Result<usize> {
    // 先落 aborted，任何重启/重试都不能再接纳这些块。finalized 与有效会话不参与清理。
    let mut tx = database.begin_write().await.map_err(failure)?;
    sqlx::query(
        "UPDATE uploads SET state='aborted' WHERE expires_at<=? AND state IN ('open','complete')",
    )
    .bind(now.to_db_integer())
    .execute(&mut *tx)
    .await
    .map_err(failure)?;
    let parts=sqlx::query("SELECT p.upload_id,p.part_index,p.path FROM upload_parts p JOIN uploads u ON u.id=p.upload_id WHERE u.state='aborted' AND u.expires_at<=? ORDER BY u.id,p.part_index LIMIT 100")
        .bind(now.to_db_integer()).fetch_all(&mut *tx).await.map_err(failure)?;
    tx.commit().await.map_err(failure)?;
    let mut count = 0;
    for p in parts {
        let id: Vec<u8> = p.try_get("upload_id").map_err(failure)?;
        let id: [u8; 16] = id
            .try_into()
            .map_err(|_| failure("GC invalid upload identity"))?;
        let index: i64 = p.try_get("part_index").map_err(failure)?;
        let name = format!("{index:020}");
        let relative = format!(
            "uploads/{}/{name}",
            vaultsyncd_domain::ids::UploadId::from_bytes(id).to_hex()
        );
        let recorded: String = p.try_get("path").map_err(failure)?;
        if relative != recorded {
            return Err(failure("GC upload part path is not canonical; preserved"));
        }
        let base = RestrictedDir::open(state, ResolveMode::Auto).map_err(failure)?;
        match base.child_dir(
            Path::new(&relative)
                .parent()
                .ok_or_else(|| failure("GC invalid part path"))?,
        ) {
            Ok(directory) => unlink_object(&directory, &name)?,
            Err(vaultsyncd_platform_linux_server::files::RestrictError::NotFound { .. }) => {}
            Err(e) => return Err(failure(e)),
        }
        let mut tx = database.begin_write().await.map_err(failure)?;
        sqlx::query("DELETE FROM upload_parts WHERE upload_id=? AND part_index=? AND path=? AND EXISTS(SELECT 1 FROM uploads WHERE id=? AND state='aborted' AND expires_at<=?)")
            .bind(id.as_slice()).bind(index).bind(recorded).bind(id.as_slice()).bind(now.to_db_integer()).execute(&mut *tx).await.map_err(failure)?;
        tx.commit().await.map_err(failure)?;
        count += 1;
    }
    Ok(count)
}

async fn clean_unregistered_parts(
    database: &Database,
    state: &Path,
    now: UtcTimestamp,
) -> io::Result<usize> {
    let ids: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT id FROM uploads WHERE state='aborted' AND expires_at<=? ORDER BY id LIMIT 100",
    )
    .bind(now.to_db_integer())
    .fetch_all(database.pool())
    .await
    .map_err(failure)?;
    let mut count = 0;
    for bytes in ids {
        let id: [u8; 16] = bytes
            .try_into()
            .map_err(|_| failure("GC invalid upload identity"))?;
        let relative = format!(
            "uploads/{}",
            vaultsyncd_domain::ids::UploadId::from_bytes(id).to_hex()
        );
        let base = RestrictedDir::open(state, ResolveMode::Auto).map_err(failure)?;
        count += clean_upload_assembly(&base, &id)?;
        let directory = match base.child_dir(Path::new(&relative)) {
            Ok(d) => d,
            Err(vaultsyncd_platform_linux_server::files::RestrictError::NotFound { .. }) => {
                forget_upload(database, &id, now).await?;
                continue;
            }
            Err(e) => return Err(failure(e)),
        };
        for entry in directory.list_entries().map_err(failure)? {
            let Some(name) = entry.name.to_str() else {
                return Err(failure("GC unrecognized temporary filename; preserved"));
            };
            let canonical_part = name.len() == 20 && name.bytes().all(|b| b.is_ascii_digit());
            let temporary = name
                .strip_prefix('.')
                .and_then(|n| n.strip_suffix(".tmp"))
                .and_then(|n| n.split_once('.'))
                .is_some_and(|(index, pid)| {
                    index.len() == 20
                        && index.bytes().all(|b| b.is_ascii_digit())
                        && !pid.is_empty()
                        && pid.bytes().all(|b| b.is_ascii_digit())
                });
            if !canonical_part && !temporary {
                return Err(failure("GC unrecognized temporary filename; preserved"));
            }
            if entry.kind != vaultsyncd_platform_linux_server::files::EntryKind::File {
                return Err(failure("GC unexpected temporary entry kind; preserved"));
            }
            let registered: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM upload_parts WHERE upload_id=? AND path=?)",
            )
            .bind(id.as_slice())
            .bind(format!("{relative}/{name}"))
            .fetch_one(database.pool())
            .await
            .map_err(failure)?;
            if registered {
                continue;
            }
            unlink_object(&directory, name)?;
            count += 1;
        }
        let has_parts: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM upload_parts WHERE upload_id=?)")
                .bind(id.as_slice())
                .fetch_one(database.pool())
                .await
                .map_err(failure)?;
        if !has_parts {
            let uploads = base.child_dir(Path::new("uploads")).map_err(failure)?;
            uploads
                .unlink_dir(Path::new(
                    &vaultsyncd_domain::ids::UploadId::from_bytes(id).to_hex(),
                ))
                .map_err(failure)?;
            uploads.fsync_dir().map_err(failure)?;
            forget_upload(database, &id, now).await?;
        }
    }
    Ok(count)
}

async fn forget_upload(database: &Database, id: &[u8; 16], now: UtcTimestamp) -> io::Result<()> {
    let mut tx = database.begin_write().await.map_err(failure)?;
    sqlx::query("DELETE FROM uploads WHERE id=? AND state='aborted' AND expires_at<=?")
        .bind(id.as_slice())
        .bind(now.to_db_integer())
        .execute(&mut *tx)
        .await
        .map_err(failure)?;
    tx.commit().await.map_err(failure)
}

fn clean_upload_assembly(base: &RestrictedDir, id: &[u8; 16]) -> io::Result<usize> {
    let staging = match base.child_dir(Path::new("objects/staging")) {
        Ok(d) => d,
        Err(vaultsyncd_platform_linux_server::files::RestrictError::NotFound { .. }) => {
            return Ok(0);
        }
        Err(e) => return Err(failure(e)),
    };
    let id = vaultsyncd_domain::ids::UploadId::from_bytes(*id).to_hex();
    let prefixes = [format!("finalize-{id}-"), format!(".assemble-{id}-")];
    let mut count = 0;
    for entry in staging.list_entries().map_err(failure)? {
        let Some(name) = entry.name.to_str() else {
            continue;
        };
        if !prefixes
            .iter()
            .any(|p| name.starts_with(p) && name.len() > p.len())
        {
            continue;
        }
        if entry.kind != vaultsyncd_platform_linux_server::files::EntryKind::File {
            return Err(failure(
                "GC assembly entry is not a regular file; preserved",
            ));
        }
        unlink_object(&staging, name)?;
        count += 1;
    }
    Ok(count)
}
