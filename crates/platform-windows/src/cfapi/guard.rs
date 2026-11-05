//! 释放前的写保护判定：「已观察 generation + 使用保护」的组合规则（纯逻辑，可在宿主测试）。
//!
//! design §8.4/§14.4 的边界是明确的：普通目录模式下没有“任何应用是否正在编辑”的可靠判定，
//! Windows 锁/共享标志只是证据的一部分，数据库里的 clean 标记也只是上次观察。因此判定必须
//! 保守：只有全部证据一致指向“本地内容等于已确认版本，且观察窗口内没有变化、没有人持有
//! 不兼容句柄”时才给出 [`Verdict::Clean`]；其余一律落到 `Dirty`/`Busy`/`Unknown`，调用方
//! 据此拒绝释放（T07：尚未上传的内容不可释放）。
//!
//! 本模块只做分类，不做 I/O：证据来自 [`super::release`]（`CfGetPlaceholderInfo`、
//! 共享模式探测、内容 SHA-256 与二次观察）。这样判定规则可以在宿主单测里逐条锁定。

/// 释放判定。
///
/// 只有 [`Verdict::Clean`] 允许继续调用平台释放（`CfSetInSyncState` + `CfDehydratePlaceholder`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 本地内容等于已确认版本、观察窗口内未变化、没有不兼容句柄持有者。
    Clean,
    /// 本地内容与已确认版本不同：存在尚未上传的本地修改。
    Dirty,
    /// 有人正在使用（持有不共享的句柄），或内容在观察窗口内被改动。
    Busy,
    /// 证据不足：不是占位符、已脱水、被 pin、探测失败或内容无法核对。
    Unknown,
}

impl Verdict {
    /// 日志/命令行里的名字；`release` 的输出与客体脚本断言共用这一处拼写。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Dirty => "dirty",
            Self::Busy => "busy",
            Self::Unknown => "unknown",
        }
    }
}

/// 一次共享模式探测的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// 打开成功：没有与之冲突的其它句柄。
    Opened,
    /// 打开失败，附带 `GetLastError` 的 Win32 码。
    Failed(u32),
    /// 没有执行（条目已脱水时不探测，以免探测本身触发水合）。
    Skipped,
}

impl ProbeOutcome {
    /// 失败时的 Win32 码。
    pub const fn error_code(self) -> Option<u32> {
        match self {
            Self::Failed(code) => Some(code),
            Self::Opened | Self::Skipped => None,
        }
    }

    /// 日志字段值：`opened` / `skipped` / `error:<code>`。
    pub fn label(self) -> String {
        match self {
            Self::Opened => "opened".to_string(),
            Self::Skipped => "skipped".to_string(),
            Self::Failed(code) => format!("error:{code}"),
        }
    }
}

/// Win32 `ERROR_SHARING_VIOLATION`（`winerror.h`）：共享模式与已有句柄冲突。
pub const ERROR_SHARING_VIOLATION: u32 = 32;
/// Win32 `ERROR_LOCK_VIOLATION`（`winerror.h`）：字节范围锁冲突。
pub const ERROR_LOCK_VIOLATION: u32 = 33;

/// `CF_PIN_STATE_UNSPECIFIED`
pub const CF_PIN_STATE_UNSPECIFIED: u32 = 0;
/// `CF_PIN_STATE_PINNED`：显式 pin 住，平台不允许脱水。
pub const CF_PIN_STATE_PINNED: u32 = 1;
/// `CF_PIN_STATE_UNPINNED`
pub const CF_PIN_STATE_UNPINNED: u32 = 2;
/// `CF_PIN_STATE_PINNED_RECURSIVE`：目录递归 pin，同样不允许脱水。
pub const CF_PIN_STATE_PINNED_RECURSIVE: u32 = 3;
/// `CF_PIN_STATE_UNPINNED_RECURSIVE`
pub const CF_PIN_STATE_UNPINNED_RECURSIVE: u32 = 4;

