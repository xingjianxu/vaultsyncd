//! 变化线索的有界去抖队列（design §12.1）。
//!
//! 监视器给出的只是“这里值得再看一眼”的线索：可能重复、可能丢失、可能在文件写到一半时到达。本队列
//! 做三件事：
//!
//! - **合并同路径线索并去抖**：一个路径的线索只在静默满 [`HintLimits::debounce_ms`] 后才交出，持续写入
//!   的文件不会被反复交出（稳定窗口另由 [`crate::observer::scan::StabilityTracker`] 负责）；
//! - **有界**：路径数达到上限就整体退化为一次全量补扫（[`FullScanCause::HintQueueFull`]），而不是无限
//!   增长或悄悄丢线索；
//! - **溢出立即补扫**：监视队列溢出（[`WatchHint::Overflow`]）不经去抖，下一次取用就要求全量补扫，
//!   并丢弃已排队的路径线索——全量补扫覆盖它们。
//!
//! 线索不是事实：队列里没有“删除”“已写完”的概念，只有路径与子树。

use std::collections::BTreeMap;
use std::fmt;

/// 监视器给出的线索（平台无关）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchHint {
    /// 这个路径（文件或目录）上发生过事件。
    Changed(String),
    /// 这个目录是新出现的，内容在监视建立之前就可能存在：补扫整个子树。
    Subtree(String),
    /// 内核事件队列溢出，已有事件丢失：立即全量补扫。
    Overflow,
    /// 该目录没有被监视（权限、监视数上限等）：只能靠周期扫描覆盖，并补看一眼这个目录。
    Gap {
        /// 相对路径。
        path: String,
        /// 原因文本。
        detail: String,
    },
}

/// 为什么要做一次全量补扫。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullScanCause {
    /// 服务（重）启动：启动前发生的变化没有事件。
    Startup,
    /// 监视队列溢出。
    QueueOverflow,
    /// 线索队列满，退化为全量。
    HintQueueFull,
    /// 子树扫描被预算截断，交给全量扫描续扫。
    SubtreeTruncated,
    /// 上一次全量扫描没有走完（预算耗尽），继续。
    Continuation,
    /// 上一次全量扫描有读不了的目录/项，到了重试时间。
    Retry,
    /// 周期元数据扫描到期。
    Periodic,
}

impl FullScanCause {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FullScanCause::Startup => "startup",
            FullScanCause::QueueOverflow => "queue_overflow",
            FullScanCause::HintQueueFull => "hint_queue_full",
            FullScanCause::SubtreeTruncated => "subtree_truncated",
            FullScanCause::Continuation => "continuation",
            FullScanCause::Retry => "retry",
            FullScanCause::Periodic => "periodic",
        }
    }
}

/// 队列参数不成立。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidHintLimits;

impl fmt::Display for InvalidHintLimits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("线索队列容量必须大于 0，去抖时间必须大于 0 毫秒")
    }
}

impl std::error::Error for InvalidHintLimits {}

/// 队列参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HintLimits {
    max_paths: usize,
    debounce_ms: u64,
}

impl HintLimits {
    /// 构造。
    ///
    /// # Errors
    ///
    /// 容量或去抖时间为 0（前者无法排队，后者等于没有去抖）。
    pub fn new(max_paths: usize, debounce_ms: u64) -> Result<Self, InvalidHintLimits> {
        if max_paths == 0 || debounce_ms == 0 {
            return Err(InvalidHintLimits);
        }
        Ok(HintLimits {
            max_paths,
            debounce_ms,
        })
    }
}

/// 一次取用的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DueHints {
    /// 要求立即全量补扫的原因（此时 `paths` 与 `subtrees` 为空）。
    pub full_scan: Option<FullScanCause>,
    /// 静默满去抖时间、需要观察的路径。
    pub paths: Vec<String>,
    /// 静默满去抖时间、需要补扫的子树。
    pub subtrees: Vec<String>,
}

/// 有界去抖队列。
#[derive(Debug)]
pub struct HintQueue {
    limits: HintLimits,
    paths: BTreeMap<String, u64>,
    subtrees: BTreeMap<String, u64>,
    full: Option<FullScanCause>,
    gaps: Vec<(String, String)>,
}

