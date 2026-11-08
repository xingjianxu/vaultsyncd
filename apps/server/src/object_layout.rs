//! Root 隔离对象布局；兼容回退必须来自该 Root 的持久对象登记，不能探测其它 Root 的文件。
use std::path::Path;

use vaultsyncd_domain::ids::RootId;
use vaultsyncd_server_core::objects::{ContentDigest, ObjectLayout};
use vaultsyncd_server_store::db::Database;

/// 调用方先完成 Root 授权，并在对象活动/维护门下使用布局。
/// 只保留旧数据库行的路径身份；不修改旧记录，也不从磁盘猜测旧对象归属。
///
/// 该 Root 全部登记都要参与时（外部扫描、删除核对、回收站恢复）用它；单条内容路径用
/// [`for_digests`] 按摘要查询，避免在每次读写时扫描整个 Root 的对象行。
pub(crate) async fn for_root(
    database: &Database,
    state_dir: &Path,
    root: RootId,
) -> Result<ObjectLayout, sqlx::Error> {
    let rows: Vec<(Vec<u8>, String)> =
        sqlx::query_as("SELECT hash, storage_path FROM objects WHERE root_id = ?")
            .bind(root.as_bytes().as_slice())
            .fetch_all(database.pool())
            .await?;
    let mut legacy = Vec::new();
    for (hash, path) in rows {
        let hash: [u8; 32] = hash
            .try_into()
            .map_err(|_| sqlx::Error::Protocol("object layout: invalid digest length".into()))?;
        let digest = ContentDigest::from_bytes(hash);
        if !ObjectLayout::valid_storage_path(root, &digest, &path) {
            return Err(sqlx::Error::Protocol(
                "object layout: noncanonical or cross-root storage path; preserved".into(),
            ));
        }
        if ObjectLayout::relative_object_path(&digest).to_str() == Some(path.as_str()) {
            legacy.push(digest);
        }
    }
    Ok(ObjectLayout::for_root(
        state_dir.join("objects"),
        root,
        legacy,
    ))
}

/// 只解析给定摘要的布局：未登记的摘要一律按 Root 分区的新路径处理。
///
/// 每条摘要一次主键查询，结果只含该 Root 已登记的旧路径摘要；跨 Root 或非规范登记失败关闭。
pub(crate) async fn for_digests(
    database: &Database,
    state_dir: &Path,
    root: RootId,
    digests: &[ContentDigest],
) -> Result<ObjectLayout, sqlx::Error> {
    let mut legacy = Vec::new();
    for digest in digests {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT storage_path FROM objects WHERE root_id = ? AND hash = ?")
                .bind(root.as_bytes().as_slice())
                .bind(digest.as_bytes().as_slice())
                .fetch_optional(database.pool())
                .await?;
        if let Some((path,)) = row {
            if !ObjectLayout::valid_storage_path(root, digest, &path) {
                return Err(sqlx::Error::Protocol(
                    "object layout: noncanonical or cross-root storage path; preserved".into(),
                ));
            }
            if ObjectLayout::relative_object_path(digest).to_str() == Some(path.as_str()) {
                legacy.push(*digest);
            }
        }
    }
    Ok(ObjectLayout::for_root(
        state_dir.join("objects"),
        root,
        legacy,
    ))
}

/// 单摘要便捷入口；内容路径由调用方按已授权 Root 解析。
pub(crate) async fn for_digest(
    database: &Database,
    state_dir: &Path,
    root: RootId,
    digest: ContentDigest,
) -> Result<ObjectLayout, sqlx::Error> {
    for_digests(database, state_dir, root, &[digest]).await
}