/// 使用保护探测：三个共享模式组合。
///
/// 共享检查是双向的：新打开请求既要满足自己的共享模式，也要与已有句柄的共享模式兼容。
/// 因此 `dwShareMode=0` 的探测能暴露“持有者不允许别人拿这种访问权限”的情形，但它**看不到**
/// 完全共享的读者/写者——这是 design §14.4 说的“只是证据的一部分”。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageProbe {
    /// `GENERIC_READ` + `dwShareMode=0`：失败说明有人持有不允许读共享的句柄。
    pub exclusive_read: ProbeOutcome,
    /// `GENERIC_WRITE` + `dwShareMode=0`：失败说明有人持有不允许写共享的句柄（如只读共享锁）。
    pub exclusive_write: ProbeOutcome,
    /// `GENERIC_READ` + 全共享：失败说明文件被独占（别人连读都不共享）。
    pub shared_read: ProbeOutcome,
}

impl UsageProbe {
    /// 没有执行任何探测（条目已脱水）。
    pub const fn skipped() -> Self {
        Self {
            exclusive_read: ProbeOutcome::Skipped,
            exclusive_write: ProbeOutcome::Skipped,
            shared_read: ProbeOutcome::Skipped,
        }
    }

    /// 三个探测的日志字段。
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        vec![
            ("probe_read_exclusive", self.exclusive_read.label()),
            ("probe_write_exclusive", self.exclusive_write.label()),
            ("probe_shared_read", self.shared_read.label()),
        ]
    }
}

/// 内容核对结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentCheck {
    /// 本地内容 SHA-256 等于固定内容版本的期望值。
    Matched,
    /// 本地内容与期望值不同：有本地修改（或内容被其它进程改过）。
    Mismatched,
    /// 无法核对（不是原型条目、读取失败或条目已脱水）。
    Unverified,
}

impl ContentCheck {
    /// 日志字段值。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::Mismatched => "mismatched",
            Self::Unverified => "unverified",
        }
    }
}

/// 从系统读到的、与“代际”有关的状态（`CfGetPlaceholderInfo`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    /// 系统是否把它认作占位符。
    pub is_placeholder: bool,
    /// `CF_PIN_STATE_*`
    pub pin_state: u32,
    /// `CF_IN_SYNC_STATE_*`
    pub in_sync_state: u32,
    /// 本地实际数据字节数。
    pub on_disk_data_size: i64,
    /// 逻辑大小。
    pub logical_size: i64,
    /// `CF_PLACEHOLDER_STANDARD_INFO::ModifiedDataSize`
    pub modified_data_size: i64,
}

/// 判定输入。
#[derive(Clone, Copy, Debug)]
pub struct Inputs<'a> {
    /// 最近一次观察（内容哈希之后）。
    pub observation: &'a Observation,
    /// 使用保护探测。
    pub usage: &'a UsageProbe,
    /// 内容核对结论。
    pub content: ContentCheck,
    /// 两次内容哈希是否一致（观察窗口内内容没有变化）。
    pub content_stable: bool,
}

/// 判定结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    /// 判定。
    pub verdict: Verdict,
    /// 判定理由（稳定拼写，写进日志并由客体脚本断言）。
    pub reason: &'static str,
}

impl Decision {
    const fn new(verdict: Verdict, reason: &'static str) -> Self {
        Self { verdict, reason }
    }
}

