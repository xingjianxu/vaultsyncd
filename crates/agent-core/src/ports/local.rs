//! 本地文件/平台端口（design §5.5、§14.1）。
//!
//! 核心只通过本端口观察与改动本地内容：**能力**（能否按需读取、释放、使用保护、稳定身份、原子
//! 替换）、**事实**（条目种类、本地代际、文件身份、使用状态）、**固定快照**（上传读取的只读视图）
//! 与**受控改动**（替换、移动、删除、释放）。平台相关的一切——CFAPI 占位符、Windows 句柄、
//! 路径规范化、权限——都在适配器里，核心不假定能力一致（design §14.1“各客户端平台向核心报告实际
//! 能力，而不是用同一函数名假定能力一致”）。
//!
//! # 不变量
//!
//! - **观测不等于承诺**：`observe` 给出的 clean/generation 只是上次观察；执行器替换或删除前必须
//!   用同一 `expected_generation` 再核对一次（design §8.4）。
//! - **快照是固定的**：`snapshot` 返回的 [`SnapshotSource`] 只按固定内容回答，读取前后不得被外部
//!   修改影响；代际不符要报 `LOCAL_CHANGED`，不能“边读边变”（design §13.1）。
//! - **条件不匹配就停**：替换/移动/删除/释放的期望代际不符时返回 `LOCAL_CHANGED`，不得强行继续
//!   （design §14.1 的 `ApplyReceipt` 约定）。本地内容与意图内容不一致时保留现场，由上层重新规划。
//! - **删除与释放只动本地副本**：`remove` 只删除本地文件，不删除远端条目；`release` 只清可重建的
//!   完整缓存，且调用方必须已经确认服务器版本仍可读（design I11、§8.4）。
//!
//! 端口不做哈希、不算块、不写数据库：整份摘要属于 [`vaultsyncd_transfer`]，意图与队列属于
//! [`crate::ports::state`]。

use std::sync::Arc;

use vaultsyncd_domain::ids::{BindingId, EntryId, OpId, VersionId};
use vaultsyncd_domain::state::Usage;
use vaultsyncd_domain::version::Generation;
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_transfer::hash::DIGEST_BYTES;
use vaultsyncd_transfer::ports::{PortError, PortFuture, SnapshotSource};

/// 本地条目的种类（与 design §7.1 的文件/目录二分一致）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalEntryKind {
    /// 普通文件。
    File,
    /// 目录。
    Directory,
}

/// 平台报告的实际能力（design §14.1）。
///
/// 能力是**事实声明**，不是“支持就悄悄做、不支持就返回成功”的开关：执行器据 `on_demand_read`
/// 决定占位符能否按需读取，据 `usage_protection` 决定替换前能否确认没有写入句柄。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalCapabilities {
    /// 支持按需读取（占位符只在打开时取内容）。
    pub on_demand_read: bool,
    /// 支持脱水（把完整内容换回占位符而不删条目）。
    pub dehydrate: bool,
    /// 支持使用保护探测（读写句柄、映射写入）。
    pub usage_protection: bool,
    /// 文件身份跨重启稳定。
    pub stable_identity: bool,
    /// 支持原子替换（同目录临时文件 + rename）。
    pub atomic_replace: bool,
    /// 报告平台名称约束（保留名、大小写、长度）。
    pub name_constraints: bool,
}

impl LocalCapabilities {
    /// 全部能力都具备（首发 Windows CFAPI 适配器的目标形态）。
    #[must_use]
    pub const fn full() -> Self {
        Self {
            on_demand_read: true,
            dehydrate: true,
            usage_protection: true,
            stable_identity: true,
            atomic_replace: true,
            name_constraints: true,
        }
    }

    /// 全部能力都不具备（纯元数据平台；执行器必须先确认再动作）。
    #[must_use]
    pub const fn none() -> Self {
        Self {
            on_demand_read: false,
            dehydrate: false,
            usage_protection: false,
            stable_identity: false,
            atomic_replace: false,
            name_constraints: false,
        }
    }
}

/// 一次本地观察的事实。
///
/// `generation` 是本地内容代际（design §8.3）：路径与身份不变、内容可能变化时由它区分。`size` 与
/// `file_identity` 是平台报告的事实，核心不用它们决定内容是否相同——内容相同只能由摘要证明。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalFacts {
    /// 相对绑定根的路径。
    pub path: RelativePath,
    /// 条目种类。
    pub kind: LocalEntryKind,
    /// 观察到的本地代际。
    pub generation: Generation,
    /// 内容字节数（目录为 0）。
    pub size: u64,
    /// 平台文件身份（稳定标识，用于移动识别与恢复核对）。
    pub file_identity: Option<Vec<u8>>,
    /// 使用状态（读写句柄是否占用）。
    pub usage: Usage,
}

