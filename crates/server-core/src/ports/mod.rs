//! 服务端端口契约（design §5.4、§5.5）。
//!
//! 端口定义在核心，平台适配层只负责实现：核心据端口报告的事实在自己这一侧判定“能否成为
//! 提交引用”，不把该判定交给平台层，也不让平台层反向依赖核心的规则。

pub mod conflict_store;
pub mod intent_log;
pub mod namespace;
pub mod namespace_store;
pub mod object_store;
pub mod put_store;
pub mod target;
