//! 有界、可续扫的目录枚举与稳定窗口（design §12.1、§12.2、C33）。
//!
//! 两件事都是**纯规则**，所有文件系统访问经 [`WorkTreePort`]：
//!
//! 1. [`scan_tree`]：广度优先、按名称字节序枚举普通文件，并把“这次到底扫全没有”说清楚。预算
//!    （[`ScanLimits::max_entries`]）耗尽、目录读不了、Root 不可用都会让结论变成
//!    [`ScanCompletion::Inconclusive`]，并带回 [`ScanCursor`]：下次从游标继续，**不会**从头再来，也
//!    **不会**把已枚举到的部分当成完整快照。读不到不等于不存在：枚举从不产生“删除”结论
//!    （根 `AGENTS.md`、design §11.5）。
//! 2. [`StabilityTracker`]：按指纹判断“连续一个窗口没有变化”。指纹含 inode/创建时间/大小/`ctime`，
//!    不以 `mtime` 决胜；它只降低读到中间状态的概率，不证明外部程序已停止写入（design §12.2）。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crate::ports::work_tree::{
    FileStamp, WorkEntryKind, WorkObservation, WorkTreeError, WorkTreePort,
};

/// 一次扫描的预算。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanLimits {
    max_entries: usize,
}

/// 预算不成立。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidScanLimits;

impl fmt::Display for InvalidScanLimits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("扫描预算必须大于 0，否则扫描永远无法推进")
    }
}

impl std::error::Error for InvalidScanLimits {}

impl ScanLimits {
    /// 一次扫描最多检查的目录项数（含文件、目录与被跳过的项）。
    ///
    /// # Errors
    ///
    /// 预算为 0 时返回 [`InvalidScanLimits`]。
    pub fn new(max_entries: usize) -> Result<Self, InvalidScanLimits> {
        if max_entries == 0 {
            return Err(InvalidScanLimits);
        }
        Ok(ScanLimits { max_entries })
    }

    /// 预算值。
    #[must_use]
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }
}

/// 待继续的一个目录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDir {
    /// 目录的相对路径（空串为 Root 自身）。
    pub path: String,
    /// 名称不大于它的项已处理（再次列目录时跳过，`retry` 里的除外）。
    pub after: Option<String>,
    /// 上次读取失败、需要重新检查的项名。
    pub retry: BTreeSet<String>,
}

impl PendingDir {
    fn fresh(path: String) -> Self {
        PendingDir {
            path,
            after: None,
            retry: BTreeSet::new(),
        }
    }

    fn wants(&self, name: &str) -> bool {
        match &self.after {
            Some(after) if name <= after.as_str() => self.retry.contains(name),
            _ => true,
        }
    }
}

/// 续扫游标：下一次扫描要处理的目录队列。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanCursor {
    dirs: VecDeque<PendingDir>,
}

impl ScanCursor {
    /// 从 Root 起点开始的新扫描。
    #[must_use]
    pub fn from_root() -> Self {
        ScanCursor {
            dirs: VecDeque::from([PendingDir::fresh(String::new())]),
        }
    }

    /// 还要处理的目录数。
    #[must_use]
    pub fn pending_dirs(&self) -> usize {
        self.dirs.len()
    }

    /// 待处理目录（只读，测试与诊断用）。
    pub fn dirs(&self) -> impl Iterator<Item = &PendingDir> {
        self.dirs.iter()
    }
}

/// 枚举到的普通文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedFile {
    /// 相对路径。
    pub path: String,
    /// 枚举时的指纹。
    pub stamp: FileStamp,
}

/// 被跳过的项：不属于同步范围，但必须可见，不能静默消失。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// 符号链接、设备等不受支持的类型。
    UnsupportedKind(WorkEntryKind),
    /// 名字不是合法 UTF-8，无法表示（数量）。
    UnrepresentableNames(usize),
}

/// 被跳过的项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedEntry {
    /// 所在目录或项的相对路径。
    pub path: String,
    /// 原因。
    pub reason: SkipReason,
}

/// 扫描结论不完整的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InconclusiveReason {
    /// 本次预算用尽，尚有目录项未检查。
    BudgetExhausted {
        /// 本次预算。
        limit: usize,
    },
    /// 目录读不了（权限、IO）：不是“目录为空”。
    DirectoryUnreadable {
        /// 目录相对路径。
        path: String,
        /// 原始错误文本。
        detail: String,
    },
    /// 某一项读不了：不是“项不存在”。
    EntryUnreadable {
        /// 项相对路径。
        path: String,
        /// 原始错误文本。
        detail: String,
    },
    /// Root 不可用（挂载漂移等）：扫描停止，不推导任何删除。
    RootUnavailable {
        /// 原因文本。
        detail: String,
    },
}

impl fmt::Display for InconclusiveReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InconclusiveReason::BudgetExhausted { limit } => {
                write!(f, "扫描预算 {limit} 项已用尽")
            }
            InconclusiveReason::DirectoryUnreadable { path, detail } => {
                write!(f, "目录 {path:?} 不可读：{detail}")
            }
            InconclusiveReason::EntryUnreadable { path, detail } => {
                write!(f, "项 {path:?} 不可读：{detail}")
            }
            InconclusiveReason::RootUnavailable { detail } => write!(f, "Root 不可用：{detail}"),
        }
    }
}

