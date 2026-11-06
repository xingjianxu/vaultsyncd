//! 事件线索驱动的观察与分批内容复核（design §12.1）。
//!
//! 这是 [`ExternalObserver`] 的两个入口，与全量扫描 [`ExternalObserver::run_pass`] 共用同一套稳定
//! 窗口、采集与采纳流程，不另开写入通道：
//!
//! - [`ExternalObserver::run_hinted_pass`]：只观察线索指向的路径/子树，加上所有仍待处理的文件（线索
//!   不会重复到达，窗口要靠重新观察才推进）。线索是提示不是事实——路径现在是什么、指纹是什么，仍由
//!   [`WorkTreePort`] 实测；观察到“不存在”**不产生删除结论**；
//! - [`ExternalObserver::run_content_review`]：对已采纳文件分批重新读取内容并与基线摘要比对。元数据
//!   扫描靠指纹（含 ctime）发现变化，内容复核补的是“指纹没变但内容变了”的漏网情形（时间戳粒度、
//!   元数据被还原的文件系统）。内容不同的文件被撤下基线，按首次看到处理（重新等待稳定窗口）。

use std::collections::BTreeSet;
use std::ops::Bound;

use crate::observer::capture::{CaptureOutcome, capture_external};
use crate::observer::import::{
    ExternalObserver, FileOutcome, ObserverPorts, PassClock, PassLimits,
};
use crate::observer::scan::{
    InconclusiveReason, ScanCompletion, ScanCursor, ScannedFile, scan_tree,
};
use crate::ports::external_store::ExternalAdoptStore;
use crate::ports::work_tree::{FileStamp, WorkEntryKind, WorkObservation, WorkTreeError};

/// 一次线索驱动观察的报告。
#[derive(Debug, Default)]
pub struct HintedReport {
    /// 逐文件结局。
    pub outcomes: Vec<FileOutcome>,
    /// 观察时读不了的路径（权限、IO）与原因：**不是删除**，保持原状态，等待重试。
    pub unreadable: Vec<(String, String)>,
    /// 观察时确认不存在的路径：只是现场事实，本流程不据此推导删除（已采纳文件的外部删除属于后续
    /// 事件管线）。
    pub absent: Vec<String>,
    /// 子树扫描被预算截断：需要交给全量扫描续扫。
    pub subtree_truncated: bool,
    /// Root 不可用而中止。
    pub root_unavailable: bool,
}

/// 一次内容复核的报告。
#[derive(Debug, Default)]
pub struct ReviewReport {
    /// 本批复核的文件数（成功重新读取并比对）。
    pub reviewed: usize,
    /// 内容与基线不同、已按外部变化重新排队的路径。
    pub content_changed: Vec<String>,
    /// 读不了的路径与原因：**不是删除**，基线保持不动。
    pub unreadable: Vec<(String, String)>,
    /// 复核时确认不存在的路径（不推导删除）。
    pub absent: Vec<String>,
    /// 内容变化的文件在本次的结局（等待窗口等）。
    pub outcomes: Vec<FileOutcome>,
    /// 一圈复核已走完（下一批从头开始）。
    pub wrapped: bool,
    /// Root 不可用而中止。
    pub root_unavailable: bool,
}

impl ExternalObserver {
    /// 观察线索指向的路径与子树，并推进所有待处理文件。
    pub async fn run_hinted_pass<S: ExternalAdoptStore>(
        &mut self,
        ports: &mut ObserverPorts<'_, S>,
        paths: &[String],
        subtrees: &[String],
        limits: PassLimits,
        clock: PassClock,
    ) -> HintedReport {
        let mut report = HintedReport::default();
        let mut files: Vec<ScannedFile> = Vec::new();
        let mut queued: BTreeSet<String> = BTreeSet::new();
        let mut dirs: Vec<String> = subtrees.to_vec();

        let mut wanted: Vec<String> = paths.to_vec();
        wanted.extend(self.pending.keys().cloned());
        for path in wanted {
            if !queued.insert(path.clone()) {
                continue;
            }
            match ports.work.observe(&path) {
                Ok(WorkObservation::File(stamp)) => files.push(ScannedFile { path, stamp }),
                Ok(WorkObservation::NotRegular(WorkEntryKind::Directory)) => dirs.push(path),
                Ok(WorkObservation::NotRegular(_)) => self.settle(&path),
                Ok(WorkObservation::Absent) | Err(WorkTreeError::Gone { .. }) => {
                    // 从未采纳的待处理文件消失了：没有历史可保护，移出待处理；已采纳的不动。
                    self.settle(&path);
                    report.absent.push(path);
                }
                Err(WorkTreeError::RootUnavailable { .. }) => {
                    report.root_unavailable = true;
                    return report;
                }
                Err(error) => report.unreadable.push((path, error.to_string())),
            }
        }

        let mut seen_dirs = BTreeSet::new();
        for dir in dirs {
            if !seen_dirs.insert(dir.clone()) {
                continue;
            }
            let scan = scan_tree(&mut *ports.work, limits.scan, ScanCursor::from_dir(&dir));
            let root_gone = scan.root_unavailable();
            if let ScanCompletion::Inconclusive { reasons, .. } = &scan.completion {
                for reason in reasons {
                    match reason {
                        InconclusiveReason::BudgetExhausted { .. } => {
                            report.subtree_truncated = true;
                        }
                        InconclusiveReason::DirectoryUnreadable { path, detail, .. }
                        | InconclusiveReason::EntryUnreadable { path, detail, .. } => {
                            report.unreadable.push((path.clone(), detail.clone()));
                        }
                        InconclusiveReason::RootUnavailable { .. } => {}
                    }
                }
            }
            if root_gone {
                report.root_unavailable = true;
                return report;
            }
            for file in scan.files {
                if queued.insert(file.path.clone()) {
                    files.push(file);
                }
            }
        }

        let (outcomes, aborted) = self.handle_files(ports, files, limits, clock, false).await;
        report.outcomes = outcomes;
        report.root_unavailable = aborted;
        report
    }

