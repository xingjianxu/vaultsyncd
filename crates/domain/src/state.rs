//! 条目状态的六个正交维度与纯投影（`docs/design.md` §8.1—8.3）。
//!
//! 同一文件可能“已下载、存在本地修改、远端又有更新”，所以状态不是一个枚举，而是六个互相独立
//! 的维度：驻留 [`Residency`]、本地变更 [`Dirty`]、远端应用 [`RemoteApply`]、任务 [`Transfer`]、
//! 使用保护 [`Usage`]、可用性 [`Availability`]。维度之间可以任意组合，例如
//! `dirty && offline && materialized` 必须能被表达，而不是被压成一个“冲突”标签。
//!
//! 界面看到的是投影结果 [`EntryState::project`]（design §8.2 的优先级）与事实判断
//! [`EntryState::is_synced`]。投影是纯函数：输入是维度值，输出是主状态；不读文件、不查数据库、
//! 不取时间。投影只做“显示什么”，不改变状态，也不代替执行器的重新校验。
//!
//! “已同步”不是第七个维度，而是一个可同时成立或否定的判断：本地基线与服务器已确认版本一致，
//! 且没有待处理的本地操作。`remote_apply = Pending`（远端有新版本尚未应用）或任一待上传的本地
//! 修改都使它为假；拿到上传字节或 `202 pending` 不能让它变真（design §8.2）。

use crate::error::ErrorCode;

/// 驻留：本地有多少内容（design §8.1）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Residency {
    /// 只有元数据占位符，本地没有内容。
    #[default]
    MetadataOnly,
    /// 内容部分在本地：内部下载中间态，**不代表**完整离线可用。
    Partial,
    /// 内容完整在本地，并已通过校验安装。
    Materialized,
}

/// 本地变更：本地内容相对已记录基线的状态（design §8.1 的 `dirty` 维度）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Dirty {
    /// 本地内容与基线一致（该标记只是上次观察的结果，不作为“文件未变”的证明）。
    #[default]
    Clean,
    /// 存在尚未上传的本地修改。
    Dirty,
    /// 正在为待上传内容生成快照；磁盘上的新修改仍属于后续 generation。
    Snapshotting,
    /// 已提交上传、等待服务器确认 committed；确认前不得当作已同步。
    AwaitingCommit,
}

/// 远端应用：服务器已确认版本在本地基线中的位置（design §8.1）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RemoteApply {
    /// 本地基线就是服务器已确认版本。
    #[default]
    Current,
    /// 服务器有新版本，尚未应用到本地。
    Pending,
    /// 服务器有新版本，但被尚未上传的本地修改挡住。
    BlockedByLocal,
    /// 双方都有新版本，已规划冲突、等待处理。
    Conflicted,
}

/// 任务：该条目当前同步任务的状态（design §8.1）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Transfer {
    /// 没有该条目的任务。
    #[default]
    Idle,
    /// 已排队，尚未开始。
    Queued,
    /// 正在传输、哈希或安装。
    Running,
    /// 上一次尝试失败，正在退避等待自动重试；携带可重试类别的错误码。
    RetryWait(ErrorCode),
    /// 已停止自动推进（需要用户处理、内容损坏或重试被放弃）；携带失败原因。
    Failed(ErrorCode),
}

impl Transfer {
    /// 携带的错误码：等待重试或失败时给出原因，其余状态为 `None`。
    pub const fn error_code(self) -> Option<ErrorCode> {
        match self {
            Self::RetryWait(code) | Self::Failed(code) => Some(code),
            Self::Idle | Self::Queued | Self::Running => None,
        }
    }
}

/// 使用保护：谁正在使用本地内容（design §8.1、§14.4）。
///
/// 该维度只影响破坏性操作（释放空间、替换内容）的准入，不改变主状态投影；它也不证明内容
/// 未被修改——[`Dirty::Clean`] 只是上次观察。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Usage {
    /// 没有观察到的句柄。
    #[default]
    Idle,
    /// 有读者持有句柄。
    ReadHandles,
    /// 有写者持有句柄。
    WriteHandles,
    /// 无法判断（探测失败或平台不支持）。
    Unknown,
}

