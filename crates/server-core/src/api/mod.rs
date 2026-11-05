//! 远端 API 的用例规则（design §15.2）：把平台层给出的探测事实变成 HTTP 语义。
//!
//! 本模块只放判定与编码，不做 I/O：
//!
//! - [`roots`] 是 Root 注册：允许基目录的包含判定、候选目录事实到设计错误码的映射（design
//!   §11.4 的“配置阶段拒绝”）。
//!
//! HTTP 处理、挂载探测与数据库写入分别由 `apps/server`、`crates/platform-linux-server` 与
//! `crates/server-store` 负责；本模块不依赖它们（design §5.5）。

pub mod roots;
