//! 外部移动、删除与挂载失效的规则（design §9.5、§12.3、T17/T19/T35）。
//!
//! 本模块只有**纯规则**：什么算一次移动、什么时候可以推导删除、什么情况必须进保护队列。真正的
//! 观察与提交在 [`crate::observer::import::ExternalObserver`] 里，平台 I/O 在适配器里。
//!
//! 三条边界（顺序即优先级）：
//!
//! 1. **扫描不完整或不健康就不推导任何删除**：`permission_denied`、`io_error`、`mount_changed`、
//!    预算耗尽的扫描里“没看到”只说明我们没看到（design §12.3）。卸载后路径下露出空目录属于
//!    `mount_changed`（Vault 不可用），零删除事件；
//! 2. **移动优先用文件身份（设备 + inode + 创建时间）与配对的改名证据**，内容哈希只作辅助。两份内容
//!    相同的文件互换名字时，按哈希配对会串掉 EntryId，因此[`pair_moves`]只认身份，且一对一不歧义
//!    （同一身份出现多次 = 硬链接等歧义情形，按独立新增/删除处理）；
//! 3. **大批量缺失先保护**：数量超过单轮上限或占已保护条目比例过高时，整批进保护队列，附原基线、
//!    目录身份与计数供复核，由人工用 [`crate::observer::import::ExternalObserver::release_protected`]
//!    放行（一次性、按路径与条目身份）。

use std::collections::{BTreeMap, BTreeSet};

use vaultsyncd_domain::ids::EntryId;
use vaultsyncd_domain::version::Revision;

use crate::objects::ExpectedContent;
use crate::observer::scan::ScanHealth;
use crate::ports::object_store::FileKey;
use crate::ports::work_tree::DirIdentity;
use crate::ports::work_tree::FileStamp;

/// 核对（移动/删除/保护）的限额。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconcileLimits {
    /// 单轮允许直接确认的外部删除数上限。
    pub max_deletes_per_cycle: usize,
    /// 单轮删除数占已保护条目的百分比上限（0—100）。
    pub max_delete_percent: u32,
    /// 保护队列里同时保留的批次数上限。
    pub protection_capacity: usize,
}

/// 占比规则的样本下限：已保护条目少于这个数时只看单轮数量上限。
///
/// 小集合里“删掉 1 个”就是 50%，比例没有统计意义；用数量上限即可，否则每次删一个文件都要人工复核。
pub const SHARE_MIN_TOTAL: usize = 16;

/// 限额不成立。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidReconcileLimits {
    /// 单轮删除上限为 0：等于永远不传播外部删除。
    ZeroDeletes,
    /// 百分比超过 100。
    PercentOutOfRange,
    /// 保护队列容量为 0：异常批量删除无处可放。
    ZeroCapacity,
}

impl std::fmt::Display for InvalidReconcileLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InvalidReconcileLimits::ZeroDeletes => f.write_str("单轮删除上限必须大于 0"),
            InvalidReconcileLimits::PercentOutOfRange => f.write_str("删除占比上限必须在 0—100"),
            InvalidReconcileLimits::ZeroCapacity => f.write_str("保护队列容量必须大于 0"),
        }
    }
}

impl std::error::Error for InvalidReconcileLimits {}

impl ReconcileLimits {
    /// 默认值：单轮最多 32 个删除、占比不超过 10%、保护队列 8 批。比例与数量都是保守起点，
    /// 部署侧可按 Vault 规模显式调大（design §23 的调参风格）。
    pub const DEFAULT: ReconcileLimits = ReconcileLimits {
        max_deletes_per_cycle: 32,
        max_delete_percent: 10,
        protection_capacity: 8,
    };

    /// 构造。
    ///
    /// # Errors
    ///
    /// 见 [`InvalidReconcileLimits`]。
    pub fn new(
        max_deletes_per_cycle: usize,
        max_delete_percent: u32,
        protection_capacity: usize,
    ) -> Result<Self, InvalidReconcileLimits> {
        if max_deletes_per_cycle == 0 {
            return Err(InvalidReconcileLimits::ZeroDeletes);
        }
        if max_delete_percent > 100 {
            return Err(InvalidReconcileLimits::PercentOutOfRange);
        }
        if protection_capacity == 0 {
            return Err(InvalidReconcileLimits::ZeroCapacity);
        }
        Ok(ReconcileLimits {
            max_deletes_per_cycle,
            max_delete_percent,
            protection_capacity,
        })
    }
}