    /// 复核一批已采纳文件的内容（每批最多 `batch` 个，按路径顺序循环）。
    pub async fn run_content_review<S: ExternalAdoptStore>(
        &mut self,
        ports: &mut ObserverPorts<'_, S>,
        batch: usize,
        limits: PassLimits,
        clock: PassClock,
    ) -> ReviewReport {
        let mut report = ReviewReport::default();
        let lower = self
            .review_after
            .as_ref()
            .map_or(Bound::Unbounded, |last| Bound::Excluded(last.clone()));
        let chosen: Vec<String> = self
            .adopted
            .range::<String, _>((lower, Bound::Unbounded))
            .map(|(path, _)| path.clone())
            .take(batch)
            .collect();
        report.wrapped = chosen.len() < batch;
        self.review_after = if report.wrapped {
            None
        } else {
            chosen.last().cloned()
        };

        let mut changed: Vec<ScannedFile> = Vec::new();
        for path in chosen {
            match self.review_one(ports, &path).await {
                Review::Same => report.reviewed += 1,
                Review::Changed(stamp) => {
                    report.reviewed += 1;
                    report.content_changed.push(path.clone());
                    changed.push(ScannedFile { path, stamp });
                }
                Review::Skipped => {}
                Review::Absent => report.absent.push(path),
                Review::Unreadable(detail) => report.unreadable.push((path, detail)),
                Review::RootUnavailable => {
                    report.root_unavailable = true;
                    return report;
                }
            }
        }
        let (outcomes, aborted) = self
            .handle_files(ports, changed, limits, clock, false)
            .await;
        report.outcomes = outcomes;
        report.root_unavailable = aborted;
        report
    }

    async fn review_one<S: ExternalAdoptStore>(
        &mut self,
        ports: &mut ObserverPorts<'_, S>,
        path: &str,
    ) -> Review {
        let stamp = match ports.work.observe(path) {
            Ok(WorkObservation::File(stamp)) => stamp,
            Ok(WorkObservation::Absent) | Err(WorkTreeError::Gone { .. }) => return Review::Absent,
            Ok(WorkObservation::NotRegular(_)) => return Review::Skipped,
            Err(WorkTreeError::RootUnavailable { .. }) => return Review::RootUnavailable,
            Err(error) => return Review::Unreadable(error.to_string()),
        };
        let staging = self.next_staging_name();
        let outcome = capture_external(
            &mut *ports.work,
            &mut *ports.blobs,
            ports.layout,
            self.root,
            path,
            &stamp,
            &staging,
        );
        let proposal = match outcome {
            Ok(CaptureOutcome::Proposal(proposal)) => proposal,
            // 复核时文件正在变化：普通扫描会按指纹处理它，这里不下结论。
            Ok(CaptureOutcome::Requeue(_) | CaptureOutcome::NotRegular(_)) => {
                return Review::Skipped;
            }
            Ok(CaptureOutcome::Vanished) => return Review::Absent,
            Err(error) if error.is_root_unavailable() => return Review::RootUnavailable,
            Err(error) => return Review::Unreadable(error.to_string()),
        };
        let Some(baseline) = self.adopted.get_mut(path) else {
            return Review::Skipped;
        };
        if baseline.content == proposal.content {
            // 内容没变：指纹可能已变（例如 touch），刷新基线指纹避免下次元数据扫描再采集。
            baseline.stamp = proposal.stamp;
            return Review::Same;
        }
        // 内容变了而指纹（或扫描）没发现：撤下基线，按首次看到重新等待稳定窗口。
        self.adopted.remove(path);
        self.tracker.forget(path);
        Review::Changed(proposal.stamp)
    }
}

enum Review {
    Same,
    Changed(FileStamp),
    Skipped,
    Absent,
    Unreadable(String),
    RootUnavailable,
}
