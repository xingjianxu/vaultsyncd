//! 服务端 SQLite 存储层（`docs/design.md` §5.4、§7.2、§7.4、§19.3）。
//!
//! 职责边界：本 crate 承载服务端元数据的持久化——数据库种类与 schema 版本门禁、迁移执行、
//! 一致性备份、短事务接口，以及命名空间（Root 与条目树）的类型化读写。业务表定义在
//! [`migrations/`](../../migrations)（编译期内嵌为 [`MIGRATOR`](crate::db::MIGRATOR)），其余业务
//! 读写（`versions.rs`、`operations.rs`、`changes.rs`）由后续卡落地。它不做 HTTP、不做文件系统
//! I/O、不实现同步语义：工作目录与不可变对象由 `crates/platform-linux-server` / 后续对象卡负责，
//! 两者不是一个事务（design §6）。
//!
//! 已落地模块：[`db`]（S026：`application_id` 种类标识、`user_version` schema 版本、迁移前
//! `VACUUM INTO` 一致性备份、WAL/外键/短事务连接选项）、[`roots`] 与 [`entries`]（S027：`roots`/
//! `entries` 表的事务内读写、owner scope、活跃同父名称唯一、父子同 Root、目录环检查）。后续卡的
//! 迁移只往 `crates/server-store/migrations/` 追加文件，不改写已发布的迁移。
#![forbid(unsafe_code)]

pub mod db;
pub mod entries;
pub mod roots;

#[cfg(test)]
mod test_support;