/// 一个已采纳、但本轮完整扫描里没再出现的条目（删除候选的输入）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingEntry {
    /// 上次采纳时的路径。
    pub path: String,
    /// 条目身份。
    pub entry: EntryId,
    /// 上次采纳时的 revision。
    pub revision: Revision,
    /// 上次采纳时的文件身份（设备、inode、链接数、长度）。
    pub identity: FileKey,
    /// 上次采纳时的创建时间（与 `identity` 一起判断“同一个文件对象”）。
    pub btime: Option<(i64, u32)>,
    /// 上次采纳时的内容摘要与长度。
    pub content: ExpectedContent,
}

impl MissingEntry {
    /// 是否是同一个文件对象：设备、inode 与创建时间都相同。
    #[must_use]
    pub fn same_object(&self, stamp: &FileStamp) -> bool {
        self.identity.same_inode(&stamp.key) && self.btime == stamp.btime
    }
}

/// 一次移动的证据：同一文件对象从 `from` 出现在 `to`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveCandidate {
    /// 原路径上的条目。
    pub missing: MissingEntry,
    /// 新路径现在的指纹（与 `missing` 的基线是同一文件对象）。
    pub stamp: FileStamp,
    /// 新路径。
    pub to: String,
}

/// 文件对象身份：设备、inode 与创建时间（内容不参与，哈希只作辅助）。
type ObjectKey = (u64, u64, Option<(i64, u32)>);

/// 一对一配对移动。返回（可信的移动、剩余的缺失、因歧义放弃配对的路径）。
///
/// `seen_new` 是本轮扫描里**尚未采纳**的路径 → 指纹；已采纳的路径不可能是“移动目标”。
/// 只有“同一文件对象一模一样，且两个方向都唯一”才配对：
///
/// - 两份同内容文件互换名字 → 各自 inode 不同，按身份仍能正确配对（按哈希会串）；
/// - 一个缺失条目对上多个新路径（硬链接或复制）→ 歧义，不猜，两边都不算移动；
/// - 多个缺失条目共用一个身份（硬链接）→ 同样歧义。
#[must_use]
pub fn pair_moves(
    missing: &[MissingEntry],
    seen_new: &BTreeMap<String, FileStamp>,
) -> (Vec<MoveCandidate>, Vec<MissingEntry>, BTreeSet<String>) {
    let mut by_identity: BTreeMap<ObjectKey, Vec<usize>> = BTreeMap::new();
    for (index, entry) in missing.iter().enumerate() {
        by_identity
            .entry((entry.identity.device, entry.identity.inode, entry.btime))
            .or_default()
            .push(index);
    }
    let mut new_by_identity: BTreeMap<ObjectKey, Vec<&String>> = BTreeMap::new();
    for (path, stamp) in seen_new {
        new_by_identity
            .entry((stamp.key.device, stamp.key.inode, stamp.btime))
            .or_default()
            .push(path);
    }

    let mut moves = Vec::new();
    let mut paired: BTreeSet<usize> = BTreeSet::new();
    let mut ambiguous_paths: BTreeSet<String> = BTreeSet::new();
    let mut ambiguous_entries: BTreeSet<usize> = BTreeSet::new();

    for (identity, indices) in &by_identity {
        let Some(destinations) = new_by_identity.get(identity).cloned() else {
            // 这个身份没有出现在任何新路径上：没有移动证据，条目照旧算缺失（不是歧义）。
            continue;
        };
        if indices.len() == 1 && destinations.len() == 1 {
            let index = indices[0];
            let to = destinations[0].clone();
            let stamp = seen_new[&to];
            if missing[index].same_object(&stamp) {
                paired.insert(index);
                moves.push(MoveCandidate {
                    missing: missing[index].clone(),
                    stamp,
                    to,
                });
                continue;
            }
        }
        // 歧义：不凭身份“猜”哪一对是一对。
        for index in indices {
            ambiguous_entries.insert(*index);
            ambiguous_paths.insert(missing[*index].path.clone());
        }
        for path in destinations {
            ambiguous_paths.insert(path.clone());
        }
    }

    // 歧义只影响“能不能配对”；这些条目仍然是缺失（删除候选或下次再试），不能从缺失里消失。
    let remaining: Vec<MissingEntry> = missing
        .iter()
        .enumerate()
        .filter(|(index, _)| !paired.contains(index))
        .map(|(_, entry)| entry.clone())
        .collect();
    (moves, remaining, ambiguous_paths)
}