/// 判定 `Clean`/`Dirty`/`Busy`/`Unknown`。
///
/// 规则顺序即保守程度顺序：任何一条无法证明的前置条件都会先返回，只有全部通过才是 `Clean`。
pub fn evaluate(input: &Inputs<'_>) -> Decision {
    let observation = input.observation;
    if !observation.is_placeholder {
        return Decision::new(Verdict::Unknown, "not_placeholder");
    }
    if observation.on_disk_data_size <= 0 {
        return Decision::new(Verdict::Unknown, "already_dehydrated");
    }
    if observation.pin_state == CF_PIN_STATE_PINNED
        || observation.pin_state == CF_PIN_STATE_PINNED_RECURSIVE
    {
        return Decision::new(Verdict::Unknown, "pinned");
    }
    let usage = input.usage;
    if let Some(decision) =
        probe_decision(usage.exclusive_read, "handle_held_read", "probe_read_error")
    {
        return decision;
    }
    if let Some(decision) = probe_decision(
        usage.exclusive_write,
        "handle_held_write",
        "probe_write_error",
    ) {
        return decision;
    }
    if let Some(decision) =
        probe_decision(usage.shared_read, "handle_exclusive", "probe_shared_error")
    {
        return decision;
    }
    if usage.exclusive_read == ProbeOutcome::Skipped
        || usage.exclusive_write == ProbeOutcome::Skipped
        || usage.shared_read == ProbeOutcome::Skipped
    {
        return Decision::new(Verdict::Unknown, "usage_unprobed");
    }
    if observation.on_disk_data_size < observation.logical_size {
        return Decision::new(Verdict::Unknown, "content_not_local");
    }
    match input.content {
        ContentCheck::Mismatched => return Decision::new(Verdict::Dirty, "content_mismatch"),
        ContentCheck::Unverified => return Decision::new(Verdict::Unknown, "content_unverified"),
        ContentCheck::Matched => {}
    }
    if !input.content_stable {
        return Decision::new(Verdict::Busy, "content_changed");
    }
    Decision::new(Verdict::Clean, "content_matches_committed_version")
}