/// 可用性：相关 Root 与服务器当前是否可达（design §8.1）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Availability {
    /// 在线。
    #[default]
    Online,
    /// 当前断网；普通操作静默重试，状态持续可见。
    Offline,
    /// Root 的挂载不可用（例如挂载身份改变）。
    MountFailed,
    /// 服务器报告该 Root 不可用（需恢复）。
    RootUnavailable,
}

/// 一个条目的全部状态维度（design §8.1）。
///
/// 字段是六个正交事实，可以任意组合；调用方更新时保持不变式：
///
/// - `remote_apply = BlockedByLocal` 只在 `dirty != Clean` 时成立（远端更新被本地未提交修改挡住）；
/// - `Transfer::RetryWait` 只携带 [`ErrorClass::Retryable`](crate::error::ErrorClass::Retryable)
///   的错误码，`Transfer::Failed` 表示已停止自动推进；
/// - `Dirty::AwaitingCommit` 的内容已 `Materialized`（提交的是完整内容）；
/// - `Usage` 与 `Availability` 不改变基线与 generation：离线不是“已修改”，写入句柄也不是。
///
/// 投影与“已同步”判断对不一致的组合保守处理（宁可显示同步中/失败，也不显示已同步）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntryState {
    /// 驻留。
    pub residency: Residency,
    /// 本地变更。
    pub dirty: Dirty,
    /// 远端应用。
    pub remote_apply: RemoteApply,
    /// 任务。
    pub transfer: Transfer,
    /// 使用保护。
    pub usage: Usage,
    /// 可用性。
    pub availability: Availability,
}

/// 界面主状态（design §8.2 的投影优先级，声明顺序即优先级，从高到低）。
///
/// 详情可以同时显示多个事实（例如“已下载 · 本地修改待上传 · 当前离线”），但主状态只有一个。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryStatus {
    /// 不可用：Root 挂载失败或服务器报告 Root 不可用。
    Unavailable,
    /// 严重失败：需用户处理的错误、内容损坏，或已停止自动推进的任务。
    Failed,
    /// 未处理冲突。
    Conflict,
    /// 待上传的本地修改（含快照中与等待服务器确认）。
    LocalChanges,
    /// 正在同步：排队、传输、等待重试，或远端更新尚未应用。
    Syncing,
    /// 已下载：本地内容完整且没有更严重的事实。
    Downloaded,
    /// 仅云端：本地只有元数据或未完成的部分内容，不能离线读取。
    CloudOnly,
}

impl EntryState {
    /// 主状态投影（design §8.2）：
    /// 严重失败/不可用 > 未处理冲突 > 待上传本地修改 > 正在同步 > 已下载 > 仅云端。
    ///
    /// 只有“没有失败、没有冲突、没有待上传修改、没有未应用远端更新、没有进行中的任务”时才落到
    /// [`EntryStatus::Downloaded`]/[`EntryStatus::CloudOnly`]；`Partial` 内容不能算已下载。
    pub fn project(&self) -> EntryStatus {
        if matches!(
            self.availability,
            Availability::MountFailed | Availability::RootUnavailable
        ) {
            return EntryStatus::Unavailable;
        }
        if self.is_severe_failure() {
            return EntryStatus::Failed;
        }
        if self.remote_apply == RemoteApply::Conflicted {
            return EntryStatus::Conflict;
        }
        if self.dirty != Dirty::Clean {
            return EntryStatus::LocalChanges;
        }
        if self.remote_apply != RemoteApply::Current || self.transfer != Transfer::Idle {
            return EntryStatus::Syncing;
        }
        match self.residency {
            Residency::Materialized => EntryStatus::Downloaded,
            Residency::MetadataOnly | Residency::Partial => EntryStatus::CloudOnly,
        }
    }