/// 为什么把一批删除放进保护队列（design §12.3：不把环境故障伪装成用户删除）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtectionReason {
    /// 本批数量超过单轮上限。
    TooManyDeletes {
        /// 本批数量。
        count: usize,
        /// 单轮上限。
        limit: usize,
    },
    /// 本批占比超过上限（例如大量文件一次性消失）。
    TooLargeShare {
        /// 本批数量。
        count: usize,
        /// 已保护条目总数。
        protected_total: usize,
        /// 占比上限（百分比）。
        percent_limit: u32,
    },
}

impl ProtectionReason {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ProtectionReason::TooManyDeletes { .. } => "too_many_deletes",
            ProtectionReason::TooLargeShare { .. } => "too_large_share",
        }
    }
}

impl std::fmt::Display for ProtectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtectionReason::TooManyDeletes { count, limit } => {
                write!(f, "本批 {count} 个删除超过单轮上限 {limit}")
            }
            ProtectionReason::TooLargeShare {
                count,
                protected_total,
                percent_limit,
            } => write!(
                f,
                "本批 {count} 个删除占已保护条目 {protected_total} 的 {percent_limit}% 以上"
            ),
        }
    }
}

/// 判断一批不会自动放行的删除是否必须先进保护队列。`count` 为 0 时永远不需要。
#[must_use]
pub fn protection_reason(
    count: usize,
    protected_total: usize,
    limits: ReconcileLimits,
) -> Option<ProtectionReason> {
    if count == 0 {
        return None;
    }
    if count > limits.max_deletes_per_cycle {
        return Some(ProtectionReason::TooManyDeletes {
            count,
            limit: limits.max_deletes_per_cycle,
        });
    }
    // 用整数比较避免浮点误差：count / total > percent / 100。样本太小时比例没有意义。
    let allowed = protected_total.saturating_mul(limits.max_delete_percent as usize) / 100;
    if protected_total >= SHARE_MIN_TOTAL && count > allowed {
        return Some(ProtectionReason::TooLargeShare {
            count,
            protected_total,
            percent_limit: limits.max_delete_percent,
        });
    }
    None
}

/// 保护队列里的一个缺失条目：原基线（复核时要能判断该恢复还是该接受删除）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedEntry {
    /// 上次采纳时的路径。
    pub path: String,
    /// 条目身份。
    pub entry: EntryId,
    /// 上次采纳时的 revision。
    pub revision: Revision,
    /// 上次采纳时的文件身份。
    pub identity: FileKey,
    /// 上次采纳时的创建时间。
    pub btime: Option<(i64, u32)>,
    /// 上次采纳时的内容。
    pub content: ExpectedContent,
}

impl From<&MissingEntry> for ProtectedEntry {
    fn from(entry: &MissingEntry) -> Self {
        ProtectedEntry {
            path: entry.path.clone(),
            entry: entry.entry,
            revision: entry.revision,
            identity: entry.identity,
            btime: entry.btime,
            content: entry.content,
        }
    }
}

/// 保护队列里记录的一个目录身份（缺失条目所在的父目录）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedDirectory {
    /// 目录相对路径（空串为 Vault 自身）。
    pub path: String,
    /// 实测身份；读不到时为 `None`，原因见 `error`。
    pub identity: Option<DirIdentity>,
    /// 读取身份失败的原因（不吞错）。
    pub error: Option<String>,
    /// 这个目录下有多少个缺失条目。
    pub missing_count: usize,
}

/// 一批被保护的缺失（design §12.3：附原基线、目录身份和计数供复核）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedBatch {
    /// 批次号（进程内单调，供 [`crate::observer::import::ExternalObserver::release_protected`] 使用）。
    pub id: u64,
    /// 触发保护的原因。
    pub reason: ProtectionReason,
    /// 触发时已保护条目总数（占比的分母）。
    pub protected_total: usize,
    /// 本批缺失条目（按路径升序）。
    pub entries: Vec<ProtectedEntry>,
    /// 这些条目的父目录及其身份（按路径升序）。
    pub directories: Vec<ProtectedDirectory>,
}

