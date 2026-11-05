//! 服务端用例核心（design §5.4、§5.5）：提交协调、不可变内容对象、外部采集与恢复编排。
//!
//! 本 crate 只放规则、顺序与端口契约，不做平台 I/O：
//!
//! - [`ports`] 定义服务端适配器必须实现的契约（内容对象的
//!   [`ports::object_store::ObjectBlobPort`]、Root 目标的 [`ports::target::TargetPort`]、意图日志的
//!   [`ports::intent_log::IntentLog`]、提交存储的 [`ports::put_store::PutCommitStore`] 与冲突提交的
//!   [`ports::conflict_store::ConflictCommitStore`]）；
//! - [`objects`] 用这些端口编排“工作文件 → 私有副本 → 长度/哈希核对 → 内容地址发布 →
//!   发布后复核”的顺序，并规定什么样的副本才允许成为不可变对象（design §11.1—11.2、I12、C37）；
//! - [`commit`] 规定一次 put 的持久阶段顺序（prepared → published → finalized）、前后证据编码与
//!   完整 CAS（design §11.2 步骤 4—7、I01、I04），并在过期基线上把不同内容提交为稳定身份的冲突条目
//!   （design §10.1—10.2、I06）；
//! - [`history`] 在提交协调之上做历史版本恢复与回收站恢复：按旧不可变版本创建新的当前版本（内容
//!   由短期租约保护，旧版本字节不变，I12），把回收站条目放回命名空间并在目标被占用时另名（T13）；
//! - [`recovery`] 按持久阶段与工作目录现场给出恢复结论，并在启动时先执行恢复、再开放写入口
//!   （design §11.3、I13）。
//!
//! 平台实现（例如 Linux 服务端的 `vaultsyncd-platform-linux-server`、服务端存储层的意图日志）只
//! 实现端口，不决定上述规则；进程入口负责组装，避免核心与平台层互相依赖。

#![forbid(unsafe_code)]

pub mod commit;
pub mod history;
pub mod objects;
pub mod ports;
pub mod recovery;

#[cfg(test)]
mod test_support;
