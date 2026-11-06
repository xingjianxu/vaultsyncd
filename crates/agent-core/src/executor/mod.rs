//! 确定性操作执行器（design §5.4、§8.4、§13.3、I02、I06、I13）。
//!
//! 执行器只做四件事，顺序固定：
//!
//! 1. 按**插入顺序**读出该绑定的待执行队列（不使用客户端时钟决定先后）；
//! 2. 按 [`crate::ports::state::QueuedOperation::depends_on`] 判定就绪；依赖未完成的操作不启动，但也不会被删除；
//! 3. 对就绪操作先落**本地写入意图**（[`crate::ports::state::IntentStore::begin_intent`]），再交给 [`ActionHandler`] 执行；
//! 4. 按结果出队或记录失败：可重试错误累加尝试次数并退避，次数用尽或遇到不可自动恢复的错误时
//!    停止该操作，**动作与依赖方都保留在队列里**，等重新规划或人工处理。
//!
//! 这里不实现上传、下载与本地替换本身（S056/S059）：执行器定义它们必须遵守的顺序与失败语义，
//! [`ActionHandler`] 是它们接入的缝。这样“先意图后动作”“失败保留”“重试有界”都只有一处实现，
//! 不会在每个动作里各写一遍。
//!
//! # 不变量
//!
//! - **失败动作及其依赖保留**：执行器从不删除失败操作；依赖方因为前置未完成而停在队列里，
//!   报告中给出 `DependencyIncomplete`，不是“悄悄跳过”（I02、I13）。
//! - **重试有界**：每个操作最多执行 [`RetryPolicy::max_attempts`] 次；`attempts` 由队列持久化，
//!   跨重启累计。用尽后不再自动重试，只保留错误码。
//! - **pending 不是 committed**：处理器报告 [`ActionOutcome::Pending`] 时操作不出队（I01）。
//! - **暂停不丢队列**：暂停只停止启动新动作；已在队列里的操作、依赖与意图都原样保留。
//! - **同一操作同一 `OpId`**：重试不换 `OpId`（I06）；执行器只读队列，不重新生成身份。
//! - **未知种类不猜**：`kind` 不是已定义的 [`OperationKind`] 时阻塞该操作并保留现场，不当作成功。

pub mod retry;
mod run;
pub mod upload;

use std::fmt;

use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_planner::reconcile::Action;

use crate::ports::state::{ActionIntent, LocalWriteKind};
use crate::ports::{PortFuture, Ports};

pub use retry::{RetryPolicy, RetryPolicyError};
pub use run::{BlockReason, OpOutcome, OpReport, RunReport, RunRequest};
pub use upload::{
    UploadActionHandler, UploadCommit, UploadCommitDisposition, UploadCommitStore, UploadSpec,
};

/// 执行器公开的单个操作报告（`OpReport` 的便捷访问）。
pub use run::Executor;
pub use run::ExecutorError;

/// 待执行操作的种类（`pending_ops.kind`；取值集合由本卡定义，见 design §7.3 的说明）。
///
/// 名称与规划器的 [`Action`] 一一对应，不另造一套动作词表。`NoOp`/`Defer`/`PreserveBoth` 没有可
/// 执行的远端或本地动作，因此不映射成操作种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    /// 上传本地内容（已有条目的 `put_content`）。
    UploadContent,
    /// 首次合并时在服务器新建条目（`create`）。
    UploadNewEntry,
    /// 同 Root 移动（`move`，内容不变）。
    UploadMove,
    /// 同 Root 移动并上传该代际的新内容。
    UploadMoveWithContent,
    /// 迟到分支成为新冲突条目（`create`）。
    CreateConflictCopy,
    /// 应用远端内容变化（下载并安装固定版本）。
    ApplyRemote,
    /// 应用远端已提交的移动。
    ApplyRemoteMove,
    /// 应用远端已提交的删除。
    ApplyRemoteDelete,
    /// 首次合并时在本地建立远端独有条目。
    CreateFromRemote,
    /// 提交本地删除。
    DeleteEntry,
    /// 首次合并时绑定远端身份并写基线（不传内容）。
    BindRemoteEntry,
    /// 双方已证明同一结果：只推进基线。
    MergeBaseline,
    /// 用户或策略请求释放本地完整内容。
    ReleaseLocal,
}

