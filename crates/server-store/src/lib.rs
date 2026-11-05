//! 服务端 SQLite 存储层（`docs/design.md` §5.4、§7.2、§19.3）。
//!
//! 职责边界：本 crate 承载服务端元数据的持久化——数据库种类与 schema 版本门禁、迁移执行、
//! 一致性备份与短事务接口；业务表定义在 [`migrations/`](../../migrations)（编译期内嵌为
//! [`MIGRATION`](crate::db::MIGRATOR)），业务读写按 design §5.4 的 `entries.rs`、`versions.rs`、
//! `operations.rs`、`changes.rs` 分卡落地。它不做 HTTP、不做文件系统 I/O、不实现同步语义：
//! 工作目录与不可变对象由 `crates/platform-linux-server` / 后续对象卡负责，两者不是一个事务
//! （design §6）。
//!
//! 已落地模块（S026）：[`db`]（`application_id` 种类标识、`user_version` schema 版本、迁移前
//! `VACUUM INTO` 一致性备份、WAL/外键/短事务连接选项）。后续卡的迁移只往
//! `crates/server-store/migrations/` 追加文件，不改写已发布的迁移。
#![forbid(unsafe_code)]

pub mod db;
