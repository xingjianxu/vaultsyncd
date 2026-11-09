-- 0013_vault_rename.sql — 客户端库把共享目录概念由 Root 改名为 Vault（S140、ADR-021）。
--
-- `bindings.root_id` 只是服务端身份列的重命名，不改行数据与绑定语义。旧客户端二进制只理解
-- 旧列名，升级必须整套替换客户端；回退使用升级前整套备份（design §19.3）。
ALTER TABLE bindings RENAME COLUMN root_id TO vault_id;
