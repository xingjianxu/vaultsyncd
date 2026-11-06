//! 传输侧纯粹规则与执行：固定块几何、流式内容哈希、续传位图与上传/下载执行（design §5.4、§13）。
//!
//! 本 crate 被客户端（Windows agent 的上传快照与下载续传）和服务端（接收固定块）共用，**不做任何
//! I/O**：不读文件、不建连接、不碰数据库、不看时钟。这样两端对“块边界”“块摘要”“缺哪些块”的判断
//! 只能来自同一份代码，续传不会把两份内容拼在一起。
//!
//! - [`chunk`]：一份快照的总大小与块大小决定块数与每块长度（块几何的唯一权威）。
//! - [`hash`]：增量 SHA-256、摘要比较与十六进制编解码（整文件流式计算，不把内容整体读进内存）。
//! - [`bitmap`]：续传位图与缺失区间（第 i 位 = 第 i 块，低位在前，尾部 bit 必须为 0）。
//! - [`ports`]：传输执行依赖的端口（固定快照、上传会话、固定版本内容、临时文件、下载进度）。
//! - [`upload`]：固定快照 → 复核/申请会话 → 只补服务端缺失块 → finalize 校验（**不等于 committed**）。
//! - [`download`]：固定 VersionId → 检查空间 → 只补缺失块 → 整份摘要校验（安装由上层完成）。
//! - [`concurrency`]：取消开关与“单个文件内的有界块并发”。
//!
//! 真正的 HTTP、文件系统与 SQLite 适配器在别处：`crates/http-client` 的 `uploads`/`content` 实现
//! 上传会话与固定版本内容端口，`crates/agent-core` 实现本地临时文件与进度持久化。本 crate 因此
//! 可以在 Linux 与 Windows 之间共用，也可以用内存替身完整验证断线续传语义。
//!
//! # 明确不做
//!
//! 跨版本块增量（内容定义分块、旧版本块复用、增量专用 GC）不在首发范围（design §13.1）。续传位图
//! 只按同一次传输的有效性使用：内容摘要或块几何变了，旧进度作废。

#![forbid(unsafe_code)]

pub mod bitmap;
pub mod chunk;
pub mod concurrency;
pub mod download;
pub mod hash;
pub mod ports;
pub mod upload;

#[cfg(test)]
mod fakes;
mod stream;
