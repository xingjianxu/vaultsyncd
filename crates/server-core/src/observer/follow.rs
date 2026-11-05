//! 持续观察的调度（design §12.1、§23）：把事件线索、全量补扫、周期元数据扫描与分批内容复核
//! 编排成一步一步的 [`FollowDriver::step`]。
//!
//! 事件只是线索，所以每一条路径都有兜底：
//!
//! | 路径 | 触发 | 目的 |
//! | --- | --- | --- |
//! | 线索观察 | 去抖后的路径/子树线索与所有待处理文件 | 1—3 秒内发现变化 |
//! | 全量补扫 | 启动/重启、监视队列溢出、线索队列满、子树截断、上次扫描没走完或有读不了的项 | 事件丢失后恢复；**不经去抖，立即执行** |
//! | 周期元数据扫描 | [`FollowConfig::metadata_interval_ms`] | 没有事件的目录（监视缺口）与遗漏 |
//! | 分批内容复核 | 每个复核间隔一批 | 指纹没变但内容变了的最终发现 |
//!
//! 调用方（进程入口）负责：读监视器、把线索 [`FollowDriver::push_hint`] 进来、按自己的节拍调用
//! `step`。本模块不做任何平台 I/O，也不推导删除：读不了的目录/项只被记录并重试，扫描没走完就保持
//! 续扫，直到一次完整扫描。

use crate::observer::hints::{FullScanCause, HintLimits, HintQueue, WatchHint};
use crate::observer::import::{
    ExternalObserver, HintedReport, ObserverPorts, PassClock, PassLimits, PassReport, ReviewReport,
};
use crate::observer::scan::InconclusiveReason;
use crate::ports::external_store::ExternalAdoptStore;

/// 调度参数不成立。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowConfigError {
    /// 周期/重试/复核间隔必须大于 0。
    ZeroInterval,
    /// 每批复核数必须大于 0，否则内容复核永远不会发生。
    ZeroReviewBatch,
}

impl std::fmt::Display for FollowConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FollowConfigError::ZeroInterval => f.write_str("扫描/重试/复核间隔必须大于 0 毫秒"),
            FollowConfigError::ZeroReviewBatch => f.write_str("每批内容复核数必须大于 0"),
        }
    }
}

impl std::error::Error for FollowConfigError {}

/// 调度参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowConfig {
    hints: HintLimits,
    metadata_interval_ms: u64,
    retry_interval_ms: u64,
    review_interval_ms: u64,
    review_batch: usize,
}

impl FollowConfig {
    /// 设计建议的初始值（design §23）：去抖 300 ms、周期元数据补扫 60 s、读不了的重试 5 s、
    /// 每 10 s 复核 64 个文件；线索队列 4096 条。
    #[must_use]
    pub fn recommended() -> Self {
        FollowConfig {
            hints: HintLimits::new(4_096, 300).expect("常量合法"),
            metadata_interval_ms: 60_000,
            retry_interval_ms: 5_000,
            review_interval_ms: 10_000,
            review_batch: 64,
        }
    }

    /// 自定义参数。
    ///
    /// # Errors
    ///
    /// 任一间隔为 0 或每批复核数为 0。
    pub fn new(
        hints: HintLimits,
        metadata_interval_ms: u64,
        retry_interval_ms: u64,
        review_interval_ms: u64,
        review_batch: usize,
    ) -> Result<Self, FollowConfigError> {
        if metadata_interval_ms == 0 || retry_interval_ms == 0 || review_interval_ms == 0 {
            return Err(FollowConfigError::ZeroInterval);
        }
        if review_batch == 0 {
            return Err(FollowConfigError::ZeroReviewBatch);
        }
        Ok(FollowConfig {
            hints,
            metadata_interval_ms,
            retry_interval_ms,
            review_interval_ms,
            review_batch,
        })
    }

    /// 周期元数据扫描间隔（毫秒）。
    #[must_use]
    pub fn metadata_interval_ms(&self) -> u64 {
        self.metadata_interval_ms
    }
}

/// 上一次全量扫描的结论，决定下一次何时再扫。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanState {
    /// 走完且没有读取失败：等周期。
    Clean,
    /// 预算耗尽、还有游标：立刻继续。
    Truncated,
    /// 有读不了的目录/项或 Root 不可用：到重试时间再扫。
    Unreadable,
}

/// 一次全量补扫及其原因。
#[derive(Debug)]
pub struct FullScanRun {
    /// 为什么扫。
    pub cause: FullScanCause,
    /// 本轮报告。
    pub report: PassReport,
}