impl OperationKind {
    /// 数据库里的规范文本。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UploadContent => "upload_content",
            Self::UploadNewEntry => "upload_new_entry",
            Self::UploadMove => "upload_move",
            Self::UploadMoveWithContent => "upload_move_with_content",
            Self::CreateConflictCopy => "create_conflict_copy",
            Self::ApplyRemote => "apply_remote",
            Self::ApplyRemoteMove => "apply_remote_move",
            Self::ApplyRemoteDelete => "apply_remote_delete",
            Self::CreateFromRemote => "create_from_remote",
            Self::DeleteEntry => "delete_entry",
            Self::BindRemoteEntry => "bind_remote_entry",
            Self::MergeBaseline => "merge_baseline",
            Self::ReleaseLocal => "release_local",
        }
    }

    /// 解析队列里的 `kind` 文本；未知文本返回 `None`，由执行器阻塞该操作而不是猜种类。
    #[must_use]
    pub fn from_db(text: &str) -> Option<Self> {
        Some(match text {
            "upload_content" => Self::UploadContent,
            "upload_new_entry" => Self::UploadNewEntry,
            "upload_move" => Self::UploadMove,
            "upload_move_with_content" => Self::UploadMoveWithContent,
            "create_conflict_copy" => Self::CreateConflictCopy,
            "apply_remote" => Self::ApplyRemote,
            "apply_remote_move" => Self::ApplyRemoteMove,
            "apply_remote_delete" => Self::ApplyRemoteDelete,
            "create_from_remote" => Self::CreateFromRemote,
            "delete_entry" => Self::DeleteEntry,
            "bind_remote_entry" => Self::BindRemoteEntry,
            "merge_baseline" => Self::MergeBaseline,
            "release_local" => Self::ReleaseLocal,
            _ => return None,
        })
    }

    /// 规划器动作对应的操作种类；没有可执行动作时返回 `None`。
    #[must_use]
    pub fn from_action(action: &Action) -> Option<Self> {
        Some(match action {
            Action::NoOp | Action::Defer(_) | Action::PreserveBoth(_) => return None,
            Action::ApplyRemote { .. } => Self::ApplyRemote,
            Action::UploadLocal { .. } => Self::UploadContent,
            Action::MergeSameResult { .. } => Self::MergeBaseline,
            Action::CreateConflictCopy { .. } => Self::CreateConflictCopy,
            Action::ApplyRemoteMove { .. } => Self::ApplyRemoteMove,
            Action::UploadMove { .. } => Self::UploadMove,
            Action::UploadMoveWithContent { .. } => Self::UploadMoveWithContent,
            Action::ApplyRemoteDelete { .. } => Self::ApplyRemoteDelete,
            Action::DeleteEntry { .. } => Self::DeleteEntry,
            Action::UploadNewEntry { .. } => Self::UploadNewEntry,
            Action::CreateFromRemote { .. } => Self::CreateFromRemote,
            Action::BindRemoteEntry { .. } => Self::BindRemoteEntry,
        })
    }

    /// 执行这个操作之前必须持久化的本地写入意图种类；不动本地内容时为 `None`。
    ///
    /// 远端删除没有对应的 `local_intents` 种类（design §7.3 只定义 replace/move/release/import）：
    /// 它的可恢复性由队列里的操作与远端事实承担，本地删除仍要求现场代际相符。
    #[must_use]
    pub const fn local_write(self) -> Option<LocalWriteKind> {
        match self {
            Self::ApplyRemote | Self::CreateFromRemote => Some(LocalWriteKind::Replace),
            Self::ApplyRemoteMove => Some(LocalWriteKind::Move),
            Self::ReleaseLocal => Some(LocalWriteKind::Release),
            Self::UploadContent
            | Self::UploadNewEntry
            | Self::UploadMove
            | Self::UploadMoveWithContent
            | Self::CreateConflictCopy
            | Self::ApplyRemoteDelete
            | Self::DeleteEntry
            | Self::BindRemoteEntry
            | Self::MergeBaseline => None,
        }
    }
}

