-- 0015_root_scoped_objects.sql — 按 Root 隔离对象物理布局的语义版本围栏（S136-c）。
--
-- 新对象使用 roots/<root_id>/objects/<fanout>/<digest>；旧的
-- objects/<fanout>/<digest> 仍按所属 Root 的不可变 objects 行解析，不原地搬迁或改写。
-- objects.storage_path 的全局 UNIQUE、内容引用不可变触发器及外键均保持不变。
--
-- 表结构虽未变化，旧二进制只理解旧物理布局，不能安全写入新格式。必须通过正常迁移路径
-- 先备份、记入 _sqlx_migrations 并提升 user_version 到 15，让 target <= 14 的旧二进制
-- 在返回可写句柄之前拒绝启动；回退使用迁移前整套备份，不做自动降级。
SELECT 1;