/// 扫描是否覆盖了完整的树。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanCompletion {
    /// 本次（含续扫）走完了游标里的全部目录，没有读取失败：目录树的受支持文件已枚举完。
    Complete,
    /// 没有走完或有读取失败：已枚举到的部分**不是**完整快照，必须按游标继续或重试。
    Inconclusive {
        /// 下一次扫描的起点（含预算外剩余与需要重试的部分）。
        resume: ScanCursor,
        /// 原因（至少一项）。
        reasons: Vec<InconclusiveReason>,
    },
}

impl ScanCompletion {
    /// 是否完整。
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self, ScanCompletion::Complete)
    }
}

/// 一次扫描的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanReport {
    /// 本次枚举到的普通文件（按访问顺序）。
    pub files: Vec<ScannedFile>,
    /// 被跳过的项。
    pub skipped: Vec<SkippedEntry>,
    /// 本次检查的目录项数。
    pub entries_examined: usize,
    /// 完整与否。
    pub completion: ScanCompletion,
}

impl ScanReport {
    /// 是否因 Root 不可用而中止。
    #[must_use]
    pub fn root_unavailable(&self) -> bool {
        match &self.completion {
            ScanCompletion::Complete => false,
            ScanCompletion::Inconclusive { reasons, .. } => reasons
                .iter()
                .any(|reason| matches!(reason, InconclusiveReason::RootUnavailable { .. })),
        }
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// 从游标开始做一次有界扫描。
///
/// 目录按队列先进先出访问，目录内按名称字节序；预算在**每个目录项**之前检查，因此可以在目录中途
/// 停下，并用 [`PendingDir::after`] 精确续扫。任何读取失败都进入 `reasons` 并保留在续扫游标里，
/// 绝不产生“删除”结论：本函数不返回任何“消失”的项。
pub fn scan_tree(
    port: &mut dyn WorkTreePort,
    limits: ScanLimits,
    cursor: ScanCursor,
) -> ScanReport {
    let mut queue = cursor.dirs;
    let mut retry_dirs: Vec<PendingDir> = Vec::new();
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut reasons = Vec::new();
    let mut examined = 0usize;

    'dirs: while let Some(mut pending) = queue.pop_front() {
        if examined >= limits.max_entries {
            queue.push_front(pending);
            reasons.push(InconclusiveReason::BudgetExhausted {
                limit: limits.max_entries,
            });
            break;
        }
        let listing = match port.list_dir(&pending.path) {
            Ok(listing) => listing,
            Err(WorkTreeError::RootUnavailable { detail }) => {
                queue.push_front(pending);
                reasons.push(InconclusiveReason::RootUnavailable { detail });
                break;
            }
            Err(error) => {
                reasons.push(InconclusiveReason::DirectoryUnreadable {
                    path: pending.path.clone(),
                    detail: error.to_string(),
                });
                retry_dirs.push(pending);
                continue;
            }
        };
        if listing.unrepresentable_names > 0 {
            skipped.push(SkippedEntry {
                path: pending.path.clone(),
                reason: SkipReason::UnrepresentableNames(listing.unrepresentable_names),
            });
        }
        let mut entries = listing.entries;
        entries.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
        let mut failed_names: BTreeSet<String> = BTreeSet::new();
        let mut last_processed = pending.after.clone();
        for entry in entries {
            if !pending.wants(&entry.name) {
                continue;
            }
            if examined >= limits.max_entries {
                pending.after = last_processed;
                pending.retry = failed_names;
                queue.push_front(pending);
                reasons.push(InconclusiveReason::BudgetExhausted {
                    limit: limits.max_entries,
                });
                break 'dirs;
            }
            examined += 1;
            let path = join(&pending.path, &entry.name);
            match entry.kind {
                WorkEntryKind::Directory => queue.push_back(PendingDir::fresh(path)),
                WorkEntryKind::File => match port.observe(&path) {
                    Ok(WorkObservation::File(stamp)) => files.push(ScannedFile { path, stamp }),
                    // 列目录与观察之间被改名/删除：它没有被采集过，没有历史可保护，也没有可删除的
                    // 已采纳条目；若后来又出现，下一轮扫描会发现。
                    Ok(WorkObservation::Absent) => {}
                    Ok(WorkObservation::NotRegular(kind)) => skipped.push(SkippedEntry {
                        path,
                        reason: SkipReason::UnsupportedKind(kind),
                    }),
                    Err(WorkTreeError::RootUnavailable { detail }) => {
                        pending.after = last_processed;
                        pending.retry = failed_names;
                        pending.retry.insert(entry.name);
                        queue.push_front(pending);
                        reasons.push(InconclusiveReason::RootUnavailable { detail });
                        break 'dirs;
                    }
                    Err(error) => {
                        reasons.push(InconclusiveReason::EntryUnreadable {
                            path,
                            detail: error.to_string(),
                        });
                        failed_names.insert(entry.name.clone());
                    }
                },
                kind => skipped.push(SkippedEntry {
                    path,
                    reason: SkipReason::UnsupportedKind(kind),
                }),
            }
            // 重试的旧名字不能把“已处理到哪里”往回拉。
            if last_processed
                .as_deref()
                .is_none_or(|last| entry.name.as_str() > last)
            {
                last_processed = Some(entry.name);
            }
        }
        if !failed_names.is_empty() {
            retry_dirs.push(PendingDir {
                path: pending.path,
                after: last_processed,
                retry: failed_names,
            });
        }
    }

    queue.extend(retry_dirs);
    let completion = if reasons.is_empty() {
        ScanCompletion::Complete
    } else {
        ScanCompletion::Inconclusive {
            resume: ScanCursor { dirs: queue },
            reasons,
        }
    };
    ScanReport {
        files,
        skipped,
        entries_examined: examined,
        completion,
    }
}

/// 稳定窗口。设计取值 500—1000 ms（design §23 表“外部稳定观察”）；任何窗口都不能证明外部写入已结束。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StabilityWindow {
    millis: u64,
}

