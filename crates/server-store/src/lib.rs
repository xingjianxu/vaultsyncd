//! 服务端 SQLite 存储层（`docs/design.md` §5.4、§7.2、§7.4、§19.3）。
//!
//! 职责边界：本 crate 承载服务端元数据的持久化——数据库种类与 schema 版本门禁、迁移执行、
//! 一致性备份、短事务接口、命名空间（Root 与条目树）、内容对象与版本历史、RootSeq 事务批次、
//! 幂等操作记录、上传会话与片段、工作目录发布意图、对象与备份租约的类型化读写。业务表定义在
//! [`migrations/`](../../migrations)（编译期内嵌为 [`MIGRATOR`](crate::db::MIGRATOR)），剩余业务表
//! （conflicts）由后续卡落地。它不做 HTTP、不做文件系统 I/O、不实现同步语义：工作目录与
//! 不可变对象由 `crates/platform-linux-server` 负责，两者不是一个事务（design §6）。
//!
//! 已落地模块：[`db`]（S026：`application_id` 种类标识、`user_version` schema 版本、迁移前
//! `VACUUM INTO` 一致性备份、WAL/外键/短事务连接选项）、[`roots`] 与 [`entries`]（S027：`roots`/
//! `entries` 表的事务内读写、owner scope、活跃同父名称唯一、父子同 Root、目录环检查）、
//! [`versions`]、[`changes`] 与 [`operations`]（S028：内容对象与不可变版本历史、RootSeq 事务批次、
//! 幂等操作记录与“批次 + 结果同一事务提交”）、[`uploads`]、[`intents`] 与 [`leases`]（S029：上传
//! 会话与固定块、显式 phase 的发布意图、下载/提交对象租约与备份租约）、[`trash`]（S039：回收站清单
//! 与墓碑、按有界批次比较并写的软删除）、[`users`] 与 [`state_lock`]（S042：首次 owner 账号的形态
//! 约束与状态目录互斥锁）。后续卡的迁移只往 `crates/server-store/migrations/` 追加文件，不改写
//! 已发布的迁移。
#![forbid(unsafe_code)]

pub mod changes;
pub mod db;
pub mod entries;
pub mod intents;
pub mod leases;
pub mod operations;
pub mod roots;
pub mod state_lock;
pub mod trash;
pub mod uploads;
pub mod users;
pub mod versions;

#[cfg(test)]
mod test_support;