/// 一次动作成功走到了哪一步。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionOutcome {
    /// 服务器已确认 committed（或本地基线已落库）：可以出队。
    Completed,
    /// 服务器已受理但尚未提交（`pending`）：不出队，也不是已同步（I01）。
    Pending {
        /// 服务端建议的下次查询间隔；`None` 表示沿用默认轮询间隔。
        poll_after: Option<std::time::Duration>,
    },
}

/// 动作失败：错误码决定能否自动重试（[`ErrorCode::class`]），详情只用于诊断。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionFailure {
    /// design §15.4 的错误码（决定处理类别）。
    pub code: ErrorCode,
    /// 面向诊断的原因；按数据处理，不执行其中内容。
    pub detail: String,
}

impl ActionFailure {
    /// 构造一次失败。
    #[must_use]
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for ActionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "动作失败（{}）：{}", self.code, self.detail)
    }
}

impl std::error::Error for ActionFailure {}

/// 单个操作的执行器（S056 上传、S059 本地应用接入的缝）。
///
/// 两个方法的顺序由执行器保证：`plan_intent` → 持久化 → `execute`。处理器不得在 `plan_intent`
/// 里改动本地内容，也不得跳过执行器自己写意图。
pub trait ActionHandler: Send + Sync {
    /// 在执行前给出要持久化的本地写入意图；不动本地内容时返回 `None`。
    fn plan_intent<'a>(
        &'a self,
        op: &'a crate::ports::state::QueuedOperation,
    ) -> PortFuture<'a, Result<Option<ActionIntent>, ActionFailure>>;

    /// 执行一个操作。`intent` 是已经落库的意图（序号在 [`ActionIntent::op`] 对应操作上）。
    fn execute<'a>(
        &'a self,
        ports: &'a Ports,
        op: &'a crate::ports::state::QueuedOperation,
        intent: Option<&'a ActionIntent>,
    ) -> PortFuture<'a, Result<ActionOutcome, ActionFailure>>;
}

/// 暂停原因：暂停只停止启动新动作，不删除队列、不改操作状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseReason {
    /// 用户请求暂停该绑定。
    UserRequested,
    /// 当前断网；普通操作静默等待，不进入问题页（design §18.2）。
    Offline,
    /// Root 挂载不可用；暂停该 Root 并保留队列（design §12.3）。
    RootUnavailable,
    /// 绑定被停用（模式切换、解除绑定准备中）。
    BindingDisabled,
}

impl PauseReason {
    /// 稳定短标签，供日志与状态展示。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserRequested => "user_requested",
            Self::Offline => "offline",
            Self::RootUnavailable => "root_unavailable",
            Self::BindingDisabled => "binding_disabled",
        }
    }
}