/// 一步的报告。
#[derive(Debug, Default)]
pub struct StepReport {
    /// 本步做了全量补扫（含续扫）。
    pub full_scan: Option<FullScanRun>,
    /// 本步做了线索观察。
    pub hinted: Option<HintedReport>,
    /// 本步做了内容复核。
    pub review: Option<ReviewReport>,
    /// 已知没有被监视的目录数（只能靠周期扫描覆盖）。
    pub watch_gaps: usize,
}

/// 持续观察的驱动器：持有观察器与线索队列。
pub struct FollowDriver {
    observer: ExternalObserver,
    hints: HintQueue,
    config: FollowConfig,
    started: bool,
    scan_state: ScanState,
    last_full_ms: Option<u64>,
    last_review_ms: Option<u64>,
    forced: Option<FullScanCause>,
}

impl FollowDriver {
    /// 以观察器与参数建立驱动器。第一次 [`step`](Self::step) 一定是启动补扫。
    #[must_use]
    pub fn new(observer: ExternalObserver, config: FollowConfig) -> Self {
        FollowDriver {
            observer,
            hints: HintQueue::new(config.hints),
            config,
            started: false,
            scan_state: ScanState::Clean,
            last_full_ms: None,
            last_review_ms: None,
            forced: None,
        }
    }

    /// 观察器（只读）。
    #[must_use]
    pub fn observer(&self) -> &ExternalObserver {
        &self.observer
    }

    /// 观察器（登记/释放自写意图等）。
    pub fn observer_mut(&mut self) -> &mut ExternalObserver {
        &mut self.observer
    }

    /// 线索队列（只读，诊断用）。
    #[must_use]
    pub fn hints(&self) -> &HintQueue {
        &self.hints
    }

    /// 记下一条线索。
    pub fn push_hint(&mut self, hint: WatchHint, now_ms: u64) {
        self.hints.push(hint, now_ms);
    }

    fn full_scan_cause(
        &mut self,
        queued: Option<FullScanCause>,
        now_ms: u64,
    ) -> Option<FullScanCause> {
        let forced = self.forced.take();
        if !self.started {
            return Some(FullScanCause::Startup);
        }
        if let Some(cause) = queued.or(forced) {
            return Some(cause);
        }
        let since_last = now_ms.saturating_sub(self.last_full_ms.unwrap_or(0));
        match self.scan_state {
            ScanState::Truncated => Some(FullScanCause::Continuation),
            ScanState::Unreadable if since_last >= self.config.retry_interval_ms => {
                Some(FullScanCause::Retry)
            }
            ScanState::Clean if since_last >= self.config.metadata_interval_ms => {
                Some(FullScanCause::Periodic)
            }
            _ => None,
        }
    }

    /// 执行一步：全量补扫（若需要）→ 线索观察（若需要）→ 内容复核（若到期）。
    pub async fn step<S: ExternalAdoptStore>(
        &mut self,
        ports: &mut ObserverPorts<'_, S>,
        limits: PassLimits,
        clock: PassClock,
    ) -> StepReport {
        let now = clock.now_ms;
        let due = self.hints.take_due(now);
        let mut report = StepReport {
            watch_gaps: self.hints.watch_gaps().len(),
            ..StepReport::default()
        };

        let mut full_complete = false;
        if let Some(cause) = self.full_scan_cause(due.full_scan, now) {
            let pass = self.observer.run_pass(ports, limits, clock).await;
            self.started = true;
            self.last_full_ms = Some(now);
            full_complete = pass.scan_complete && !pass.root_unavailable;
            self.scan_state = if full_complete {
                ScanState::Clean
            } else if !pass.root_unavailable
                && pass
                    .inconclusive
                    .iter()
                    .all(|reason| matches!(reason, InconclusiveReason::BudgetExhausted { .. }))
            {
                ScanState::Truncated
            } else {
                ScanState::Unreadable
            };
            report.full_scan = Some(FullScanRun {
                cause,
                report: pass,
            });
        }

        let has_hints = !due.paths.is_empty() || !due.subtrees.is_empty();
        let pending_to_advance = report.full_scan.is_none() && self.observer.pending_count() > 0;
        if !full_complete && (has_hints || pending_to_advance) {
            let hinted = self
                .observer
                .run_hinted_pass(ports, &due.paths, &due.subtrees, limits, clock)
                .await;
            if hinted.subtree_truncated {
                self.forced = Some(FullScanCause::SubtreeTruncated);
            }
            report.hinted = Some(hinted);
        }

        let last_review = *self.last_review_ms.get_or_insert(now);
        if now.saturating_sub(last_review) >= self.config.review_interval_ms {
            self.last_review_ms = Some(now);
            let review = self
                .observer
                .run_content_review(ports, self.config.review_batch, limits, clock)
                .await;
            report.review = Some(review);
        }
        report
    }
}

#[cfg(test)]
mod tests;
