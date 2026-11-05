//! 外部目录的初始稳定采集：把扫描、稳定窗口、采集与采纳串成一轮轮可续的流程（design §12.1）。
//!
//! 一轮 [`ExternalObserver::run_pass`] 做的事：
//!
//! 1. 从上次的续扫游标（没有就从 Root 起点）做一次有界扫描（[`scan_tree`]）；
//! 2. 每个枚举到的文件记录指纹；**首次看到总要等待一个完整稳定窗口**，指纹在窗口内变化就重新计时
//!    （持续写入的文件一直是“等待”，不会被反复传播部分片段）；
//! 3. 稳定的文件做采集（[`capture_external`]）与采纳（[`adopt_external`]），每轮最多
//!    [`PassLimits::max_captures`] 个；
//! 4. 结果记入内存账本：**只有采纳成功才算“已有历史保护”**，其余一律是待处理，各带原因。
//!
//! 不变量：
//!
//! - 本流程**从不推导删除**：扫描中读不到、消失、Root 不可用都不产生任何删除结论；消失的从未采纳
//!   文件只是从待处理中移除。已采纳文件之后被外部删除属于后续事件管线的职责，这里不处理；
//! - 扫描不完整（预算耗尽、读取失败、Root 不可用）时，**不**宣布初始导入完成，也不据此清理账本；
//! - 初始导入完成需要：一轮扫描完整走完，且这一轮结束时没有任何待处理文件。完成标志一旦为真
//!   就保持（之后的新文件按普通外部变化处理）；
//! - 账本只是进程内的报告视图，真相在库里；重启后账本为空，文件会被重新枚举，内容相同的由库查询
//!   识别为“内容未变”，不会产生重复版本。

use std::collections::{BTreeMap, BTreeSet};

use vaultsyncd_domain::ids::RootId;
use vaultsyncd_domain::version::UtcTimestamp;

use crate::commit::put::RootCommitGate;
use crate::objects::{InvalidStagingName, ObjectLayout};
use crate::observer::capture::{
    AdoptFailure, AdoptOutcome, BlockReason, CaptureOutcome, RequeueReason, adopt_external,
    capture_external,
};
use crate::observer::scan::{
    InconclusiveReason, ScanCompletion, ScanCursor, ScanLimits, ScannedFile, SkippedEntry,
    Stability, StabilityTracker, StabilityWindow, scan_tree,
};
use crate::ports::external_store::{ExternalAdoptStore, ExternalRecord};
use crate::ports::object_store::{CopyFallback, CopyMethod, ObjectBlobPort};
use crate::ports::work_tree::{FileStamp, WorkObservation, WorkTreeError, WorkTreePort};

/// 一轮的预算。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PassLimits {
    /// 扫描预算。
    pub scan: ScanLimits,
    /// 每轮最多采集的文件数（采集要读完整文件，必须有上限）。
    pub max_captures: usize,
}

/// 一轮的时钟读数。
#[derive(Clone, Copy, Debug)]
pub struct PassClock {
    /// 单调毫秒读数（稳定窗口用）。
    pub now_ms: u64,
    /// 墙钟时间（写入版本记录）。
    pub at: UtcTimestamp,
}

/// 一轮用到的端口集合。
pub struct ObserverPorts<'a, S: ExternalAdoptStore> {
    /// 工作目录只读观察端口。
    pub work: &'a mut dyn WorkTreePort,
    /// 对象字节端口。
    pub blobs: &'a mut dyn ObjectBlobPort,
    /// 对象区布局。
    pub layout: &'a ObjectLayout,
    /// Root 内提交串行化（与 put 共用）。
    pub gate: &'a mut RootCommitGate,
    /// 采纳存储端口。
    pub store: &'a mut S,
}

/// 被推迟的原因（不是失败，下一轮继续）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferReason {
    /// 同一 Root 正在提交别的内容。
    RootBusy,
    /// 本轮采集预算用尽。
    CaptureBudget,
}

/// 文件尚未被采纳的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingReason {
    /// 正在等待文件写入完成。
    Waiting {
        /// 指纹已保持不变的毫秒数。
        quiet_ms: u64,
    },
    /// 需要重新排队。
    Requeued(RequeueReason),
    /// 被阻止，需要人工协调。
    Blocked(BlockReason),
    /// 采集或采纳失败（稳定错误码 + 文本），下一轮重试。
    Failed {
        /// 稳定错误码。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
    /// 被推迟。
    Deferred(DeferReason),
}

