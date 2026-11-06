//! 状态端口的现成适配器（design §5.5：适配器在应用入口组装，核心只认端口）。
//!
//! 目前只有一个：[`client_store`] 把 `crates/client-store` 的 SQLite 读写接到
//! [`crate::ports::state`] 的队列/意图/上传续传端口，以及 `crates/transfer` 的下载进度端口。
//! 本地文件端口由平台层实现（`crates/platform-windows`，S059 起使用），远端端口由
//! `crates/http-client` 的端点在 `apps/agent` 里组装（S056 起使用）；本 crate 不直接依赖它们。

pub mod client_store;

pub use client_store::ClientStorePorts;
