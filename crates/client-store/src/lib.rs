//! 客户端 SQLite 存储层（`docs/design.md` §5.4、§7.3、§8.4）。
//!
//! 职责边界：本 crate 承载客户端本地持久化——独立的客户端数据库种类与 schema 版本门禁、
//! 迁移执行、一致性备份、短事务接口，以及绑定、基线、远端状态、本地状态、待执行操作、事件
//! 批次游标、传输进度、本地意图与通知的类型化读写。它不复用服务端 schema，也不打开服务端
//! 数据库文件（design §7.3：两套库各自独立）。它不做文件系统 I/O、不规划、不传输、不分配
//! 身份：指纹、file identity、generation、`op_id` 都由平台层与应用层给出，本 crate 只保存
//! 它们。
//!
//! 已落地模块：S030 的 [`db`]（`application_id` 种类标识、`user_version` schema 版本、迁移前
//! `VACUUM INTO` 一致性备份、WAL/外键/短事务连接选项）、[`bindings`]（绑定与同步模式）、
//! [`entries`]（基线、远端状态、本地观察三张互相独立的表）、[`queue`]（待执行本地操作），以及
//! S031 的 [`events`]（事件批次、接收游标与应用游标）、[`transfers`]（入站内容块位图与取消
//! 状态）、[`intents`]（可恢复的本地动作）、[`notifications`]（问题的去重、已读与恢复），以及
//! S055 的 [`upload_resumes`]（出站上传的快照与会话续传记录）。后续卡的迁移只往
//! `crates/client-store/migrations/` 追加文件，不改写已发布的迁移。
//!
//! 持久化的关键不变量（design §7.3、§8.4、§6）：
//!
//! - 同一条目同时保有“旧基线 + 新远端 + 本地 dirty”，三张表之间没有外键约束；
//! - 事件批次、远端状态、应用任务与接收游标在同一事务写入，接收游标最后推进（I09）；
//! - 迁移先于写入，失败不返回可写句柄（不部分启动）；
//! - 客户端库与服务端库用不同的 `application_id`，互相拒绝，且拒绝时不动对方的文件；
//! - 数据库写入是短事务，文件系统内容与 SQLite 不是一个事务。
#![forbid(unsafe_code)]

pub mod bindings;
pub mod db;
pub mod entries;
pub mod events;
pub mod intents;
pub mod notifications;
pub mod queue;
pub mod transfers;
pub mod upload_resumes;

#[cfg(test)]
mod test_support;
