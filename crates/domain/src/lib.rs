//! vaultsyncd 的领域内核：与平台无关的身份、条目、版本、操作、错误与业务不变量。
//!
//! 职责边界（`docs/design.md` §5.4）：本 crate 只承载纯数据与纯规则，供 planner、agent-core、
//! server-core、客户端/服务端 store 与平台适配器共享。它不做 I/O：不打开文件与网络、不读时钟、
//! 不取随机数、不访问数据库，也不依赖任何平台 API；时间戳、随机源、内容哈希等外部事实由调用方
//! 以值或端口传入，因此同一输入在任何平台必须得到相同结果（design §6 的确定性要求）。
//!
//! 需要对外传递的边界值（身份、计数、状态）在这里以受约束的类型表达，而不是裸整数或字符串，
//! 避免把非法组合带到持久化与 wire 层。已落地的模块：S015 的 [`ids`]（不透明身份与 [`ids::IdSource`]
//! 端口）与 [`version`]（`Revision`/`VaultSeq`/`Generation`/`ServerEpoch` 有界计数、UTC 时间与
//! [`version::Clock`] 端口）；S016 的 [`state`]（六个正交状态维度、界面主状态投影与“已同步”判断）
//! 与 [`error`]（design §15.4 错误码的三个处理类别）。
//!
//! [`state`] 与 [`error`] 的接缝：任务失败/等待重试时携带 [`error::ErrorCode`]，投影只按类别判断
//! 是否属于“严重失败”，不解释错误文本，也不做重试调度（design §8.2、§13.3、§18.2）。
#![forbid(unsafe_code)]

pub mod error;
pub mod ids;
pub mod state;
pub mod version;
