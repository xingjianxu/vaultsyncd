//! vaultsyncd 的 Windows 平台适配层（`docs/design.md` §5.3 的 `cfapi/`、`shell/`、`files/`、
//! `watcher/`、`ipc/`）。
//!
//! 本卡（S010）只落地 `cfapi/` 中 CFAPI 同步根注册、连接、占位创建、状态查询、清理与
//! `FileIdentity` 编码所需的部分；`shell/`、`files/`、`watcher/`、`ipc/` 与内容水合
//! （S011）、释放与写保护（S012）仍属后续卡，这里没有占位实现。
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
