//! 对象区布局入口：对象一律落在 `<state>/objects/<vault hex>/<fanout>/<hash>`。
//!
//! 布局是纯计算，不再读数据库：同一 Vault 同一摘要只有一条规范路径，其它 Vault 的相同摘要
//! 落在完全不同的目录。旧全局布局与旧的 `roots/` 前缀已按 ADR-022 删除，持久行里的非规范
//! `storage_path` 会在使用前被 `valid_storage_path` 拒绝（失败关闭）。
use std::path::Path;

use vaultsyncd_domain::ids::VaultId;
use vaultsyncd_server_core::objects::ObjectLayout;

/// 该 Vault 的对象布局。调用方必须先完成 Vault 授权。
pub(crate) fn layout(state_dir: &Path, vault: VaultId) -> ObjectLayout {
    ObjectLayout::for_vault(state_dir.join("objects"), vault)
}