impl fmt::Display for PauseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use vaultsyncd_domain::ids::{EntryId, OpId};
    use vaultsyncd_domain::version::{Generation, Revision};
    use vaultsyncd_fs_common::path::RelativePath;
    use vaultsyncd_planner::first_sync::{BindingMode, PreserveReason};
    use vaultsyncd_planner::reconcile::DeferReason;

    fn generation() -> Generation {
        Generation::new(3).expect("代际")
    }

    #[test]
    fn every_planner_action_has_one_operation_kind_or_none() {
        let entry = EntryId::from_bytes([1; 16]);
        let revision = Revision::new(4).expect("修订");
        let target = RelativePath::parse("docs/report.txt").expect("路径");
        let actions = [
            (Action::NoOp, None),
            (Action::Defer(DeferReason::FirstSync), None),
            (Action::PreserveBoth(PreserveReason::ContentDiffers), None),
            (
                Action::ApplyRemote {
                    revision,
                    version: None,
                },
                Some(OperationKind::ApplyRemote),
            ),
            (
                Action::UploadLocal {
                    base_revision: revision,
                    generation: generation(),
                },
                Some(OperationKind::UploadContent),
            ),
            (
                Action::MergeSameResult {
                    revision,
                    version: None,
                },
                Some(OperationKind::MergeBaseline),
            ),
            (
                Action::CreateConflictCopy {
                    entry_id: entry,
                    op_id: OpId::from_bytes([2; 16]),
                    generation: generation(),
                },
                Some(OperationKind::CreateConflictCopy),
            ),
            (
                Action::ApplyRemoteMove {
                    entry_id: entry,
                    revision,
                    version: None,
                    target: target.clone(),
                },
                Some(OperationKind::ApplyRemoteMove),
            ),
            (
                Action::UploadMove {
                    entry_id: entry,
                    base_revision: revision,
                    target: target.clone(),
                },
                Some(OperationKind::UploadMove),
            ),
            (
                Action::UploadMoveWithContent {
                    entry_id: entry,
                    base_revision: revision,
                    target,
                    generation: generation(),
                },
                Some(OperationKind::UploadMoveWithContent),
            ),
            (
                Action::ApplyRemoteDelete {
                    entry_id: entry,
                    revision,
                },
                Some(OperationKind::ApplyRemoteDelete),
            ),
            (
                Action::DeleteEntry {
                    entry_id: entry,
                    base_revision: revision,
                    subtree: None,
                },
                Some(OperationKind::DeleteEntry),
            ),
            (
                Action::UploadNewEntry {
                    generation: generation(),
                },
                Some(OperationKind::UploadNewEntry),
            ),
            (
                Action::CreateFromRemote {
                    entry_id: entry,
                    revision,
                    version: None,
                    mode: BindingMode::Full,
                },
                Some(OperationKind::CreateFromRemote),
            ),
            (
                Action::BindRemoteEntry {
                    entry_id: entry,
                    revision,
                    version: None,
                },
                Some(OperationKind::BindRemoteEntry),
            ),
        ];
        for (action, expected) in actions {
            assert_eq!(
                OperationKind::from_action(&action),
                expected,
                "动作 {action:?} 的种类映射"
            );
        }
    }

    #[test]
    fn kind_text_round_trips_and_unknown_text_is_not_guessed() {
        let all = [
            OperationKind::UploadContent,
            OperationKind::UploadNewEntry,
            OperationKind::UploadMove,
            OperationKind::UploadMoveWithContent,
            OperationKind::CreateConflictCopy,
            OperationKind::ApplyRemote,
            OperationKind::ApplyRemoteMove,
            OperationKind::ApplyRemoteDelete,
            OperationKind::CreateFromRemote,
            OperationKind::DeleteEntry,
            OperationKind::BindRemoteEntry,
            OperationKind::MergeBaseline,
            OperationKind::ReleaseLocal,
        ];
        for kind in all {
            assert_eq!(OperationKind::from_db(kind.as_str()), Some(kind));
        }
        assert_eq!(OperationKind::from_db("delete_everything"), None);
        assert_eq!(OperationKind::from_db(""), None);
    }

    #[test]
    fn only_local_mutations_need_an_intent() {
        for kind in [
            OperationKind::ApplyRemote,
            OperationKind::CreateFromRemote,
            OperationKind::ApplyRemoteMove,
            OperationKind::ReleaseLocal,
        ] {
            assert!(kind.local_write().is_some(), "{kind:?} 会改本地内容");
        }
        for kind in [
            OperationKind::UploadContent,
            OperationKind::UploadNewEntry,
            OperationKind::UploadMove,
            OperationKind::UploadMoveWithContent,
            OperationKind::CreateConflictCopy,
            OperationKind::ApplyRemoteDelete,
            OperationKind::DeleteEntry,
            OperationKind::BindRemoteEntry,
            OperationKind::MergeBaseline,
        ] {
            assert!(kind.local_write().is_none(), "{kind:?} 不应写本地意图");
        }
        assert_eq!(
            OperationKind::ApplyRemote.local_write(),
            Some(LocalWriteKind::Replace)
        );
        assert_eq!(
            OperationKind::ApplyRemoteMove.local_write(),
            Some(LocalWriteKind::Move)
        );
        assert_eq!(
            OperationKind::ReleaseLocal.local_write(),
            Some(LocalWriteKind::Release)
        );
    }
}