/// 一个文件在本轮的结局。
#[derive(Debug)]
pub enum FileState {
    /// 已采纳（有版本与事件）。
    Adopted {
        /// 事务证据。
        record: ExternalRecord,
        /// 是否新建条目。
        created: bool,
        /// 候选副本的来源方式（reflink 或校验复制）。
        copy_method: CopyMethod,
        /// 没走克隆的原因（跨卷或文件系统不支持时非空）。
        copy_fallback: Option<CopyFallback>,
    },
    /// 库里已有相同内容，没有新版本。
    UnchangedContent,
    /// 本轮没有被采纳，附原因。
    Pending(PendingReason),
    /// 采集前后消失：从未采纳，没有历史需要保护。
    Vanished,
    /// 采集时已不是普通文件。
    NotRegular,
}

/// 一个文件的本轮结局。
#[derive(Debug)]
pub struct FileOutcome {
    /// 相对路径。
    pub path: String,
    /// 结局。
    pub state: FileState,
}

/// 一轮的报告。
#[derive(Debug)]
pub struct PassReport {
    /// 本轮扫描是否完整（含续扫游标走完）。
    pub scan_complete: bool,
    /// 扫描不完整的原因。
    pub inconclusive: Vec<InconclusiveReason>,
    /// 本轮检查的目录项数。
    pub entries_examined: usize,
    /// 被跳过的项。
    pub skipped: Vec<SkippedEntry>,
    /// 逐文件结局。
    pub outcomes: Vec<FileOutcome>,
    /// Root 不可用而中止（本轮没有采集任何内容，也没有推导任何删除）。
    pub root_unavailable: bool,
    /// 初始导入是否已完成（只增不减）。
    pub initial_import_complete: bool,
}

/// 初始导入创建时的参数错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObserverConfigError {
    /// 暂存名前缀不合规。
    StagingPrefix(InvalidStagingName),
    /// 每轮采集数必须大于 0。
    NoCaptureBudget,
}

impl std::fmt::Display for ObserverConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObserverConfigError::StagingPrefix(error) => write!(f, "暂存名前缀不合规：{error}"),
            ObserverConfigError::NoCaptureBudget => f.write_str("每轮采集数必须大于 0"),
        }
    }
}

impl std::error::Error for ObserverConfigError {}

impl PassLimits {
    /// 构造；`max_captures` 为 0 会让稳定文件永远不被采集，因此拒绝。
    ///
    /// # Errors
    ///
    /// 采集数为 0。
    pub fn new(scan: ScanLimits, max_captures: usize) -> Result<Self, ObserverConfigError> {
        if max_captures == 0 {
            return Err(ObserverConfigError::NoCaptureBudget);
        }
        Ok(PassLimits { scan, max_captures })
    }
}

/// 某个 Root 的外部内容观察器（初始稳定采集）。
pub struct ExternalObserver {
    root: RootId,
    tracker: StabilityTracker,
    /// 已采纳（或库里已有相同内容）的路径 → 采纳时的指纹；指纹未变则不再采集。
    adopted: BTreeMap<String, FileStamp>,
    pending: BTreeMap<String, PendingReason>,
    resume: Option<ScanCursor>,
    cycle_seen: BTreeSet<String>,
    staging_prefix: String,
    staging_counter: u64,
    initial_import_complete: bool,
}

impl ExternalObserver {
    /// 建立观察器。`staging_prefix` 必须在对象区 staging 目录里唯一（例如含进程号与 Root 前缀）。
    ///
    /// # Errors
    ///
    /// 暂存名前缀不是合规的单个文件名。
    pub fn new(
        root: RootId,
        window: StabilityWindow,
        staging_prefix: &str,
    ) -> Result<Self, ObserverConfigError> {
        ObjectLayout::validate_staging_name(staging_prefix)
            .map_err(ObserverConfigError::StagingPrefix)?;
        Ok(ExternalObserver {
            root,
            tracker: StabilityTracker::new(window),
            adopted: BTreeMap::new(),
            pending: BTreeMap::new(),
            resume: None,
            cycle_seen: BTreeSet::new(),
            staging_prefix: staging_prefix.to_owned(),
            staging_counter: 0,
            initial_import_complete: false,
        })
    }

    /// 路径是否已有历史保护：只有采纳成功（或库里已有相同内容）才为真，未采集成功的条目一律为假。
    #[must_use]
    pub fn history_protected(&self, path: &str) -> bool {
        self.adopted.contains_key(path)
    }

