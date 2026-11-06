//! 客户端端口契约（design §5.5「`agent-core` 依赖端口接口」、§14.1 能力抽象）。
//!
//! 端口定义在核心，适配器在别处实现：核心据端口报告的事实在自己这一侧判定“能不能执行、执行
//! 到什么程度”，不把该判定交给平台层，也不让平台层反向依赖核心的规则。
//!
//! # 端口依赖方向（无环）
//!
//! | 端口 | 依赖其它端口 | 说明 |
//! | --- | --- | --- |
//! | [`clock::Clock`] | 无 | 只读当前 UTC 秒；调度用它判断“退避是否到期” |
//! | [`local::LocalPort`] | 无 | 本地观察、固定快照、受控替换/移动/删除/释放 |
//! | [`remote::RemotePort`] | 无 | 提交带 `OpId` 的操作、按 `OpId` 查询结果、取传输端点 |
//! | [`state::OperationStore`] | 无 | 待执行队列的读写 |
//! | [`state::IntentStore`] | 无 | 本地写入意图的持久化 |
//! | [`state::UploadResumeStore`] | 无 | 上传快照与会话的续传记录 |
//!
//! 上表是**唯一**的端口依赖声明：没有任何端口以另一个端口为参数、字段或超 trait，端口只共享
//! [`PortError`]/[`PortFuture`] 这两个错误与 future 别名。因此每个端口都能独立构造，[`Ports`]
//! 只是把它们并在一起交给执行器；不可能出现“构造 A 需要 B、构造 B 又需要 A”的初始化环。
//!
//! 端口失败统一用 [`vaultsyncd_transfer::ports::PortError`] 分类（design §13.3）：调度器要能区分
//! “退避后原样重试”“需要重新登录”“服务端明确拒绝”“本地磁盘/权限问题”。本地 SQLite 的读写
//! 失败按 `Local{Write}` 归类——它同样属于“本机状态暂时不可用”，不是服务端结论。

pub mod clock;
pub mod local;
pub mod remote;
pub mod state;

use std::sync::Arc;

pub use vaultsyncd_transfer::ports::{LocalErrorKind, PortError, PortFuture};

use crate::ports::clock::Clock;
use crate::ports::local::LocalPort;
use crate::ports::remote::RemotePort;
use crate::ports::state::{IntentStore, OperationStore, UploadResumeStore};

/// 执行器与动作处理器共享的一组端口。
///
/// 字段是 `Arc<dyn …>` 而不是泛型：适配器由应用入口各自构造（HTTP、平台、SQLite），执行器不应
/// 为每个实现重新单态化一遍。所有端口都可以独立构造，这个结构体只负责组装，**不**规定构造顺序。
#[derive(Clone)]
pub struct Ports {
    /// 本地文件/平台能力。
    pub local: Arc<dyn LocalPort>,
    /// 时钟（退避到期判断）。
    pub clock: Arc<dyn Clock>,
    /// 远端操作与传输端点。
    pub remote: Arc<dyn RemotePort>,
    /// 待执行队列。
    pub operations: Arc<dyn OperationStore>,
    /// 本地写入意图。
    pub intents: Arc<dyn IntentStore>,
    /// 上传续传记录。
    pub upload_resumes: Arc<dyn UploadResumeStore>,
}

impl Ports {
    /// 组装全部端口。参数顺序与字段顺序一致，只是为了让调用点显式说出每个适配器是谁。
    #[must_use]
    pub fn new(
        local: Arc<dyn LocalPort>,
        clock: Arc<dyn Clock>,
        remote: Arc<dyn RemotePort>,
        operations: Arc<dyn OperationStore>,
        intents: Arc<dyn IntentStore>,
        upload_resumes: Arc<dyn UploadResumeStore>,
    ) -> Self {
        Self {
            local,
            clock,
            remote,
            operations,
            intents,
            upload_resumes,
        }
    }
}

impl std::fmt::Debug for Ports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ports").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    /// 端口模块的字面依赖检查：每个叶模块都不得提到另一个端口的 trait 名。
    ///
    /// 这是对模块文档里“端口之间无依赖”表的机器校验：如果有人在 `local` 里加上
    /// `RemotePort` 参数，这个测试会立刻失败，而不是等出现初始化环才发现。
    #[test]
    fn port_modules_do_not_reference_each_other() {
        let modules: [(&str, &str, &[&str]); 4] = [
            (
                "clock",
                include_str!("clock.rs"),
                &[
                    "LocalPort",
                    "RemotePort",
                    "OperationStore",
                    "IntentStore",
                    "UploadResumeStore",
                ],
            ),
            (
                "local",
                include_str!("local.rs"),
                &[
                    "Clock",
                    "RemotePort",
                    "OperationStore",
                    "IntentStore",
                    "UploadResumeStore",
                ],
            ),
            (
                "remote",
                include_str!("remote.rs"),
                &[
                    "Clock",
                    "LocalPort",
                    "OperationStore",
                    "IntentStore",
                    "UploadResumeStore",
                ],
            ),
            (
                "state",
                include_str!("state.rs"),
                &["Clock", "LocalPort", "RemotePort"],
            ),
        ];
        for (name, source, forbidden) in modules {
            for other in forbidden {
                assert!(
                    !source.contains(other),
                    "端口模块 {name} 不得引用另一个端口 {other}"
                );
            }
        }
    }

    /// 只有组装点 `Ports` 同时提到全部端口；组装不规定构造顺序。
    #[test]
    fn only_the_assembly_point_names_every_port() {
        let assembly = include_str!("mod.rs");
        for port in [
            "LocalPort",
            "Clock",
            "RemotePort",
            "OperationStore",
            "IntentStore",
            "UploadResumeStore",
        ] {
            assert!(assembly.contains(port), "组装点应包含 {port}");
        }
    }
}