/// 受控替换（下载内容安装、远端内容首次落盘）。
///
/// 调用方必须先持久化 [`crate::ports::state::ActionIntent`]，把它的 `intent` 序号带进来：平台层
/// 不需要读数据库，但恢复流程需要知道“这次改动对应哪条意图”。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyRequest {
    /// 触发这次改动的操作身份（幂等与诊断）。
    pub op: OpId,
    /// 已持久化的本地意图序号。
    pub intent: i64,
    /// 所属绑定。
    pub binding: BindingId,
    /// 目标条目；本地新建在提交前可以没有服务器身份。
    pub entry: Option<EntryId>,
    /// 替换目标（相对绑定根）。
    pub target: RelativePath,
    /// 暂存内容的位置（相对绑定根或平台内部的暂存路径）；目录创建时为 `None`。
    pub staged: Option<String>,
    /// 要安装的远端内容版本。
    pub version: Option<VersionId>,
    /// 期望的整份内容摘要；目录创建时为 `None`。
    pub expected_digest: Option<[u8; DIGEST_BYTES]>,
    /// 替换前观察到的本地代际；不符即 `LOCAL_CHANGED`。
    pub expected_generation: Generation,
}

/// 同 Root 内的本地移动（应用远端已提交的移动）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRequest {
    /// 触发这次改动的操作身份。
    pub op: OpId,
    /// 已持久化的本地意图序号。
    pub intent: i64,
    /// 所属绑定。
    pub binding: BindingId,
    /// 条目身份：路径改变不影响它（I03）。
    pub entry: EntryId,
    /// 当前位置。
    pub from: RelativePath,
    /// 目标位置。
    pub to: RelativePath,
    /// 移动前观察到的本地代际；不符即 `LOCAL_CHANGED`。
    pub expected_generation: Generation,
}

/// 本地删除（应用远端已提交的删除）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoveRequest {
    /// 触发这次改动的操作身份。
    pub op: OpId,
    /// 已持久化的本地意图序号；删除没有 `local_intents` 种类，因此可为 `None`。
    pub intent: Option<i64>,
    /// 所属绑定。
    pub binding: BindingId,
    /// 被删除的条目身份。
    pub entry: EntryId,
    /// 目标位置。
    pub target: RelativePath,
    /// 删除前观察到的本地代际；不符即 `LOCAL_CHANGED`。
    pub expected_generation: Generation,
    /// 删除依据的远端版本；固定它可避免“删掉的是之后新建的内容”。
    pub expected_version: Option<VersionId>,
}

/// 释放本地完整内容（保留条目与元数据）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseRequest {
    /// 触发这次改动的操作身份。
    pub op: OpId,
    /// 已持久化的本地意图序号。
    pub intent: i64,
    /// 所属绑定。
    pub binding: BindingId,
    /// 条目身份。
    pub entry: EntryId,
    /// 目标位置。
    pub target: RelativePath,
    /// 释放前观察到的本地代际；不符即 `LOCAL_CHANGED`。
    pub expected_generation: Generation,
    /// 释放后仍可重新读取的服务器版本（调用方必须已确认它仍可读）。
    pub pinned_version: VersionId,
}

/// 受控改动的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReceipt {
    /// 实际结果阶段。
    pub stage: ApplyStage,
    /// 改动后的本地代际（新内容或新位置）。
    pub generation: Generation,
    /// 改动后的文件身份（平台能给出时）。
    pub file_identity: Option<Vec<u8>>,
}

/// 改动实际走到了哪一步。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyStage {
    /// 已替换/移动/删除/释放。
    Applied,
    /// 现场已经等于目标（重复应用是幂等的），没有再次改动。
    Unchanged,
}

/// 本地文件/平台端口。
pub trait LocalPort: Send + Sync {
    /// 平台实际能力。
    fn capabilities(&self) -> LocalCapabilities;

    /// 观察一个路径的事实；不存在返回 `Local{Missing}`，不把故障报告成“不存在”。
    fn observe(&self, path: &RelativePath) -> PortFuture<'_, Result<LocalFacts, PortError>>;

    /// 固定一份只读快照供上传使用。
    ///
    /// 适配器必须核对观察到的代际仍是 `generation`（否则 `LOCAL_CHANGED`），并保证之后按快照
    /// 内容回答：上传读取的是固定内容，不是“边读边被外部修改的当前路径”。
    fn snapshot(
        &self,
        path: &RelativePath,
        generation: Generation,
    ) -> PortFuture<'_, Result<Arc<dyn SnapshotSource>, PortError>>;

    /// 受控替换/安装。
    fn apply(&self, request: &ApplyRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>>;

    /// 同 Root 移动。
    fn move_entry(&self, request: &MoveRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>>;

    /// 删除本地条目（远端已提交删除后）。
    fn remove(&self, request: &RemoveRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>>;

    /// 释放本地完整内容；服务器版本仍可读由调用方保证。
    fn release(&self, request: &ReleaseRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>>;
}
