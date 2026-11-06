//! 本机 IPC 的 wire 契约与消息帧（`docs/design.md` §5.6、§15.5）。
//!
//! 边界：
//!
//! - [`generated`] 是**生成代码**，唯一编辑源是 `api/local-ipc.schema.json`，改契约后运行
//!   `cargo xtask generate`。本 crate 不手改生成文件，也不在生成文件里加业务逻辑。
//! - [`version`] 是协议版本判定：major 不同即不兼容，minor 高于 agent 支持的版本同样拒绝。
//! - [`framing`] 是消息帧的编解码：大端长度头加 JSON 负载，声明长度有上限，部分读取只在凑满
//!   整帧后产出，缓冲区随帧数封顶。
//! - [`service`] 是命令处理器的最小骨架：目前只实现 `get_status`，GUI 与 CLI 走同一条路径。
//!
//! 本 crate 是纯数据层：不做 I/O、不读时钟、不取随机数，也不包含 Windows 命名管道、ACL 或
//! 调用者身份校验——那些属于平台传输实现（design §5.3、S071），因此同一份帧与 DTO 定义可以
//! 在任何宿主上编译与测试。契约与服务器管理 API 分开：本机 IPC 不经网络传输，也不复用
//! `api/openapi.yaml`。

#![forbid(unsafe_code)]

pub mod commands;
pub mod framing;
pub mod generated;
pub mod service;
pub mod version;
