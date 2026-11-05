//! 服务端用例核心（design §5.4、§5.5）：提交协调、不可变内容对象、外部采集与恢复编排。
//!
//! 本 crate 只放规则、顺序与端口契约，不做平台 I/O：
//!
//! - [`ports`] 定义服务端适配器必须实现的契约（内容对象的
//!   [`ports::object_store::ObjectBlobPort`]、Root 目标的 [`ports::target::TargetPort`]、意图日志的
//!   [`ports::intent_log::IntentLog`]）；
//! - [`objects`] 用这些端口编排“工作文件 → 私有副本 → 长度/哈希核对 → 内容地址发布 →
//!   发布后复核”的顺序，并规定什么样的副本才允许成为不可变对象（design §11.1—11.2、I12、C37）；
//! - [`commit`] 规定一次 put 的持久阶段顺序（prepared → published → finalized）与前后证据编码
//!   （design §11.2 步骤 4—7）；
//! - [`recovery`] 按持久阶段与工作目录现场给出恢复结论（design §11.3）。
//!
//! 平台实现（例如 Linux 服务端的 `vaultsyncd-platform-linux-server`、服务端存储层的意图日志）只
//! 实现端口，不决定上述规则；进程入口负责组装，避免核心与平台层互相依赖。

#![forbid(unsafe_code)]

pub mod commit;
pub mod objects;
pub mod ports;
pub mod recovery;

#[cfg(test)]
mod test_support;
