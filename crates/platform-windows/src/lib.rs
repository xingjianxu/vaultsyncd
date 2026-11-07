//! vaultsyncd 的 Windows 平台适配层（`docs/design.md` §5.3 的 `cfapi/`、`shell/`、`files/`、
//! `watcher/`、`ipc/`）。
//!
//! 已落地 `cfapi/` 的同步根注册、连接、占位创建、状态查询、`FileIdentity` 编码（S010）
//! 与内容水合读取、范围供给、取消、超时/离线失败语义（S011）、释放/写保护（S012）；
//! S065 增加 `files/` 的句柄相对安全访问/元数据扫描及 `watcher/` 的原生通知与补扫围栏。
//! S071 增加 `ipc/` 的本机同用户受限 named pipe 传输；不初始化 CFAPI。
//!
//! 边界（design §14.2）：
//!
//! - 只用 CFAPI 认可的调用序列修改占位与状态，不手改 reparse point、不截断文件伪造脱水。
//! - `FileIdentity` 只承载 schema_version、RootId、EntryId 与版本提示；不放密码、访问令牌、
//!   服务器地址或绝对路径，也不作为业务真相（业务基线仍由核心数据库持有）。
//! - 平台 I/O 只在本层出现：纯逻辑（identity 编解码、原型固定布局、生命周期记录格式）不依赖
//!   Windows，可在宿主 `cargo test -p vaultsyncd-platform-windows` 直接验证。
//!
//! 本包不在 workspace `default-members` 中：Windows 目标代码只有交叉构建与客体运行才能验证，
//! 便携检查不构建它。

pub mod cfapi;
pub mod files;
pub mod watcher;

#[cfg(windows)]
pub mod single;

#[cfg(windows)]
pub mod shell;

#[cfg(windows)]
pub mod ipc;
