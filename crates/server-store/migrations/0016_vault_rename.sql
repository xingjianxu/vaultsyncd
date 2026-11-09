-- 0016_vault_rename.sql — 共享目录概念由 Root 改名为 Vault（S140、ADR-021）。
--
-- 只做重命名，不改数据语义：`roots` 表变为 `vaults`，各业务表的 `root_id` 列变为 `vault_id`，
-- `roots.root_file_identity` 变为 `vaults.vault_file_identity`，`operations.root_seq` 变为
-- `operations.vault_seq`，`trash_membership.deletion_root` 变为 `deletion_vault`，并重建名字里
-- 带 root 的索引。SQLite 的 ALTER TABLE RENAME 会同步更新外键、触发器与索引里的引用，因此
-- 结构等价、行数据原样保留。
--
-- 物理对象区前缀仍是 `roots/<vault_id>/objects/...`：那是 S136/ADR-020 已落地的磁盘布局，
-- 改名要搬迁全部已发布对象，不属于术语重命名范围（见 ADR-021）。
--
-- 旧二进制只理解 `roots`/`root_id` 与旧配置键，不能安全读写新 schema。必须通过正常迁移路径
-- 先备份、记入 `_sqlx_migrations` 并提升 user_version 到 16，让 target <= 15 的旧二进制在
-- 返回可写句柄之前拒绝启动；回退使用迁移前整套备份，不做自动降级。
ALTER TABLE roots RENAME TO vaults;
ALTER TABLE vaults RENAME COLUMN root_file_identity TO vault_file_identity;

ALTER TABLE entries RENAME COLUMN root_id TO vault_id;
ALTER TABLE objects RENAME COLUMN root_id TO vault_id;
ALTER TABLE changes RENAME COLUMN root_id TO vault_id;
ALTER TABLE versions RENAME COLUMN root_id TO vault_id;
ALTER TABLE operations RENAME COLUMN root_id TO vault_id;
ALTER TABLE operations RENAME COLUMN root_seq TO vault_seq;
ALTER TABLE uploads RENAME COLUMN root_id TO vault_id;
ALTER TABLE fs_intents RENAME COLUMN root_id TO vault_id;
ALTER TABLE object_leases RENAME COLUMN root_id TO vault_id;
ALTER TABLE trash_entries RENAME COLUMN root_id TO vault_id;
ALTER TABLE tombstones RENAME COLUMN root_id TO vault_id;
ALTER TABLE snapshots RENAME COLUMN root_id TO vault_id;
ALTER TABLE conflicts RENAME COLUMN root_id TO vault_id;
ALTER TABLE execution_journal RENAME COLUMN root_id TO vault_id;
ALTER TABLE intent_operations RENAME COLUMN root_id TO vault_id;
ALTER TABLE version_lease_requests RENAME COLUMN root_id TO vault_id;
ALTER TABLE gc_candidates RENAME COLUMN root_id TO vault_id;
ALTER TABLE trash_membership RENAME COLUMN deletion_root TO deletion_vault;

-- SQLite 不支持索引改名，删除后按原定义重建；列引用已由上面的 RENAME COLUMN 指向新列名。
DROP INDEX entries_one_active_root;
CREATE UNIQUE INDEX entries_one_active_vault
    ON entries (vault_id)
    WHERE deleted_at IS NULL AND parent_id IS NULL;

DROP INDEX uploads_open_by_root;
CREATE INDEX uploads_open_by_vault ON uploads (vault_id, expires_at, id);

DROP INDEX fs_intents_unfinished_by_root;
CREATE INDEX fs_intents_unfinished_by_vault ON fs_intents (vault_id, created_at, intent_id);

DROP INDEX snapshots_by_root;
CREATE INDEX snapshots_by_vault ON snapshots (vault_id, created_at, id);
