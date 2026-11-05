//! 远端 API 的用例规则（design §5.4、§15.2）：把平台层给出的事实变成端点语义。
//!
//! 本层只放“端点该做什么”的规则与顺序：校验输入、决定状态、规定持久化顺序；wire 编解码在
//! `apps/server`，存取与字节动作走 [`crate::ports`] 的端口（适配器实现）。因此同一套规则可以在
//! 没有 HTTP、没有文件系统的便携检查里被测试。
//!
//! - [`roots`] 是 Root 注册：允许基目录的包含判定、候选目录事实到设计错误码的映射（design
//!   §11.4 的“配置阶段拒绝”）；
//! - [`uploads`] 是上传会话与固定块接收：总大小/块大小边界、接收顺序（校验 → 落盘 → 登记）与
//!   同索引重传语义（design §13.1—13.2、I05）；
//! - [`content`] 是上传 finalize 与固定版本下载：拼装发布只产出可提交引用，下载按
//!   `(root, entry, version)` 与 Range 取字节并在读取期间持有下载租约（design §13.2、I01、I12）。
//! - [`operations`] 是操作命令端点：put/create/move/delete/restore 的形状与前置、`(root, device,
//!   op_id)` 的幂等重放/协议错误判定、`pending` 不判已同步，以及 design §15.4 的错误码与状态码映射
//!   （design §15.2—15.4、I06）。
//! - [`events`] 与 [`snapshots`] 是一致性快照、`changes` 拉取与 SSE 提示：游标是否可服务（过期/超前
//!   一律 410 `CURSOR_EXPIRED`，不返回假空集）、事件保留窗口、分页上限，以及 SSE 只在头变化时提示
//!   （design §9.3—9.4、I09）。

pub mod content;
pub mod events;
pub mod operations;
pub mod roots;
pub mod snapshots;
pub mod uploads;