    /// 路径尚未被采纳的原因。
    #[must_use]
    pub fn pending_reason(&self, path: &str) -> Option<&PendingReason> {
        self.pending.get(path)
    }

    /// 待处理文件数。
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// 初始导入是否已完成（`pending_import` 的反面；只增不减）。
    #[must_use]
    pub fn initial_import_complete(&self) -> bool {
        self.initial_import_complete
    }

    /// 上一轮是否留下了续扫游标（扫描不完整）。
    #[must_use]
    pub fn resume_cursor(&self) -> Option<&ScanCursor> {
        self.resume.as_ref()
    }

    fn next_staging_name(&mut self) -> String {
        self.staging_counter += 1;
        format!("{}-{}", self.staging_prefix, self.staging_counter)
    }

    fn mark_pending(&mut self, path: &str, reason: PendingReason) {
        self.pending.insert(path.to_owned(), reason);
    }

    fn settle(&mut self, path: &str) {
        self.pending.remove(path);
        self.tracker.forget(path);
    }

    /// 执行一轮。
    pub async fn run_pass<S: ExternalAdoptStore>(
        &mut self,
        ports: &mut ObserverPorts<'_, S>,
        limits: PassLimits,
        clock: PassClock,
    ) -> PassReport {
        let cursor = self.resume.take().unwrap_or_else(ScanCursor::from_root);
        let scan = scan_tree(&mut *ports.work, limits.scan, cursor);
        let root_unavailable = scan.root_unavailable();
        let (scan_complete, inconclusive) = match scan.completion {
            ScanCompletion::Complete => (true, Vec::new()),
            ScanCompletion::Inconclusive { resume, reasons } => {
                self.resume = Some(resume);
                (false, reasons)
            }
        };

        let mut outcomes = Vec::new();
        let mut candidates: Vec<(String, FileStamp)> = Vec::new();
        let mut files = scan.files;
        let mut aborted = root_unavailable;
        if !root_unavailable {
            // 续扫只覆盖游标里的目录；之前已见过、仍待处理的文件要在这一轮单独复查，否则一个一直读不了
            // 的目录或一次被截断的扫描会让其他文件的稳定窗口永远不推进。
            aborted |= self.recheck_pending(&mut *ports.work, limits, &mut files);
        }
        if !aborted {
            for file in files {
                self.cycle_seen.insert(file.path.clone());
                if self.adopted.get(&file.path) == Some(&file.stamp) {
                    self.pending.remove(&file.path);
                    continue;
                }
                match self.tracker.observe(&file.path, file.stamp, clock.now_ms) {
                    Stability::Stable => candidates.push((file.path, file.stamp)),
                    Stability::Waiting { quiet_ms, .. } => {
                        let reason = PendingReason::Waiting { quiet_ms };
                        self.mark_pending(&file.path, reason.clone());
                        outcomes.push(FileOutcome {
                            path: file.path,
                            state: FileState::Pending(reason),
                        });
                    }
                }
            }
        }

        let mut captures = 0usize;
        for (path, stamp) in candidates {
            if aborted {
                break;
            }
            if captures >= limits.max_captures {
                let reason = PendingReason::Deferred(DeferReason::CaptureBudget);
                self.mark_pending(&path, reason.clone());
                outcomes.push(FileOutcome {
                    path,
                    state: FileState::Pending(reason),
                });
                continue;
            }
            captures += 1;
            let (state, root_gone) = self.process_candidate(ports, &path, &stamp, clock).await;
            aborted |= root_gone;
            outcomes.push(FileOutcome { path, state });
        }

        if scan_complete && !aborted {
            let seen = std::mem::take(&mut self.cycle_seen);
            self.pending.retain(|path, _| seen.contains(path));
            self.tracker.retain(|path| seen.contains(path));
            if self.pending.is_empty() {
                self.initial_import_complete = true;
            }
        }

        PassReport {
            scan_complete,
            inconclusive,
            entries_examined: scan.entries_examined,
            skipped: scan.skipped,
            outcomes,
            root_unavailable: aborted,
            initial_import_complete: self.initial_import_complete,
        }
    }

