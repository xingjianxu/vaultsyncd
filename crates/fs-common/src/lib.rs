//! vaultsyncd 与平台无关的路径与名称规则（`docs/design.md` §5.4、§16.1、§16.2）。
//!
//! 职责边界：本 crate 只做纯判定与纯分类，不做 I/O。它不打开文件、不读挂载信息、不调用
//! `canonicalize`，也不取平台 API（系统调用与原生名称语义留在 `crates/platform-*`）；
//! 因此同一输入在任何平台得到相同结论，可被 planner、agent-core、server-core 与两端平台层共用。
//!
//! 规则立场（design §16.1、§16.2）：跨端传输的名称取 Linux 与 Windows 可表示性的交集，
//! 任一侧无法无损表示就报出明确原因，**不自动小写、不做 Unicode 归一化改名、不静默替换字符**；
//! 冲突只报告，由用户或上层决定改名。名称“是否指向同一条目”的原生语义经
//! [`name::NameEquivalence`] 端口注入，默认实现只判定能确定性证明的等价关系。
//!
//! 已落地模块（S017）：[`path`]（相对路径分量校验与按分量的重叠判定）、[`name`]（单个名称
//! 分量的跨平台问题分类、保留名与等价/碰撞判定）。排除规则（`ignore.rs`）与文件事实类型
//! 由后续卡补充，本卡不预置占位 API。
#![forbid(unsafe_code)]

pub mod name;
pub mod path;