/// 单个探测的分类：共享冲突 ⇒ `Busy`，其它失败 ⇒ `Unknown`。
fn probe_decision(
    outcome: ProbeOutcome,
    held_reason: &'static str,
    error_reason: &'static str,
) -> Option<Decision> {
    match outcome {
        ProbeOutcome::Opened | ProbeOutcome::Skipped => None,
        ProbeOutcome::Failed(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION) => {
            Some(Decision::new(Verdict::Busy, held_reason))
        }
        ProbeOutcome::Failed(_) => Some(Decision::new(Verdict::Unknown, error_reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CF_PIN_STATE_PINNED, CF_PIN_STATE_UNPINNED, ContentCheck, Inputs, Observation,
        ProbeOutcome, UsageProbe, Verdict, evaluate,
    };

    fn hydrated(pin_state: u32) -> Observation {
        Observation {
            is_placeholder: true,
            pin_state,
            in_sync_state: 0,
            on_disk_data_size: 12_345,
            logical_size: 12_345,
            modified_data_size: 0,
        }
    }

    fn clean_usage() -> UsageProbe {
        UsageProbe {
            exclusive_read: ProbeOutcome::Opened,
            exclusive_write: ProbeOutcome::Opened,
            shared_read: ProbeOutcome::Opened,
        }
    }

    fn decide(
        observation: &Observation,
        usage: &UsageProbe,
        content: ContentCheck,
        stable: bool,
    ) -> (Verdict, &'static str) {
        let inputs = Inputs {
            observation,
            usage,
            content,
            content_stable: stable,
        };
        let decision = evaluate(&inputs);
        (decision.verdict, decision.reason)
    }

    #[test]
    fn clean_content_without_holders_is_releasable() {
        let observation = hydrated(CF_PIN_STATE_UNPINNED);
        assert_eq!(
            decide(&observation, &clean_usage(), ContentCheck::Matched, true),
            (Verdict::Clean, "content_matches_committed_version")
        );
    }

    #[test]
    fn locally_modified_content_is_dirty_and_never_clean() {
        let observation = hydrated(CF_PIN_STATE_UNPINNED);
        assert_eq!(
            decide(&observation, &clean_usage(), ContentCheck::Mismatched, true),
            (Verdict::Dirty, "content_mismatch")
        );
    }

    #[test]
    fn unverified_content_is_unknown() {
        let observation = hydrated(CF_PIN_STATE_UNPINNED);
        assert_eq!(
            decide(&observation, &clean_usage(), ContentCheck::Unverified, true),
            (Verdict::Unknown, "content_unverified")
        );
    }

    #[test]
    fn held_handles_are_busy() {
        let observation = hydrated(CF_PIN_STATE_UNPINNED);
        let mut usage = clean_usage();
        usage.exclusive_read = ProbeOutcome::Failed(super::ERROR_SHARING_VIOLATION);
        assert_eq!(
            decide(&observation, &usage, ContentCheck::Matched, true),
            (Verdict::Busy, "handle_held_read")
        );

        let mut usage = clean_usage();
        usage.exclusive_write = ProbeOutcome::Failed(super::ERROR_LOCK_VIOLATION);
        assert_eq!(
            decide(&observation, &usage, ContentCheck::Matched, true),
            (Verdict::Busy, "handle_held_write")
        );

        let mut usage = clean_usage();
        usage.shared_read = ProbeOutcome::Failed(super::ERROR_SHARING_VIOLATION);
        assert_eq!(
            decide(&observation, &usage, ContentCheck::Matched, true),
            (Verdict::Busy, "handle_exclusive")
        );
    }

    #[test]
    fn probe_errors_cannot_be_read_as_clean() {
        let observation = hydrated(CF_PIN_STATE_UNPINNED);
        let mut usage = clean_usage();
        usage.exclusive_read = ProbeOutcome::Failed(5);
        assert_eq!(
            decide(&observation, &usage, ContentCheck::Matched, true),
            (Verdict::Unknown, "probe_read_error")
        );
    }

    #[test]
    fn skipped_probes_are_unknown() {
        let observation = hydrated(CF_PIN_STATE_UNPINNED);
        assert_eq!(
            decide(
                &observation,
                &UsageProbe::skipped(),
                ContentCheck::Matched,
                true
            ),
            (Verdict::Unknown, "usage_unprobed")
        );
    }

    #[test]
    fn content_changed_during_observation_is_busy() {
        let observation = hydrated(CF_PIN_STATE_UNPINNED);
        assert_eq!(
            decide(&observation, &clean_usage(), ContentCheck::Matched, false),
            (Verdict::Busy, "content_changed")
        );
    }

    #[test]
    fn dehydrated_pinned_and_partial_entries_are_not_releasable() {
        let mut observation = hydrated(CF_PIN_STATE_UNPINNED);
        observation.on_disk_data_size = 0;
        assert_eq!(
            decide(
                &observation,
                &UsageProbe::skipped(),
                ContentCheck::Unverified,
                true
            ),
            (Verdict::Unknown, "already_dehydrated")
        );

        let observation = hydrated(CF_PIN_STATE_PINNED);
        assert_eq!(
            decide(&observation, &clean_usage(), ContentCheck::Matched, true),
            (Verdict::Unknown, "pinned")
        );

        // 只有部分区间在本地（范围水合后）：内容哈希无法代表整份文件。
        let mut observation = hydrated(CF_PIN_STATE_UNPINNED);
        observation.on_disk_data_size = 1_052_672;
        observation.logical_size = 16_777_216;
        assert_eq!(
            decide(&observation, &clean_usage(), ContentCheck::Matched, true),
            (Verdict::Unknown, "content_not_local")
        );
    }

    #[test]
    fn non_placeholder_is_unknown() {
        let mut observation = hydrated(CF_PIN_STATE_UNPINNED);
        observation.is_placeholder = false;
        assert_eq!(
            decide(&observation, &clean_usage(), ContentCheck::Matched, true),
            (Verdict::Unknown, "not_placeholder")
        );
    }

    #[test]
    fn verdict_and_probe_labels_are_stable() {
        assert_eq!(Verdict::Clean.name(), "clean");
        assert_eq!(Verdict::Dirty.name(), "dirty");
        assert_eq!(Verdict::Busy.name(), "busy");
        assert_eq!(Verdict::Unknown.name(), "unknown");
        assert_eq!(ContentCheck::Matched.name(), "matched");
        assert_eq!(ProbeOutcome::Opened.label(), "opened");
        assert_eq!(ProbeOutcome::Skipped.label(), "skipped");
        assert_eq!(ProbeOutcome::Failed(32).label(), "error:32");
    }
}