impl ProtectedBatch {
    /// 批次覆盖的条目数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否没有条目（构造后不应出现，保留给诊断）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 一轮核对的结论（写进 [`crate::observer::import::PassReport`]）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileReport {
    /// 本轮扫描的健康结论。
    pub health: ScanHealth,
    /// 是否真的执行了核对（只在“完整且健康”的扫描上执行）。
    pub ran: bool,
    /// 识别出并已提交的移动。
    pub moves: Vec<MoveRecord>,
    /// 因歧义放弃配对、按独立新增/删除处理的路径（诊断用）。
    pub ambiguous: Vec<String>,
    /// 已提交的外部删除。
    pub deletions: Vec<DeleteRecord>,
    /// 保持不动、下一轮重试的删除（存储比较并写失败等）。
    pub held: Vec<HeldEntry>,
    /// 本轮进入保护队列的批次。
    pub protected: Option<ProtectedBatch>,
    /// Vault 正在提交别的内容，本轮跳过核对（下一轮再做）。
    pub deferred: bool,
}

impl ReconcileReport {
    /// 没有执行的报告（扫描不健康）。
    #[must_use]
    pub fn skipped(health: ScanHealth) -> Self {
        ReconcileReport {
            health,
            ran: false,
            moves: Vec::new(),
            ambiguous: Vec::new(),
            deletions: Vec::new(),
            held: Vec::new(),
            protected: None,
            deferred: false,
        }
    }
}

/// 已提交的一次移动。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRecord {
    /// 原路径。
    pub from: String,
    /// 新路径。
    pub to: String,
    /// 保持不变的条目身份。
    pub entry: EntryId,
    /// 新 revision。
    pub revision: Revision,
}

/// 已提交的一次外部删除。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteRecord {
    /// 消失的路径。
    pub path: String,
    /// 被删除的条目身份。
    pub entry: EntryId,
}