    /// “已同步”（design §8.2）：本地基线与服务器已确认版本一致，且没有待处理的本地操作。
    ///
    /// 成立条件是 `dirty = Clean`、`remote_apply = Current`，且任务没有失败/重试中的失败：
    ///
    /// - `remote_apply = Pending`（远端有新版本未应用）、`Conflicted`、`BlockedByLocal` 都为假；
    /// - 任一未上传本地修改（`Dirty`/`Snapshotting`/`AwaitingCommit`）都为假；
    /// - 失败或等待重试的任务为假（内容可能不再可信）。
    ///
    /// 正在进行的下载/哈希任务（`Queued`/`Running`）不改变“基线一致”这一事实；`Residency` 与
    /// `Availability` 也不参与判断——离线不改变已确认的基线，仅云端条目的元数据基线同样可以一致。
    /// 界面要展示“当前离线”“仅云端”时应读取对应维度，而不是把这个判断当作下载状态。
    pub fn is_synced(&self) -> bool {
        self.dirty == Dirty::Clean
            && self.remote_apply == RemoteApply::Current
            && matches!(
                self.transfer,
                Transfer::Idle | Transfer::Queued | Transfer::Running
            )
    }

    /// 是否按“严重失败”显示：任务已停止自动推进，或重试队列里出现了本不该重试的错误类别。
    ///
    /// `RetryWait` 携带严重错误属于不变式被破坏；这里按失败显示，避免把损坏隐藏在“同步中”。
    fn is_severe_failure(&self) -> bool {
        match self.transfer {
            Transfer::Failed(_) => true,
            Transfer::RetryWait(code) => code.class().is_severe(),
            Transfer::Idle | Transfer::Queued | Transfer::Running => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Availability, Dirty, EntryState, EntryStatus, RemoteApply, Residency, Transfer, Usage,
    };
    use crate::error::ErrorCode;

    fn materialized() -> EntryState {
        EntryState {
            residency: Residency::Materialized,
            ..EntryState::default()
        }
    }

    #[test]
    fn default_state_is_cloud_only_and_synced() {
        let state = EntryState::default();
        assert_eq!(state.residency, Residency::MetadataOnly);
        assert_eq!(state.project(), EntryStatus::CloudOnly);
        assert!(state.is_synced());
    }

    #[test]
    fn dirty_offline_and_materialized_coexist() {
        let state = EntryState {
            dirty: Dirty::Dirty,
            availability: Availability::Offline,
            ..materialized()
        };
        assert_eq!(state.residency, Residency::Materialized);
        assert_eq!(state.dirty, Dirty::Dirty);
        assert_eq!(state.availability, Availability::Offline);
        assert_eq!(state.transfer, Transfer::Idle);
        assert_eq!(state.usage, Usage::Idle);
        assert_eq!(state.project(), EntryStatus::LocalChanges);
        assert!(!state.is_synced());
    }

    #[test]
    fn pending_remote_apply_is_never_synced() {
        for (remote_apply, expected) in [
            (RemoteApply::Pending, EntryStatus::Syncing),
            (RemoteApply::BlockedByLocal, EntryStatus::Syncing),
            (RemoteApply::Conflicted, EntryStatus::Conflict),
        ] {
            let state = EntryState {
                remote_apply,
                ..materialized()
            };
            assert!(!state.is_synced(), "{remote_apply:?}");
            assert_eq!(state.project(), expected, "{remote_apply:?}");
        }
    }

    #[test]
    fn conflict_outranks_local_changes_but_availability_outranks_both() {
        let conflicted = EntryState {
            dirty: Dirty::Dirty,
            remote_apply: RemoteApply::Conflicted,
            ..materialized()
        };
        assert_eq!(conflicted.project(), EntryStatus::Conflict);

        for availability in [Availability::MountFailed, Availability::RootUnavailable] {
            let state = EntryState {
                availability,
                ..conflicted
            };
            assert_eq!(
                state.project(),
                EntryStatus::Unavailable,
                "{availability:?}"
            );
        }
    }

    #[test]
    fn severe_failure_outranks_conflict() {
        for code in [ErrorCode::HashMismatch, ErrorCode::AccessDenied] {
            let state = EntryState {
                remote_apply: RemoteApply::Conflicted,
                transfer: Transfer::Failed(code),
                ..materialized()
            };
            assert_eq!(state.project(), EntryStatus::Failed, "{code}");
            assert!(!state.is_synced(), "{code}");
        }
    }

    #[test]
    fn retry_wait_stays_syncing_and_is_not_synced() {
        let state = EntryState {
            transfer: Transfer::RetryWait(ErrorCode::RateLimited),
            ..materialized()
        };
        assert_eq!(state.project(), EntryStatus::Syncing);
        assert!(!state.is_synced());
    }

    #[test]
    fn terminal_failure_is_shown_as_failed_even_for_retryable_codes() {
        for code in [ErrorCode::PlaceholderOffline, ErrorCode::HashMismatch] {
            let state = EntryState {
                transfer: Transfer::Failed(code),
                ..materialized()
            };
            assert_eq!(state.project(), EntryStatus::Failed, "{code}");
            assert!(!state.is_synced(), "{code}");
        }
    }

    #[test]
    fn retry_wait_with_a_severe_code_is_shown_as_failure() {
        let state = EntryState {
            transfer: Transfer::RetryWait(ErrorCode::UnsupportedEntry),
            ..materialized()
        };
        assert_eq!(state.project(), EntryStatus::Failed);
    }

    #[test]
    fn residency_decides_downloaded_versus_cloud_only() {
        let downloaded = materialized();
        assert_eq!(downloaded.project(), EntryStatus::Downloaded);
        assert!(downloaded.is_synced());

        for residency in [Residency::MetadataOnly, Residency::Partial] {
            let state = EntryState {
                residency,
                ..materialized()
            };
            assert_eq!(state.project(), EntryStatus::CloudOnly, "{residency:?}");
            assert!(state.is_synced(), "{residency:?}");
        }
    }

    #[test]
    fn offline_keeps_the_confirmed_baseline_synced() {
        let state = EntryState {
            availability: Availability::Offline,
            ..materialized()
        };
        assert_eq!(state.project(), EntryStatus::Downloaded);
        assert!(state.is_synced());
    }

    #[test]
    fn usage_does_not_change_the_main_status_or_sync_fact() {
        for usage in [Usage::ReadHandles, Usage::WriteHandles, Usage::Unknown] {
            let state = EntryState {
                usage,
                ..materialized()
            };
            assert_eq!(state.project(), EntryStatus::Downloaded, "{usage:?}");
            assert!(state.is_synced(), "{usage:?}");
        }
    }

    #[test]
    fn local_change_states_project_as_upload_pending() {
        for dirty in [Dirty::Dirty, Dirty::Snapshotting, Dirty::AwaitingCommit] {
            let state = EntryState {
                dirty,
                transfer: Transfer::Running,
                ..materialized()
            };
            assert_eq!(state.project(), EntryStatus::LocalChanges, "{dirty:?}");
            assert!(!state.is_synced(), "{dirty:?}");
        }
    }

    #[test]
    fn transfer_exposes_error_code_only_when_it_failed() {
        assert_eq!(Transfer::Idle.error_code(), None);
        assert_eq!(Transfer::Queued.error_code(), None);
        assert_eq!(Transfer::Running.error_code(), None);
        assert_eq!(
            Transfer::RetryWait(ErrorCode::RateLimited).error_code(),
            Some(ErrorCode::RateLimited)
        );
        assert_eq!(
            Transfer::Failed(ErrorCode::HashMismatch).error_code(),
            Some(ErrorCode::HashMismatch)
        );
    }
}