    /// 复查本轮扫描没有覆盖到的待处理文件，把仍是普通文件的并入 `files`；返回 Root 是否不可用。
    ///
    /// 复查读不到（权限等）的文件保持待处理，不当成消失；只有确认路径不存在才移出待处理——它从未
    /// 被采纳，没有历史可保护，也没有可传播的删除。
    fn recheck_pending(
        &mut self,
        work: &mut dyn WorkTreePort,
        limits: PassLimits,
        files: &mut Vec<ScannedFile>,
    ) -> bool {
        let scanned: BTreeSet<&str> = files.iter().map(|file| file.path.as_str()).collect();
        let wanted: Vec<String> = self
            .pending
            .keys()
            .filter(|path| !scanned.contains(path.as_str()))
            .take(limits.scan.max_entries())
            .cloned()
            .collect();
        for path in wanted {
            match work.observe(&path) {
                Ok(WorkObservation::File(stamp)) => files.push(ScannedFile { path, stamp }),
                Ok(WorkObservation::Absent | WorkObservation::NotRegular(_)) => {
                    self.settle(&path);
                }
                Err(WorkTreeError::RootUnavailable { .. }) => return true,
                Err(_) => {}
            }
        }
        false
    }

    /// 处理一个稳定文件；返回结局与“Root 是否不可用”。
    async fn process_candidate<S: ExternalAdoptStore>(
        &mut self,
        ports: &mut ObserverPorts<'_, S>,
        path: &str,
        stamp: &FileStamp,
        clock: PassClock,
    ) -> (FileState, bool) {
        let staging = self.next_staging_name();
        let captured = capture_external(
            &mut *ports.work,
            &mut *ports.blobs,
            ports.layout,
            self.root,
            path,
            stamp,
            &staging,
        );
        let proposal = match captured {
            Ok(CaptureOutcome::Proposal(proposal)) => proposal,
            Ok(CaptureOutcome::Requeue(reason)) => return (self.requeue(path, reason), false),
            Ok(CaptureOutcome::Vanished) => {
                self.settle(path);
                return (FileState::Vanished, false);
            }
            Ok(CaptureOutcome::NotRegular(_)) => {
                self.settle(path);
                return (FileState::NotRegular, false);
            }
            Err(error) => {
                let root_gone = error.is_root_unavailable();
                let reason = PendingReason::Failed {
                    code: error.code(),
                    detail: error.to_string(),
                };
                self.mark_pending(path, reason.clone());
                return (FileState::Pending(reason), root_gone);
            }
        };

        let adopted = adopt_external(
            &mut *ports.gate,
            &mut *ports.store,
            &mut *ports.work,
            &proposal,
            clock.at,
        )
        .await;
        match adopted {
            Ok(AdoptOutcome::Adopted { record, created }) => {
                self.adopted.insert(path.to_owned(), proposal.stamp);
                self.settle(path);
                let state = FileState::Adopted {
                    record,
                    created,
                    copy_method: proposal.object.method,
                    copy_fallback: proposal.object.fallback,
                };
                (state, false)
            }
            Ok(AdoptOutcome::UnchangedContent { .. }) => {
                self.adopted.insert(path.to_owned(), proposal.stamp);
                self.settle(path);
                (FileState::UnchangedContent, false)
            }
            Ok(AdoptOutcome::Requeue(reason)) => (self.requeue(path, reason), false),
            Ok(AdoptOutcome::Vanished) => {
                self.settle(path);
                (FileState::Vanished, false)
            }
            Ok(AdoptOutcome::NotRegular(_)) => {
                self.settle(path);
                (FileState::NotRegular, false)
            }
            Ok(AdoptOutcome::RootBusy) => {
                let reason = PendingReason::Deferred(DeferReason::RootBusy);
                self.mark_pending(path, reason.clone());
                (FileState::Pending(reason), false)
            }
            Ok(AdoptOutcome::Blocked(reason)) => {
                let reason = PendingReason::Blocked(reason);
                self.mark_pending(path, reason.clone());
                (FileState::Pending(reason), false)
            }
            Err(error) => {
                let root_gone = matches!(
                    error,
                    AdoptFailure::Work(
                        crate::ports::work_tree::WorkTreeError::RootUnavailable { .. }
                    )
                );
                let reason = PendingReason::Failed {
                    code: error.code(),
                    detail: error.to_string(),
                };
                self.mark_pending(path, reason.clone());
                (FileState::Pending(reason), root_gone)
            }
        }
    }

    /// 重排队：忘记指纹，重新等待一个完整窗口。
    fn requeue(&mut self, path: &str, reason: RequeueReason) -> FileState {
        self.tracker.forget(path);
        let reason = PendingReason::Requeued(reason);
        self.mark_pending(path, reason.clone());
        FileState::Pending(reason)
    }
}

#[cfg(test)]
mod tests;
