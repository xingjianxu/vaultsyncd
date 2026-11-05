-- 0001_initial.sql — 服务端数据库基线迁移（S026 迁移框架首次落地）。
--
-- 本迁移只建立迁移框架自己需要的记账表。design §7.2 的业务表（users、roots、entries、
-- versions、objects、changes、operations、uploads、upload_parts、conflicts、trash、
-- fs_intents、backup_leases）由 S027—S029 逐卡追加迁移，不在基线里预建空表。
--
-- STRICT 表：列类型按声明严格检查，避免“TEXT 列里塞进数字”这类跨端读回不一致；需要更宽
-- 兼容的列显式写 ANY。迁移文件一旦发布就不能改写（SQLx 以校验和比对），格式变更只能新增
-- 迁移。

CREATE TABLE store_metadata (
    key   TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
