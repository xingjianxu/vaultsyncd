//! 客户端 agent 核心：端口契约与确定性操作执行器（`docs/design.md` §5.4、§5.5、§8.4、§13.3）。
//!
//! 职责边界：本 crate 是客户端应用服务（design §5.4 `crates/agent-core`）的纯核心——把持久队列
//! 里的待执行操作按依赖顺序执行，并在动本地内容前先落持久意图。它**不做平台 I/O**：文件系统、
//! 网络与 SQLite 都在端口后面（[`ports`]），适配器由应用入口组装（[`adapters`] 提供
//! `client-store` 的现成实现，`crates/http-client` 提供上传/内容端点，`crates/platform-windows`
//! 提供 CFAPI 与本地文件操作）。因此本 crate 可在 Linux 与 Windows 之间共用，测试用假端口即可
//! 覆盖调度语义。
//!
//! - [`ports`]：文件/平台、时钟、远端与持久状态（队列、本地意图、上传续传）四类端口；端口之间
//!   没有相互依赖，任何端口都可以独立构造（见 `ports` 模块文档的依赖表）。
//! - [`executor`]：确定性执行器。队列顺序是插入顺序（不使用客户端时钟）；动作先写意图再执行；
//!   失败动作及其依赖都保留在队列里（I02、I06、I13）；可重试错误的尝试次数有界（§13.3）；
//!   暂停只停止启动新动作，不丢队列。
//! - [`adapters`]：`crates/client-store` 上的状态端口实现（队列、意图、上传续传、下载进度）。
//!
//! # 明确不做
//!
//! 上传快照与 ACK 代际核对由 [`executor::upload`] 提供，事件拉取与应用游标在 [`engine::events`]，
//! 受控内容应用与重启核对在 [`executor::apply`] / [`recovery`]。下载优先级、有界资源槽和持久 jitter 退避由 [`engine::scheduler`] 提供。
//! 平台/HTTP/SQLite 适配器仍由应用入口组装；本地移动、释放及按需占位的原生实现不在此包。
#![forbid(unsafe_code)]

pub mod adapters;
pub mod bindings;
pub mod engine;
pub mod executor;
pub mod ports;
pub mod recovery;

#[cfg(test)]
mod testing;

pub mod status;