fn is_inside(path: &str, dir: &str) -> bool {
    dir.is_empty()
        || path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

impl HintQueue {
    /// 建立空队列。
    #[must_use]
    pub fn new(limits: HintLimits) -> Self {
        HintQueue {
            limits,
            paths: BTreeMap::new(),
            subtrees: BTreeMap::new(),
            full: None,
            gaps: Vec::new(),
        }
    }

    /// 排队中的路径与子树数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len() + self.subtrees.len()
    }

    /// 是否没有排队的线索（含待执行的全量补扫）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0 && self.full.is_none()
    }

    /// 已报告的未监视目录（路径与原因），只增不减，供诊断；它们只能靠周期扫描覆盖。
    #[must_use]
    pub fn watch_gaps(&self) -> &[(String, String)] {
        &self.gaps
    }

    /// 记下一条线索。`now_ms` 为单调毫秒读数。
    pub fn push(&mut self, hint: WatchHint, now_ms: u64) {
        match hint {
            WatchHint::Overflow => self.demand_full(FullScanCause::QueueOverflow),
            WatchHint::Changed(path) => self.push_path(path, now_ms),
            WatchHint::Subtree(path) => self.push_subtree(path, now_ms),
            WatchHint::Gap { path, detail } => {
                if !self.gaps.iter().any(|(known, _)| *known == path) {
                    self.gaps.push((path.clone(), detail));
                }
                self.push_path(path, now_ms);
            }
        }
    }

    /// 要求一次全量补扫（已排队的路径线索被它覆盖，丢弃）。
    pub fn demand_full(&mut self, cause: FullScanCause) {
        // 溢出比队列满更具体；其余先到先得。
        if self.full.is_none() || cause == FullScanCause::QueueOverflow {
            self.full = Some(cause);
        }
        self.paths.clear();
        self.subtrees.clear();
    }

    fn at_capacity(&self) -> bool {
        self.len() >= self.limits.max_paths
    }

    fn push_path(&mut self, path: String, now_ms: u64) {
        if self.full.is_some() {
            return;
        }
        if let Some((_, last)) = self
            .subtrees
            .iter_mut()
            .find(|(dir, _)| is_inside(&path, dir))
        {
            *last = now_ms;
            return;
        }
        if let Some(last) = self.paths.get_mut(&path) {
            *last = now_ms;
            return;
        }
        if self.at_capacity() {
            self.demand_full(FullScanCause::HintQueueFull);
            return;
        }
        self.paths.insert(path, now_ms);
    }

    fn push_subtree(&mut self, dir: String, now_ms: u64) {
        if self.full.is_some() {
            return;
        }
        if let Some((_, last)) = self
            .subtrees
            .iter_mut()
            .find(|(known, _)| is_inside(&dir, known))
        {
            *last = now_ms;
            return;
        }
        // 新子树覆盖其内已排队的路径与更小的子树。
        self.paths.retain(|path, _| !is_inside(path, &dir));
        self.subtrees.retain(|known, _| !is_inside(known, &dir));
        if self.at_capacity() {
            self.demand_full(FullScanCause::HintQueueFull);
            return;
        }
        self.subtrees.insert(dir, now_ms);
    }

    /// 取出已静默满去抖时间的线索；有待执行的全量补扫时**不等去抖**立即返回它。
    pub fn take_due(&mut self, now_ms: u64) -> DueHints {
        if let Some(cause) = self.full.take() {
            return DueHints {
                full_scan: Some(cause),
                ..DueHints::default()
            };
        }
        let debounce = self.limits.debounce_ms;
        let due = |last: &u64| now_ms.saturating_sub(*last) >= debounce;
        let paths: Vec<String> = self
            .paths
            .iter()
            .filter(|(_, last)| due(last))
            .map(|(path, _)| path.clone())
            .collect();
        let subtrees: Vec<String> = self
            .subtrees
            .iter()
            .filter(|(_, last)| due(last))
            .map(|(dir, _)| dir.clone())
            .collect();
        for path in &paths {
            self.paths.remove(path);
        }
        for dir in &subtrees {
            self.subtrees.remove(dir);
        }
        DueHints {
            full_scan: None,
            paths,
            subtrees,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(max: usize, debounce: u64) -> HintQueue {
        HintQueue::new(HintLimits::new(max, debounce).expect("合法"))
    }

    fn changed(path: &str) -> WatchHint {
        WatchHint::Changed(path.to_owned())
    }

    #[test]
    fn same_path_hints_merge_and_wait_for_quiet() {
        let mut q = queue(10, 100);
        q.push(changed("a.txt"), 0);
        q.push(changed("a.txt"), 60);
        assert_eq!(q.len(), 1);
        assert!(q.take_due(120).paths.is_empty(), "最后一次线索后未满 100ms");
        assert_eq!(q.take_due(160).paths, vec!["a.txt".to_owned()]);
        assert!(q.is_empty());
    }

    #[test]
    fn continuous_events_keep_deferring_but_other_paths_are_not_blocked() {
        let mut q = queue(10, 100);
        q.push(changed("busy.log"), 0);
        q.push(changed("quiet.txt"), 0);
        for now in [50, 100, 150] {
            q.push(changed("busy.log"), now);
        }
        let due = q.take_due(200);
        assert_eq!(due.paths, vec!["quiet.txt".to_owned()]);
        assert_eq!(q.len(), 1, "持续写入的路径仍在等待");
    }

    #[test]
    fn overflow_demands_an_immediate_full_scan_without_debounce_and_drops_path_hints() {
        let mut q = queue(10, 10_000);
        q.push(changed("a"), 0);
        q.push(WatchHint::Subtree("d".into()), 0);
        q.push(WatchHint::Overflow, 1);
        let due = q.take_due(2);
        assert_eq!(due.full_scan, Some(FullScanCause::QueueOverflow));
        assert!(due.paths.is_empty() && due.subtrees.is_empty());
        assert!(q.is_empty(), "全量补扫已取走");
        // 溢出之后到达的线索照常排队。
        q.push(changed("b"), 3);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn queue_is_bounded_and_degrades_to_a_full_scan_instead_of_dropping_hints() {
        let mut q = queue(2, 100);
        q.push(changed("a"), 0);
        q.push(changed("b"), 0);
        q.push(changed("c"), 0);
        assert_eq!(q.len(), 0, "超出容量时清空并退化");
        let due = q.take_due(1);
        assert_eq!(due.full_scan, Some(FullScanCause::HintQueueFull));
    }

    #[test]
    fn overflow_cause_is_not_downgraded_by_queue_full() {
        let mut q = queue(1, 100);
        q.push(changed("a"), 0);
        q.push(changed("b"), 0);
        q.push(WatchHint::Overflow, 0);
        assert_eq!(q.take_due(0).full_scan, Some(FullScanCause::QueueOverflow));
    }

    #[test]
    fn subtree_hint_absorbs_paths_inside_it_and_later_hints_under_it() {
        let mut q = queue(10, 100);
        q.push(changed("d/x.txt"), 0);
        q.push(changed("other.txt"), 0);
        q.push(WatchHint::Subtree("d".into()), 10);
        q.push(changed("d/y.txt"), 20);
        assert_eq!(q.len(), 2);
        let due = q.take_due(200);
        assert_eq!(due.subtrees, vec!["d".to_owned()]);
        assert_eq!(due.paths, vec!["other.txt".to_owned()]);
    }

    #[test]
    fn similar_prefixes_are_not_treated_as_inside() {
        let mut q = queue(10, 100);
        q.push(WatchHint::Subtree("d".into()), 0);
        q.push(changed("dd/file"), 0);
        assert_eq!(q.len(), 2, "dd 不在 d 之内");
    }

    #[test]
    fn watch_gap_is_recorded_for_diagnostics_and_the_directory_is_looked_at() {
        let mut q = queue(10, 100);
        q.push(
            WatchHint::Gap {
                path: "locked".into(),
                detail: "permission_denied".into(),
            },
            0,
        );
        assert_eq!(q.watch_gaps().len(), 1);
        assert_eq!(q.take_due(100).paths, vec!["locked".to_owned()]);
    }
}
