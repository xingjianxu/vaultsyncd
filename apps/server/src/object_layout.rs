//! Vault 隔离对象布局；兼容回退必须来自该 Vault 的持久对象登记，不能探测其它 Vault 的文件。
use std::path::Path;

use vaultsyncd_domain::ids::VaultId;
use vaultsyncd_server_core::objects::{ContentDigest, ObjectLayout};
use vaultsyncd_server_store::db::Database;

/// 调用方先完成 Vault 授权，并在对象活动/维护门下使用布局。
/// 只保留旧数据库行的路径身份；不修改旧记录，也不从磁盘猜测旧对象归属。
///
/// 该 Vault 全部登记都要参与时（外部扫描、删除核对、回收站恢复）用它；单条内容路径用
/// [`for_digests`] 按摘要查询，避免在每次读写时扫描整个 Vault 的对象行。
pub(crate) async fn for_vault(
    database: &Database,
    state_dir: &Path,
    vault: VaultId,
) -> Result<ObjectLayout, sqlx::Error> {
    let rows: Vec<(Vec<u8>, String)> =
        sqlx::query_as("SELECT hash, storage_path FROM objects WHERE vault_id = ?")
            .bind(vault.as_bytes().as_slice())
            .fetch_all(database.pool())
            .await?;
    let mut legacy = Vec::new();
    for (hash, path) in rows {
        let hash: [u8; 32] = hash
            .try_into()
            .map_err(|_| sqlx::Error::Protocol("object layout: invalid digest length".into()))?;
        let digest = ContentDigest::from_bytes(hash);
        if !ObjectLayout::valid_storage_path(vault, &digest, &path) {
            return Err(sqlx::Error::Protocol(
                "object layout: noncanonical or cross-vault storage path; preserved".into(),
            ));
        }
        if ObjectLayout::relative_object_path(&digest).to_str() == Some(path.as_str()) {
            legacy.push(digest);
        }
    }
    Ok(ObjectLayout::for_vault(
        state_dir.join("objects"),
        vault,
        legacy,
    ))
}

/// 只解析给定摘要的布局：未登记的摘要一律按 Vault 分区的新路径处理。
///
/// 每条摘要一次主键查询，结果只含该 Vault 已登记的旧路径摘要；跨 Vault 或非规范登记失败关闭。
pub(crate) async fn for_digests(
    database: &Database,
    state_dir: &Path,
    vault: VaultId,
    digests: &[ContentDigest],
) -> Result<ObjectLayout, sqlx::Error> {
    let mut legacy = Vec::new();
    for digest in digests {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT storage_path FROM objects WHERE vault_id = ? AND hash = ?")
                .bind(vault.as_bytes().as_slice())
                .bind(digest.as_bytes().as_slice())
                .fetch_optional(database.pool())
                .await?;
        if let Some((path,)) = row {
            if !ObjectLayout::valid_storage_path(vault, digest, &path) {
                return Err(sqlx::Error::Protocol(
                    "object layout: noncanonical or cross-vault storage path; preserved".into(),
                ));
            }
            if ObjectLayout::relative_object_path(digest).to_str() == Some(path.as_str()) {
                legacy.push(*digest);
            }
        }
    }
    Ok(ObjectLayout::for_vault(
        state_dir.join("objects"),
        vault,
        legacy,
    ))
}

/// 单摘要便捷入口；内容路径由调用方按已授权 Vault 解析。
pub(crate) async fn for_digest(
    database: &Database,
    state_dir: &Path,
    vault: VaultId,
    digest: ContentDigest,
) -> Result<ObjectLayout, sqlx::Error> {
    for_digests(database, state_dir, vault, &[digest]).await
}