/// 保持不动、下一轮重试的变更（移动或删除）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldEntry {
    /// 路径。
    pub path: String,
    /// 稳定原因码。
    pub code: &'static str,
    /// 原因文本。
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(inode: u64) -> FileKey {
        FileKey {
            device: 1,
            inode,
            links: 1,
            bytes: 3,
        }
    }

    fn stamp(inode: u64) -> FileStamp {
        FileStamp {
            key: key(inode),
            btime: Some((1, 0)),
            mtime: (2, 0),
            ctime: (2, 0),
        }
    }

    fn gone(path: &str, inode: u64) -> MissingEntry {
        MissingEntry {
            path: path.to_owned(),
            entry: EntryId::from_bytes([u8::try_from(inode).expect("小"); 16]),
            revision: Revision::new(3).expect("合法"),
            identity: key(inode),
            btime: Some((1, 0)),
            content: ExpectedContent::new(crate::objects::ContentDigest::from_bytes([9u8; 32]), 3),
        }
    }

    #[test]
    fn identity_pairs_a_rename_even_when_two_files_share_the_same_content() {
        // a.txt(inode 7) → c.txt，b.txt(inode 8) → d.txt，两份内容相同（哈希相同）。
        let missing = vec![gone("a.txt", 7), gone("b.txt", 8)];
        let mut seen = BTreeMap::new();
        seen.insert("c.txt".to_owned(), stamp(7));
        seen.insert("d.txt".to_owned(), stamp(8));
        let (moves, remaining, ambiguous) = pair_moves(&missing, &seen);
        assert!(remaining.is_empty());
        assert!(ambiguous.is_empty());
        assert_eq!(moves.len(), 2);
        let pair_of = |from: &str| {
            moves
                .iter()
                .find(|candidate| candidate.missing.path == from)
                .map(|candidate| candidate.to.clone())
                .expect("已配对")
        };
        // 身份配对：a.txt 只能是 inode 7 的 c.txt，不能因为内容相同串到 d.txt。
        assert_eq!(pair_of("a.txt"), "c.txt");
        assert_eq!(pair_of("b.txt"), "d.txt");
    }

    #[test]
    fn same_content_alone_never_pairs_a_new_file() {
        // x.txt 消失，z.txt 出现；内容相同但 inode 不同 = 独立新增（不是移动）。
        let missing = vec![gone("x.txt", 7)];
        let mut seen = BTreeMap::new();
        seen.insert("z.txt".to_owned(), stamp(9));
        let (moves, remaining, ambiguous) = pair_moves(&missing, &seen);
        assert!(moves.is_empty(), "内容相同不能当移动证据");
        assert_eq!(remaining.len(), 1);
        assert!(ambiguous.is_empty());
    }

    #[test]
    fn ambiguous_identities_are_not_paired_in_any_direction() {
        // 两个缺失条目共用一个身份（硬链接）：谁都不能认。
        let missing = vec![gone("a.txt", 7), gone("b.txt", 7)];
        let mut seen = BTreeMap::new();
        seen.insert("c.txt".to_owned(), stamp(7));
        let (moves, remaining, ambiguous) = pair_moves(&missing, &seen);
        assert!(moves.is_empty());
        assert_eq!(remaining.len(), 2);
        assert!(ambiguous.contains("a.txt") && ambiguous.contains("b.txt"));
        assert!(ambiguous.contains("c.txt"));

        // 一个缺失条目对上两个新路径（硬链接）：同样不猜。
        let missing = vec![gone("a.txt", 7)];
        let mut seen = BTreeMap::new();
        seen.insert("c.txt".to_owned(), stamp(7));
        seen.insert("d.txt".to_owned(), stamp(7));
        let (moves, remaining, ambiguous) = pair_moves(&missing, &seen);
        assert!(moves.is_empty());
        assert_eq!(remaining.len(), 1);
        assert!(ambiguous.contains("a.txt"));
        assert!(ambiguous.contains("c.txt") && ambiguous.contains("d.txt"));
    }

    #[test]
    fn overwritten_inode_with_new_btime_is_not_the_same_object() {
        let missing = vec![gone("a.txt", 7)];
        let mut seen = BTreeMap::new();
        let mut other = stamp(7);
        other.btime = Some((5, 0));
        seen.insert("c.txt".to_owned(), other);
        let (moves, remaining, _) = pair_moves(&missing, &seen);
        assert!(moves.is_empty(), "创建时间不同就不是同一个文件对象");
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn protection_kicks_in_on_counts_and_shares() {
        let limits = ReconcileLimits::new(10, 10, 4).expect("合法");
        assert_eq!(protection_reason(0, 100, limits), None);
        assert_eq!(protection_reason(3, 100, limits), None);
        assert_eq!(
            protection_reason(11, 100, limits),
            Some(ProtectionReason::TooManyDeletes {
                count: 11,
                limit: 10
            })
        );
        // 10% of 20 = 2：3 个删除超过占比（20 ≥ 样本下限 16）。
        assert_eq!(
            protection_reason(3, 20, limits),
            Some(ProtectionReason::TooLargeShare {
                count: 3,
                protected_total: 20,
                percent_limit: 10
            })
        );
        // 小集合里 1 个删除不算突变：样本不足下限时只看数量上限。
        assert_eq!(protection_reason(1, 3, limits), None);
        assert_eq!(protection_reason(2, 3, limits), None);
    }

    #[test]
    fn limits_reject_zero_capacity_and_percent_over_100() {
        assert_eq!(
            ReconcileLimits::new(0, 10, 1).unwrap_err(),
            InvalidReconcileLimits::ZeroDeletes
        );
        assert_eq!(
            ReconcileLimits::new(1, 101, 1).unwrap_err(),
            InvalidReconcileLimits::PercentOutOfRange
        );
        assert_eq!(
            ReconcileLimits::new(1, 10, 0).unwrap_err(),
            InvalidReconcileLimits::ZeroCapacity
        );
    }

    #[test]
    fn health_names_match_the_design_vocabulary() {
        assert_eq!(ScanHealth::Complete.as_str(), "complete");
        assert!(ScanHealth::Complete.is_complete_and_healthy());
        assert_eq!(
            ScanHealth::PermissionDenied { paths: vec![] }.as_str(),
            "permission_denied"
        );
        assert_eq!(ScanHealth::IoError { paths: vec![] }.as_str(), "io_error");
        assert_eq!(
            ScanHealth::MountChanged {
                detail: "unmounted".to_owned()
            }
            .as_str(),
            "mount_changed"
        );
        assert!(
            !ScanHealth::MountChanged {
                detail: "unmounted".to_owned()
            }
            .is_complete_and_healthy()
        );
    }
}
