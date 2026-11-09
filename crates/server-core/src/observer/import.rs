//! 外部目录的初始稳定采集：把扫描、稳定窗口、采集与采纳串成一轮轮可续的流程（design §12.1）。
//!
//! 一轮 [`ExternalObserver::run_pass`] 做的事：
//!
//! 1. 从上次的续扫游标（没有就从 Vault 起点）做一次有界扫描（[`scan_tree`]）；
//! 2. 每个枚举到的文件记录指纹；**首次看到总要等待一个完整稳定窗口**，指纹在窗口内变化就重新计时
//!    （持续写入的文件一直是“等待”，不会被反复传播部分片段）；
//! 3. 稳定的文件做采集（[`capture_external`]）与采纳（[`adopt_external`]），每轮最多
//!    [`PassLimits::max_captures`] 个；
//! 4. 结果记入内存账本：**只有采纳成功才算“已有历史保护”**，其余一律是待处理，各带原因。
//!
//! 不变量：
//!
//! - 本流程**从不推导删除**：扫描中读不到、消失、Vault 不可用都不产生任何删除结论；消失的从未采纳
//!   文件只是从待处理中移除。已采纳文件之后被外部删除属于后续事件管线的职责，这里不处理；
//! - 扫描不完整（预算耗尽、读取失败、Vault 不可用）时，**不**宣布初始导入完成，也不据此清理账本；
//! - 初始导入完成需要：一轮扫描完整走完，且这一轮结束时没有任何待处理文件。完成标志一旦为真
//!   就保持（之后的新文件按普通外部变化处理）；
//! - 账本只是进程内的报告视图，真相在库里；重启后账本为空，文件会被重新枚举，内容相同的由库查询
//!   识别为“内容未变”，不会产生重复版本。

use std::collections::{BTreeMap, BTreeSet};

use vaultsyncd_domain::ids::{EntryId, VaultId};
use vaultsyncd_domain::version::{Revision, UtcTimestamp};

use crate::commit::put::VaultCommitGate;
use crate::objects::{ExpectedContent, InvalidStagingName, ObjectLayout};
use crate::observer::capture::{
    AdoptFailure, AdoptOutcome, BlockReason, CaptureOutcome, RequeueReason, adopt_external,
    capture_external,
};
use crate::observer::intents::{IntentError, IntentVerdict, OwnWriteIntent, OwnWriteLedger};
use crate::observer::reconcile::{
    DeleteRecord, HeldEntry, MissingEntry, MoveRecord, ProtectedBatch, ProtectedDirectory,
    ProtectedEntry, ProtectionReason, ReconcileLimits, ReconcileReport, pair_moves,
    protection_reason,
};
use crate::observer::scan::{
    InconclusiveReason, ScanCompletion, ScanCursor, ScanHealth, ScanLimits, ScannedFile,
    SkippedEntry, Stability, StabilityTracker, StabilityWindow, scan_tree,
};
use crate::ports::external_store::{
    ExternalAdoptStore, ExternalDelete, ExternalMove, ExternalRecord,
};
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
    /// Vault 内提交串行化（与 put 共用）。
    pub gate: &'a mut VaultCommitGate,
    /// 采纳存储端口。
    pub store: &'a mut S,
}