impl StabilityWindow {
    /// 设计默认值：1000 ms。
    pub const DEFAULT: StabilityWindow = StabilityWindow { millis: 1000 };

    /// 以毫秒构造；0 会让“第一次看到就稳定”，等于没有窗口，因此拒绝。
    ///
    /// # Errors
    ///
    /// 0 毫秒返回 [`InvalidStabilityWindow`]。
    pub fn from_millis(millis: u64) -> Result<Self, InvalidStabilityWindow> {
        if millis == 0 {
            return Err(InvalidStabilityWindow);
        }
        Ok(StabilityWindow { millis })
    }

    /// 毫秒数。
    #[must_use]
    pub fn millis(&self) -> u64 {
        self.millis
    }
}

/// 稳定窗口不成立。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidStabilityWindow;

impl fmt::Display for InvalidStabilityWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("稳定窗口必须大于 0 毫秒")
    }
}

impl std::error::Error for InvalidStabilityWindow {}

/// 一次稳定性判断。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stability {
    /// 指纹在整个窗口内没有变化，可以采集。
    Stable,
    /// 还在等待文件写入完成（用户可见状态，不传播部分片段）。
    Waiting {
        /// 指纹自上次变化起保持不变的毫秒数。
        quiet_ms: u64,
        /// 距离稳定还需的毫秒数。
        remaining_ms: u64,
        /// 与上次观察不同的字段；首次看到时为空。
        changed: Vec<&'static str>,
    },
}

struct Tracked {
    stamp: FileStamp,
    quiet_since_ms: u64,
}

/// 按指纹判断“连续一个窗口没变”。
pub struct StabilityTracker {
    window: StabilityWindow,
    tracked: BTreeMap<String, Tracked>,
}

impl StabilityTracker {
    /// 以窗口建立空跟踪器。
    #[must_use]
    pub fn new(window: StabilityWindow) -> Self {
        StabilityTracker {
            window,
            tracked: BTreeMap::new(),
        }
    }

    /// 记录一次观察并给出结论。`now_ms` 必须是单调时钟读数（毫秒）。
    ///
    /// 指纹变化（含只有 `ctime` 变化、`mtime` 被保留的情形）会重新计时；首次看到总是等待一个完整
    /// 窗口——即使文件看起来很旧，因为 `mtime` 不是可信的稳定证据。
    pub fn observe(&mut self, path: &str, stamp: FileStamp, now_ms: u64) -> Stability {
        let window = self.window.millis;
        match self.tracked.get_mut(path) {
            None => {
                self.tracked.insert(
                    path.to_owned(),
                    Tracked {
                        stamp,
                        quiet_since_ms: now_ms,
                    },
                );
                Stability::Waiting {
                    quiet_ms: 0,
                    remaining_ms: window,
                    changed: Vec::new(),
                }
            }
            Some(tracked) => {
                let changed = tracked.stamp.changed_fields(&stamp);
                if !changed.is_empty() {
                    tracked.stamp = stamp;
                    tracked.quiet_since_ms = now_ms;
                    return Stability::Waiting {
                        quiet_ms: 0,
                        remaining_ms: window,
                        changed,
                    };
                }
                let quiet_ms = now_ms.saturating_sub(tracked.quiet_since_ms);
                if quiet_ms >= window {
                    Stability::Stable
                } else {
                    Stability::Waiting {
                        quiet_ms,
                        remaining_ms: window - quiet_ms,
                        changed: Vec::new(),
                    }
                }
            }
        }
    }

    /// 忘记路径（已采纳、已消失或需要重新计时）。
    pub fn forget(&mut self, path: &str) {
        self.tracked.remove(path);
    }

    /// 只保留 `keep` 返回真的路径（完整扫描后清理已不存在的路径）。
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.tracked.retain(|path, _| keep(path));
    }

    /// 正在跟踪的路径数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.tracked.len()
    }

    /// 是否没有跟踪任何路径。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracked.is_empty()
    }
}

#[cfg(test)]
mod tests;