/// 被推迟的原因（不是失败，下一轮继续）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferReason {
    /// 同一 Vault 正在提交别的内容。
    VaultBusy,
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
    /// 现场成了别人（另一个已采纳条目）的文件：不按路径重新绑 EntryId，等待复核。
    Held {
        /// 稳定原因码。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
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
    /// 本服务自己的写入（inode 与内容都与登记的意图一致）：不当作外部变化采纳。
    OwnWrite,
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
    /// Vault 不可用而中止（本轮没有采集任何内容，也没有推导任何删除）。
    pub vault_unavailable: bool,
    /// 初始导入是否已完成（只增不减）。
    pub initial_import_complete: bool,
    /// 移动/删除/保护核对的结论（含本轮扫描健康结论）。
    pub reconcile: ReconcileReport,
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

/// 已采纳路径的基线：采纳时的条目身份、指纹与内容。指纹未变不再采集；内容复核拿它比对重新读到的内容；
/// 外部移动/删除核对拿条目身份与 revision 做比较并写。
#[derive(Clone, Copy, Debug)]
struct Adopted {
    entry: EntryId,
    revision: Revision,
    stamp: FileStamp,
    content: ExpectedContent,
}

/// 某个 Vault 的外部内容观察器（初始稳定采集，以及事件线索驱动下的持续观察）。
pub struct ExternalObserver {
    vault: VaultId,
    tracker: StabilityTracker,
    /// 已采纳（或库里已有相同内容）的路径 → 采纳时的基线；指纹未变则不再采集。
    adopted: BTreeMap<String, Adopted>,
    // 同一 EntryId 只能保留当前路径；目录自身改名不会推进后代 revision。
    // 反向索引避免每次内容采纳线性扫描整棵 Vault。
    adopted_paths: BTreeMap<EntryId, String>,
    pending: BTreeMap<String, PendingReason>,
    resume: Option<ScanCursor>,
    cycle_seen: BTreeSet<String>,
    /// 一次完整扫描（可能跨多轮续扫）里枚举到的文件；只有走完的完整健康扫描才拿它核对删除。
    scan_files: BTreeMap<String, FileStamp>,
    staging_prefix: String,
    staging_counter: u64,
    initial_import_complete: bool,
    intents: OwnWriteLedger,
    /// 内容复核的游标：下一批从这个路径之后开始。
    review_after: Option<String>,
    /// 移动/删除/保护的限额。
    limits: ReconcileLimits,
    /// 保护队列（异常批量缺失，等待人工复核）。
    protected: Vec<ProtectedBatch>,
    /// 已复核放行的缺失条目（路径 → 条目身份）：下一轮允许直接传播删除。
    approved: BTreeMap<String, EntryId>,
    /// 现场变成另一个条目文件的路径（例如两份同内容文件互换名字）：保留基线、不重绑身份。
    displaced: BTreeSet<String>,
    next_batch: u64,
}

impl ExternalObserver {
    /// 建立观察器。`staging_prefix` 必须在对象区 staging 目录里唯一（例如含进程号与 Vault 前缀）。
    ///
    /// # Errors
    ///
    /// 暂存名前缀不是合规的单个文件名。
    pub fn new(
        vault: VaultId,
        window: StabilityWindow,
        staging_prefix: &str,
    ) -> Result<Self, ObserverConfigError> {
        ObjectLayout::validate_staging_name(staging_prefix)
            .map_err(ObserverConfigError::StagingPrefix)?;
        Ok(ExternalObserver {
            vault,
            tracker: StabilityTracker::new(window),
            adopted: BTreeMap::new(),
            adopted_paths: BTreeMap::new(),
            pending: BTreeMap::new(),
            resume: None,
            cycle_seen: BTreeSet::new(),
            scan_files: BTreeMap::new(),
            staging_prefix: staging_prefix.to_owned(),
            staging_counter: 0,
            initial_import_complete: false,
            intents: OwnWriteLedger::new(OwnWriteLedger::DEFAULT_LIMIT),
            review_after: None,
            limits: ReconcileLimits::DEFAULT,
            protected: Vec::new(),
            approved: BTreeMap::new(),
            displaced: BTreeSet::new(),
            next_batch: 1,
        })
    }

    /// 设定移动/删除/保护的限额（默认 [`ReconcileLimits::DEFAULT`]）。
    #[must_use]
    pub fn with_reconcile_limits(mut self, limits: ReconcileLimits) -> Self {
        self.limits = limits;
        self
    }

    /// 保护队列（异常批量缺失，按发现顺序）。
    #[must_use]
    pub fn protected_batches(&self) -> &[ProtectedBatch] {
        &self.protected
    }

    /// 人工复核后放行一批：从队列移除，并把其中（路径, 条目身份）记成允许删除。
    ///
    /// 放行只对“同一个条目身份”的下一次缺失生效（一次性）：路径被换成别的条目时重新受保护。
    pub fn release_protected(&mut self, id: u64) -> Option<ProtectedBatch> {
        let index = self.protected.iter().position(|batch| batch.id == id)?;
        let batch = self.protected.remove(index);
        for entry in &batch.entries {
            self.approved.insert(entry.path.clone(), entry.entry);
        }
        Some(batch)
    }

    /// 登记本服务即将发布到 `path` 的文件（服务写入之前调用）。观察器只在 inode 与内容都与意图一致时
    /// 才忽略它；任何时间窗口都不参与判定。提交完成或放弃后必须 [`release_own_write`](Self::release_own_write)。
    ///
    /// # Errors
    ///
    /// 账本已满（服务应暂缓该写入）或该路径已有未释放的意图。
    pub fn expect_own_write(
        &mut self,
        path: &str,
        intent: OwnWriteIntent,
    ) -> Result<(), IntentError> {
        self.intents.expect(path, intent)
    }

    /// 释放自写意图。之后路径上的文件按普通外部内容处理：库里已有相同内容时不产生版本。
    pub fn release_own_write(&mut self, path: &str) -> bool {
        self.intents.release(path)
    }

    /// 未释放的自写意图数。
    #[must_use]
    pub fn own_write_intents(&self) -> usize {
        self.intents.len()
    }

    /// 已采纳（或库里已有相同内容）的路径数。
    #[must_use]
    pub fn adopted_count(&self) -> usize {
        self.adopted.len()
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
        let fresh = self.resume.is_none();
        let cursor = self.resume.take().unwrap_or_else(ScanCursor::from_root);
        if fresh {
            // 新一轮完整扫描：上一轮的（可能是被 Vault 不可用打断的）部分快照不再使用。
            self.scan_files.clear();
        }
        let scan = scan_tree(&mut *ports.work, limits.scan, cursor);
        let health = scan.health();
        let vault_unavailable = scan.vault_unavailable();
        let (scan_complete, inconclusive) = match scan.completion {
            ScanCompletion::Complete => (true, Vec::new()),
            ScanCompletion::Inconclusive { resume, reasons } => {
                self.resume = Some(resume);
                (false, reasons)
            }
        };

        let mut files = scan.files;
        let mut aborted = vault_unavailable;
        if !vault_unavailable {
            // 续扫只覆盖游标里的目录；之前已见过、仍待处理的文件要在这一轮单独复查，否则一个一直读不了
            // 的目录或一次被截断的扫描会让其他文件的稳定窗口永远不推进。
            aborted |= self.recheck_pending(&mut *ports.work, limits, &mut files);
        }
        // 移动/删除只能在“完整且健康”的扫描上核对：扫描不完整或不健康时“没看到”不是删除证据
        // （design §12.3）。必须在 handle_files 之前做，否则移动目标会被当成新文件建立新条目。
        if !vault_unavailable {
            for file in &files {
                self.scan_files.insert(file.path.clone(), file.stamp);
            }
        }
        let reconcile = if scan_complete && !aborted {
            // 续扫可能跨多轮：核对要用整轮累计的文件，只有本轮枚举到的部分会把别处已采纳的文件
            // 误判成删除。
            let seen = std::mem::take(&mut self.scan_files);
            self.reconcile(ports, &seen, clock, health).await
        } else {
            ReconcileReport::skipped(health)
        };
        if !aborted {
            for file in &files {
                self.cycle_seen.insert(file.path.clone());
            }
        }
        let (outcomes, aborted) = self
            .handle_files(ports, files, limits, clock, aborted)
            .await;

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
            vault_unavailable: aborted,
            initial_import_complete: self.initial_import_complete,
            reconcile,
        }
    }

    /// 核对上一轮已采纳、本轮完整健康扫描里消失的条目：移动、删除或保护（design §9.5、§12.3）。
    ///
    /// 顺序：移动（只认文件身份、一对一不歧义）→ 删除（已复核放行的先做）→ 保护（数量或占比异常时
    /// 整批暂停）。任何一步失败都保持基线不动，下一轮重试；绝不把“没读到”当成删除。
    async fn reconcile<S: ExternalAdoptStore>(
        &mut self,
        ports: &mut ObserverPorts<'_, S>,
        seen: &BTreeMap<String, FileStamp>,
        clock: PassClock,
        health: ScanHealth,
    ) -> ReconcileReport {
        let mut report = ReconcileReport::skipped(health);
        if !report.health.is_complete_and_healthy() {
            return report;
        }
        self.displaced.clear();
        report.ran = true;
        let entries: Vec<MissingEntry> = self
            .adopted
            .iter()
            .map(|(path, baseline)| MissingEntry {
                path: path.clone(),
                entry: baseline.entry,
                revision: baseline.revision,
                identity: baseline.stamp.key,
                btime: baseline.stamp.btime,
                content: baseline.content,
            })
            .collect();
        let missing = missing_of(&entries, seen);
        // 没有缺失并不意味着没变化：互换名字时两条路径都还在，只有文件身份换了位置。
        let displaced = displaced_of(&entries, seen);
        if missing.is_empty() && displaced.is_empty() {
            return report;
        }

        let Ok(ticket) = ports.gate.acquire(self.vault) else {
            report.deferred = true;
            return report;
        };
        report = self
            .reconcile_locked(ports, seen, clock, report, missing, displaced)
            .await;
        ports.gate.release(ticket);
        report
    }

    #[allow(clippy::too_many_arguments)]
    async fn reconcile_locked<S: ExternalAdoptStore>(
        &mut self,
        ports: &mut ObserverPorts<'_, S>,
        seen: &BTreeMap<String, FileStamp>,
        clock: PassClock,
        mut report: ReconcileReport,
        missing: Vec<MissingEntry>,
        displaced: Vec<(String, MissingEntry)>,
    ) -> ReconcileReport {
        // 移动目标必须是本轮新出现（尚未采纳）的路径；已采纳路径上的文件不可能是移动目标。
        let seen_new: BTreeMap<String, FileStamp> = seen
            .iter()
            .filter(|(path, _)| !self.adopted.contains_key(path.as_str()))
            .map(|(path, stamp)| (path.clone(), *stamp))
            .collect();
        let displaced_entries: BTreeSet<EntryId> =
            displaced.iter().map(|(_, owner)| owner.entry).collect();
        let considered: Vec<MissingEntry> = missing
            .into_iter()
            .filter(|entry| !displaced_entries.contains(&entry.entry))
            .collect();
        self.displaced = displaced.iter().map(|(path, _)| path.clone()).collect();
        for (path, owner) in &displaced {
            report.ambiguous.push(path.clone());
            report.ambiguous.push(owner.path.clone());
            report.held.push(HeldEntry {
                path: path.clone(),
                code: "identity_swap_needs_review",
                detail: format!(
                    "该路径现在放的是 {} 的文件；不按路径重新绑定 EntryId",
                    owner.path
                ),
            });
        }
        let (moves, rest, ambiguous) = pair_moves(&considered, &seen_new);
        report.ambiguous.extend(ambiguous);

        for candidate in moves {
            let from = candidate.missing.path.clone();
            let to = candidate.to.clone();
            // 目标路径在库里已有条目就不能当成一次改名（不能覆盖别人的条目）。
            match ports.store.lookup_path(self.vault, &to).await {
                Ok(None) => {}
                Ok(Some(_)) => {
                    report.ambiguous.push(to.clone());
                    report.ambiguous.push(from.clone());
                    continue;
                }
                Err(error) => {
                    report.held.push(HeldEntry {
                        path: from,
                        code: error.code(),
                        detail: error.to_string(),
                    });
                    continue;
                }
            }
            let request = ExternalMove {
                vault: self.vault,
                from: from.clone(),
                to: to.clone(),
                entry: candidate.missing.entry,
                expected_revision: candidate.missing.revision,
                at: clock.at,
            };
            match ports.store.apply_external_move(&request).await {
                Ok(record)
                    if record.entry == request.entry
                        && record.revision > request.expected_revision =>
                {
                    self.adopted.remove(&from);
                    self.settle(&from);
                    self.record_baseline(
                        &to,
                        Adopted {
                            entry: record.entry,
                            revision: record.revision,
                            stamp: candidate.stamp,
                            content: candidate.missing.content,
                        },
                    );
                    self.approved.remove(&from);
                    report.moves.push(MoveRecord {
                        from,
                        to,
                        entry: record.entry,
                        revision: record.revision,
                    });
                }
                Ok(record) => report.held.push(HeldEntry {
                    path: from,
                    code: "store_contract_violation",
                    detail: format!(
                        "移动证据条目 {} revision {} 与请求（{} / {}）不符",
                        record.entry, record.revision, request.entry, request.expected_revision
                    ),
                }),
                Err(error) => report.held.push(HeldEntry {
                    path: from,
                    code: error.code(),
                    detail: error.to_string(),
                }),
            }
        }

        // 已复核放行的先做；其余按数量与占比判断是否整批进保护队列。
        let (approved, mut remaining): (Vec<MissingEntry>, Vec<MissingEntry>) = rest
            .into_iter()
            .partition(|entry| self.approved.get(&entry.path) == Some(&entry.entry));
        for entry in approved {
            self.delete_missing(ports, entry, clock.at, &mut report)
                .await;
        }
        if !remaining.is_empty() {
            let protected_total = self.adopted.len();
            match protection_reason(remaining.len(), protected_total, self.limits) {
                Some(reason) => {
                    self.protect(
                        ports,
                        remaining.split_off(0),
                        reason,
                        protected_total,
                        &mut report,
                    );
                }
                None => {
                    for entry in remaining {
                        self.delete_missing(ports, entry, clock.at, &mut report)
                            .await;
                    }
                }
            }
        }
        report
    }

    /// 提交一次外部删除；失败只记入 `held`，基线保持不动。
    async fn delete_missing<S: ExternalAdoptStore>(
        &mut self,
        ports: &mut ObserverPorts<'_, S>,
        entry: MissingEntry,
        at: UtcTimestamp,
        report: &mut ReconcileReport,
    ) {
        let request = ExternalDelete {
            vault: self.vault,
            path: entry.path.clone(),
            entry: entry.entry,
            expected_revision: entry.revision,
            at,
        };
        match ports.store.apply_external_delete(&request).await {
            Ok(record)
                if record.entry == request.entry && record.revision > request.expected_revision =>
            {
                self.adopted.remove(&entry.path);
                self.adopted_paths.remove(&entry.entry);
                self.settle(&entry.path);
                self.approved.remove(&entry.path);
                report.deletions.push(DeleteRecord {
                    path: entry.path,
                    entry: record.entry,
                });
            }
            Ok(record) => report.held.push(HeldEntry {
                path: entry.path,
                code: "store_contract_violation",
                detail: format!(
                    "删除证据条目 {} revision {} 与请求（{} / {}）不符",
                    record.entry, record.revision, request.entry, request.expected_revision
                ),
            }),
            Err(error) => report.held.push(HeldEntry {
                path: entry.path,
                code: error.code(),
                detail: error.to_string(),
            }),
        }
    }

    /// 把一批异常缺失放进保护队列（附原基线、目录身份与计数）。队列已满时不默默丢弃：
    /// 每个条目都记入 `held`，并保留已有批次。
    fn protect<S: ExternalAdoptStore>(
        &mut self,
        ports: &mut ObserverPorts<'_, S>,
        entries: Vec<MissingEntry>,
        reason: ProtectionReason,
        protected_total: usize,
        report: &mut ReconcileReport,
    ) {
        let directories = group_directories(&entries, &mut *ports.work);
        if self.protected.len() >= self.limits.protection_capacity {
            for entry in entries {
                report.held.push(HeldEntry {
                    path: entry.path,
                    code: "protection_queue_full",
                    detail: format!(
                        "保护队列已有 {} 批（上限 {}），本批未入队：{reason}",
                        self.protected.len(),
                        self.limits.protection_capacity
                    ),
                });
            }
            return;
        }
        let batch = ProtectedBatch {
            id: self.next_batch,
            reason,
            protected_total,
            entries: entries.iter().map(ProtectedEntry::from).collect(),
            directories,
        };
        self.next_batch += 1;
        report.protected = Some(batch.clone());
        self.protected.push(batch);
    }

    /// 对一批已枚举的文件做稳定性判断、采集与采纳。`aborted` 为真（Vault 不可用）时什么都不做；
    /// 返回逐文件结局与“Vault 是否不可用”。
    async fn handle_files<S: ExternalAdoptStore>(
        &mut self,
        ports: &mut ObserverPorts<'_, S>,
        files: Vec<ScannedFile>,
        limits: PassLimits,
        clock: PassClock,
        mut aborted: bool,
    ) -> (Vec<FileOutcome>, bool) {
        let mut outcomes = Vec::new();
        let mut candidates: Vec<(String, FileStamp)> = Vec::new();
        if !aborted {
            for file in files {
                if self
                    .adopted
                    .get(&file.path)
                    .is_some_and(|baseline| baseline.stamp == file.stamp)
                {
                    self.pending.remove(&file.path);
                    continue;
                }
                // 已核对为本服务写入、指纹未变：不必再等待与采集。任何指纹变化都会重新核对。
                if self.intents.is_verified(&file.path, &file.stamp) {
                    self.settle(&file.path);
                    continue;
                }
                // 这条路径上的文件现在是另一个已采纳条目的文件：按路径采纳会把两个 EntryId 串起来。
                // 保留双方基线，等人工复核（design §9.5：不凭内容猜身份）。
                if self.displaced.contains(&file.path) {
                    let reason = PendingReason::Held {
                        code: "identity_swap_needs_review",
                        detail: "现场是另一个已采纳条目的文件".to_owned(),
                    };
                    self.mark_pending(&file.path, reason.clone());
                    outcomes.push(FileOutcome {
                        path: file.path,
                        state: FileState::Pending(reason),
                    });
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
            let (state, vault_gone) = self.process_candidate(ports, &path, &stamp, clock).await;
            aborted |= vault_gone;
            outcomes.push(FileOutcome { path, state });
        }
        (outcomes, aborted)
    }

    /// 复查本轮扫描没有覆盖到的待处理文件，把仍是普通文件的并入 `files`；返回 Vault 是否不可用。
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
                Err(WorkTreeError::VaultUnavailable { .. }) => return true,
                Err(_) => {}
            }
        }
        false
    }

    /// 处理一个稳定文件；返回结局与“Vault 是否不可用”。
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
            self.vault,
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
                let vault_gone = error.is_vault_unavailable();
                let reason = PendingReason::Failed {
                    code: error.code(),
                    detail: error.to_string(),
                };
                self.mark_pending(path, reason.clone());
                return (FileState::Pending(reason), vault_gone);
            }
        };

        match self.intents.judge(path, &proposal.stamp, &proposal.content) {
            IntentVerdict::Ours => {
                self.settle(path);
                return (FileState::OwnWrite, false);
            }
            // 同一个 inode 被外部改了内容：意图已作废，按外部修改继续采纳。
            IntentVerdict::Modified | IntentVerdict::NotOurs => {}
        }

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
                self.record_baseline(
                    path,
                    Adopted {
                        entry: record.entry,
                        revision: record.revision,
                        stamp: proposal.stamp,
                        content: proposal.content,
                    },
                );
                self.settle(path);
                let state = FileState::Adopted {
                    record,
                    created,
                    copy_method: proposal.object.method,
                    copy_fallback: proposal.object.fallback,
                };
                (state, false)
            }
            Ok(AdoptOutcome::UnchangedContent {
                content,
                entry,
                revision,
            }) => {
                self.record_baseline(
                    path,
                    Adopted {
                        entry,
                        revision,
                        stamp: proposal.stamp,
                        content,
                    },
                );
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
            Ok(AdoptOutcome::VaultBusy) => {
                let reason = PendingReason::Deferred(DeferReason::VaultBusy);
                self.mark_pending(path, reason.clone());
                (FileState::Pending(reason), false)
            }
            Ok(AdoptOutcome::Blocked(reason)) => {
                let reason = PendingReason::Blocked(reason);
                self.mark_pending(path, reason.clone());
                (FileState::Pending(reason), false)
            }
            Err(error) => {
                let vault_gone = matches!(
                    error,
                    AdoptFailure::Work(
                        crate::ports::work_tree::WorkTreeError::VaultUnavailable { .. }
                    )
                );
                let reason = PendingReason::Failed {
                    code: error.code(),
                    detail: error.to_string(),
                };
                self.mark_pending(path, reason.clone());
                (FileState::Pending(reason), vault_gone)
            }
        }
    }

    fn record_baseline(&mut self, path: &str, baseline: Adopted) {
        // 已提交的 API 移动可改变文件/祖先路径而不改变子文件 revision。
        // 新路径仍经过稳定采集及内容核验，确认同一 EntryId 后才替换旧观察路径。
        // 不直接采用扫描的新 stamp，否则会跳过移动期间发生的外部内容改写。
        if let Some(previous) = self.adopted_paths.insert(baseline.entry, path.to_owned())
            && previous != path
        {
            self.adopted.remove(&previous);
            self.settle(&previous);
            self.approved.remove(&previous);
        }
        if let Some(previous) = self.adopted.insert(path.to_owned(), baseline)
            && previous.entry != baseline.entry
        {
            self.adopted_paths.remove(&previous.entry);
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

mod hinted;
#[cfg(test)]
mod reconcile_tests;
#[cfg(test)]
mod tests;

/// 本轮完整扫描里没再出现的已采纳条目。
fn missing_of(entries: &[MissingEntry], seen: &BTreeMap<String, FileStamp>) -> Vec<MissingEntry> {
    entries
        .iter()
        .filter(|entry| !seen.contains_key(entry.path.as_str()))
        .cloned()
        .collect()
}

/// 现场已经换成“另一个已采纳条目的文件”的路径（互换/被覆盖），以及它现在放的是谁的文件。
fn displaced_of(
    entries: &[MissingEntry],
    seen: &BTreeMap<String, FileStamp>,
) -> Vec<(String, MissingEntry)> {
    let mut displaced = Vec::new();
    for (path, stamp) in seen {
        let Some(baseline) = entries.iter().find(|entry| entry.path == *path) else {
            continue;
        };
        if baseline.same_object(stamp) {
            continue;
        }
        if let Some(owner) = entries
            .iter()
            .find(|entry| entry.path != *path && entry.same_object(stamp))
        {
            displaced.push((path.clone(), owner.clone()));
        }
    }
    displaced
}

/// 把一批缺失条目按父目录分组，并逐个实测目录身份（design §12.3 的“目录身份”）”。
fn group_directories(
    entries: &[MissingEntry],
    work: &mut dyn WorkTreePort,
) -> Vec<ProtectedDirectory> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in entries {
        let parent = entry
            .path
            .rsplit_once('/')
            .map_or_else(String::new, |(dir, _)| dir.to_owned());
        *counts.entry(parent).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(path, missing_count)| match work.observe_dir(&path) {
            Ok(identity) => ProtectedDirectory {
                path,
                identity,
                error: None,
                missing_count,
            },
            Err(error) => ProtectedDirectory {
                path,
                identity: None,
                error: Some(error.to_string()),
                missing_count,
            },
        })
        .collect()
}

pub use hinted::{HintedReport, ReviewReport};
