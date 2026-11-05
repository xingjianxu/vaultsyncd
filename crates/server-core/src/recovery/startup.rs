//! 启动恢复：先按未完成意图恢复，再开放写入口（design §11.3 末段、I13、T23）。
//!
//! 顺序即不变量：进程启动时 [`WriteAdmission`] 里每个 Root 都是 [`RootAdmission::AwaitingRecovery`]，
//! 只有 [`recover_root`] 给出“没有需要暂停的证据”的报告后，[`WriteAdmission::admit_write`] 才放行。
//! 恢复只重放能证明的操作（§11.3“只重放可证明的操作，不依赖最后一条普通日志推测事实”）：
//!
//! | 恢复前证据 | 动作 | 结果 |
//! | --- | --- | --- |
//! | `prepared`，现场 == `expected_before`（前 hash） | 按同一意图重新发布、记 `published`、补元数据事务 | [`RecoveryOutcome::Redone`] |
//! | 现场 == `desired_after`（后 hash），阶段 `prepared`/`published` | 只补元数据事务 | [`RecoveryOutcome::Completed`] |
//! | 现场是第三种内容（已又被外部改写） | 不动磁盘、不提交，保留前/后两个可读版本 | [`RecoveryOutcome::Preserved`] |
//! | 现场读不到、意图已标 `needs_recovery`、或证据不足以收尾 | 意图落 `needs_recovery`，Root 停写 | [`RecoveryOutcome::NeedsRecovery`] |
//! | 恢复动作本身失败（满盘、库忙、端口违约） | 什么都不假装完成，Root 保持关闭，下次启动重试 | [`RecoveryOutcome::Unresolved`] |
//!
//! 四条不变量：
//!
//! - **无假成功**：只有元数据事务真的提交（且返回证据与意图一致）才产生 `Completed`/`Redone`；
//!   没有任何路径在没有版本行的情况下报告新结果（I01）；
//! - **故障不是删除**：目标读不到只走向 `NeedsRecovery`，不会被当成“目标不存在”而重做发布；
//! - **不盲目覆盖**：现场是第三种内容时保留双方，既不重做也不回退；
//! - **暂停优先于便利**：任何没证明完成的意图都让该 Root 保持关闭（[`RecoveryReport::pauses_writes`]）。

use std::collections::BTreeMap;
use std::fmt;

use vaultsyncd_domain::ids::{IntentId, RootId, VersionId};
use vaultsyncd_domain::version::{Revision, UtcTimestamp};

use crate::commit::intent::{PutIntent, PutPhase, TargetBefore};
use crate::objects::ExpectedContent;
use crate::ports::intent_log::{IntentLog, IntentLogError};
use crate::ports::put_store::{PutCommitStore, PutWrite};
use crate::ports::target::{PublishRequest, PublishedTarget, TargetPort};
use crate::recovery::{NeedsRecoveryReason, ReconcileReason, RecoveryDecision, classify_put};

/// 一条意图的恢复结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// 未发布：按同一意图重新发布并补完元数据事务（§11.3 第 3 行），得到可证明的新结果。
    Redone {
        /// 元数据事务提交后产生的新版本身份。
        version: VersionId,
        /// 重新发布后目标的实测现场（等于 `desired_after`）。
        published: PublishedTarget,
    },
    /// 已发布：只补完元数据事务（§11.3 第 4 行），得到可证明的新结果。
    Completed {
        /// 元数据事务提交后产生的新版本身份。
        version: VersionId,
    },
    /// 现场已被外部改成第三种内容：保留前/后两个可读版本，等协调，不覆盖、不提交。
    Preserved {
        /// 具体情形。
        reason: ReconcileReason,
    },
    /// 没有可证明的现场或证据不足：意图已落 `needs_recovery`，该 Root 暂停破坏性写入。
    NeedsRecovery {
        /// 具体原因。
        reason: NeedsRecoveryReason,
    },
    /// 恢复动作本身失败：没有完成也没有假装完成，Root 保持关闭，下次启动重试同一意图。
    Unresolved {
        /// 稳定错误码（端口错误码或 `store_contract_violation` 等核心判定）。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
}

impl RecoveryOutcome {
    /// 稳定短名（恢复报告、日志与验收脚本共用）。
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            RecoveryOutcome::Redone { .. } => "redone",
            RecoveryOutcome::Completed { .. } => "completed",
            RecoveryOutcome::Preserved { reason } => reason.as_str(),
            RecoveryOutcome::NeedsRecovery { reason } => reason.as_str(),
            RecoveryOutcome::Unresolved { code, .. } => code,
        }
    }

    /// 是否产生了新的已提交版本（[`RecoveryOutcome::Redone`]/[`RecoveryOutcome::Completed`]）。
    #[must_use]
    pub fn is_committed(&self) -> bool {
        matches!(
            self,
            RecoveryOutcome::Redone { .. } | RecoveryOutcome::Completed { .. }
        )
    }

    /// 新版本身份（没有提交时为 `None`）。
    #[must_use]
    pub fn version(&self) -> Option<VersionId> {
        match self {
            RecoveryOutcome::Redone { version, .. } | RecoveryOutcome::Completed { version } => {
                Some(*version)
            }
            _ => None,
        }
    }

    /// 是否让该 Root 保持关闭（`NeedsRecovery` 或 `Unresolved`）。
    #[must_use]
    pub fn pauses_writes(&self) -> bool {
        matches!(
            self,
            RecoveryOutcome::NeedsRecovery { .. } | RecoveryOutcome::Unresolved { .. }
        )
    }
}

impl fmt::Display for RecoveryOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecoveryOutcome::Redone { version, .. } => {
                write!(f, "已重做发布并提交新版本 {version}")
            }
            RecoveryOutcome::Completed { version } => write!(f, "已补完事务，新版本 {version}"),
            RecoveryOutcome::Preserved { reason } => {
                write!(f, "保留双方可读版本，等协调（{reason}）")
            }
            RecoveryOutcome::NeedsRecovery { reason } => {
                write!(f, "暂停该 Root 的破坏性写入（{reason}）")
            }
            RecoveryOutcome::Unresolved { code, detail } => {
                write!(f, "恢复未完成（{code}）：{detail}")
            }
        }
    }
}

/// 一条被处理过的未完成意图：意图证据、判定与结果（恢复报告的一行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredIntent {
    /// 恢复前的意图记录（`phase` 是崩溃后读到的阶段，`before`/`after` 是候选版本证据）。
    pub intent: PutIntent,
    /// 按恢复前证据得到的判定。
    pub decision: RecoveryDecision,
    /// 实际执行的恢复结果。
    pub outcome: RecoveryOutcome,
}

impl RecoveredIntent {
    /// 意图身份。
    #[must_use]
    pub fn intent_id(&self) -> IntentId {
        self.intent.intent_id
    }
}

/// 一个 Root 的恢复报告：按意图写入顺序的结果 + 是否必须继续停写。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    /// 被恢复的 Root。
    pub root: RootId,
    /// 每条未完成意图的判定与结果（保持意图日志的写入顺序）。
    pub intents: Vec<RecoveredIntent>,
}

impl RecoveryReport {
    /// 是否必须保持该 Root 的写入关闭（任一条 `NeedsRecovery`/`Unresolved` 即真）。
    #[must_use]
    pub fn pauses_writes(&self) -> bool {
        self.intents
            .iter()
            .any(|recovered| recovered.outcome.pauses_writes())
    }

    /// 是否有需要协调的条目（有 `Preserved` 即真）：旧对象与发布候选都还在，等采集/协调路径处理。
    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        self.intents
            .iter()
            .any(|recovered| matches!(recovered.outcome, RecoveryOutcome::Preserved { .. }))
    }

    /// 本次恢复产生的已提交版本。
    #[must_use]
    pub fn committed_versions(&self) -> Vec<VersionId> {
        self.intents
            .iter()
            .filter_map(|recovered| recovered.outcome.version())
            .collect()
    }

    /// 按意图身份查结果。
    #[must_use]
    pub fn outcome_for(&self, id: IntentId) -> Option<&RecoveryOutcome> {
        self.intents
            .iter()
            .find(|recovered| recovered.intent.intent_id == id)
            .map(|recovered| &recovered.outcome)
    }
}

/// 启动恢复在 Root 级别的失败：拿不到未完成清单就不能假设“没有未完成意图”。
#[derive(Clone, Debug)]
pub enum RecoveryError {
    /// 意图日志读不出来（存储失败、越权）：该 Root 必须保持关闭。
    Intent(IntentLogError),
}

impl RecoveryError {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            RecoveryError::Intent(error) => error.code(),
        }
    }
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecoveryError::Intent(error) => write!(f, "读不出未完成意图：{error}"),
        }
    }
}

impl std::error::Error for RecoveryError {}

/// 恢复一个 Root：按写入顺序处理全部未完成意图，返回可核对的恢复报告。
///
/// 调用方必须在写入开放之前调用本函数并把报告交给 [`WriteAdmission::record`]；报告
/// [`RecoveryReport::pauses_writes`] 为真时该 Root 只能只读。
///
/// 只处理 put 意图：create/move/delete 的意图日志由后续卡接入同一条启动顺序。
///
/// # Errors
///
/// 见 [`RecoveryError`]：读不出未完成意图（越权、存储失败）。单条意图的失败不在此返回，而是作为
/// [`RecoveryOutcome::Unresolved`] 留在报告里，让该 Root 保持关闭。
pub async fn recover_root<L, T, S>(
    log: &mut L,
    target: &mut T,
    store: &mut S,
    at: UtcTimestamp,
    root: RootId,
) -> Result<RecoveryReport, RecoveryError>
where
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
    S: PutCommitStore + ?Sized,
{
    let unfinished = log
        .unfinished_puts(root)
        .await
        .map_err(RecoveryError::Intent)?;
    let mut intents = Vec::with_capacity(unfinished.len());
    for intent in unfinished {
        // 已收尾的行不会出现在未完成清单里；万一出现，恢复对它无事可做，也不再改动它。
        if !intent.phase.is_open() {
            continue;
        }
        let decision = classify_put(&intent, target.observe(&intent.target_path));
        let outcome = apply_decision(log, target, store, at, &intent, decision).await;
        intents.push(RecoveredIntent {
            intent,
            decision,
            outcome,
        });
    }
    Ok(RecoveryReport { root, intents })
}

async fn apply_decision<L, T, S>(
    log: &mut L,
    target: &mut T,
    store: &mut S,
    at: UtcTimestamp,
    intent: &PutIntent,
    decision: RecoveryDecision,
) -> RecoveryOutcome
where
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
    S: PutCommitStore + ?Sized,
{
    match decision {
        // 未发布：先按同一意图重新发布，再补完元数据事务。
        RecoveryDecision::Redo => match republish(target, intent) {
            Ok(published) => match log
                .advance_put(intent.intent_id, PutPhase::Published, at)
                .await
            {
                Ok(_) => match finish_metadata(store, at, intent).await {
                    RecoveryOutcome::Completed { version } => {
                        RecoveryOutcome::Redone { version, published }
                    }
                    other => other,
                },
                Err(error) => unresolved(error.code(), error.to_string()),
            },
            Err((code, detail)) => unresolved(code, detail),
        },
        // 已发布：工作文件已经是目标内容，只需补完同一次操作的元数据事务。
        RecoveryDecision::Complete => finish_metadata(store, at, intent).await,
        // 现场被外部改成第三种内容：保留双方可读版本，不覆盖、不提交。
        RecoveryDecision::Reconcile { reason } => RecoveryOutcome::Preserved { reason },
        // 读不到现场 / 已被标 needs_recovery / 证据矛盾：落盘暂停，交给人核对。
        RecoveryDecision::NeedsRecovery { reason } => {
            if intent.phase == PutPhase::NeedsRecovery {
                RecoveryOutcome::NeedsRecovery { reason }
            } else {
                match log
                    .advance_put(intent.intent_id, PutPhase::NeedsRecovery, at)
                    .await
                {
                    Ok(_) => RecoveryOutcome::NeedsRecovery { reason },
                    Err(error) => unresolved(error.code(), error.to_string()),
                }
            }
        }
        // 未完成清单里不该出现已收尾的意图：按“没证明”处理，绝不假装完成。
        RecoveryDecision::AlreadyFinished => unresolved(
            "intent_already_finished",
            "未完成清单里出现已收尾意图".to_owned(),
        ),
    }
}

/// 按意图记录重新发布工作文件，并核对实测内容等于 `desired_after`（短写/被外部改写都不算发布成功）。
fn republish<T>(
    target: &mut T,
    intent: &PutIntent,
) -> Result<PublishedTarget, (&'static str, String)>
where
    T: TargetPort + ?Sized,
{
    let request = PublishRequest {
        target_path: intent.target_path.clone(),
        staging_path: intent.staging_path.clone(),
        digest: intent.after.digest,
        size: intent.after.size,
    };
    match target.publish(&request) {
        Ok(published) if intent.after.satisfied_by_published(&published) => Ok(published),
        Ok(published) => Err((
            "published_target_mismatch",
            format!(
                "重新发布后实测 {}（{} 字节），意图目标是 {}（{} 字节）",
                published.digest, published.identity.bytes, intent.after.digest, intent.after.size
            ),
        )),
        Err(error) => Err((error.code(), error.to_string())),
    }
}

/// 补完崩溃前没提交的元数据事务（versions/entries/changes/结果 + 意图收尾，同一事务）。
async fn finish_metadata<S>(store: &mut S, at: UtcTimestamp, intent: &PutIntent) -> RecoveryOutcome
where
    S: PutCommitStore + ?Sized,
{
    // 恢复只能重放意图里已有的证据：条目身份与基线 revision 来自 `expected_before` 的快照。
    let (entry, expected_revision) = match &intent.before {
        TargetBefore::Present(snapshot) => match Revision::new(snapshot.revision) {
            Ok(revision) => (snapshot.entry, revision),
            Err(error) => {
                return unresolved("invalid_before_revision", error.to_string());
            }
        },
        // 意图只记录“目标当时不存在”，没有条目身份与 revision 证据；put 的元数据事务无法收尾。
        // 新建条目的恢复属 create/move/delete 意图（S038 承接），这里不猜一个条目身份。
        TargetBefore::Absent => {
            return unresolved(
                "missing_entry_baseline",
                "意图的 expected_before 是“当时不存在”，没有条目/revision 证据可收尾".to_owned(),
            );
        }
    };
    let write = PutWrite {
        intent_id: intent.intent_id,
        root: intent.root,
        entry,
        expected_revision,
        content: ExpectedContent::new(intent.after.digest, intent.after.size),
        target_path: intent.target_path.clone(),
        at,
    };
    match store.apply_put(&write).await {
        Ok(record) => {
            // 事务声称成功时核对证据：有任意一条不符就是适配器违约，必须停写人工核对。
            if !record.phase.is_finished() {
                unresolved(
                    "store_contract_violation",
                    format!("事务后意图阶段是 {} 而不是 finalized", record.phase),
                )
            } else if record.digest != intent.after.digest || record.size != intent.after.size {
                unresolved(
                    "store_contract_violation",
                    format!(
                        "版本内容是 {}（{} 字节），不是意图目标 {}（{} 字节）",
                        record.digest, record.size, intent.after.digest, intent.after.size
                    ),
                )
            } else if record.revision <= expected_revision {
                unresolved(
                    "store_contract_violation",
                    format!(
                        "条目 revision 没有推进：{} 不大于基线 {expected_revision}",
                        record.revision
                    ),
                )
            } else {
                RecoveryOutcome::Completed {
                    version: record.version,
                }
            }
        }
        Err(error) => unresolved(error.code(), error.to_string()),
    }
}

fn unresolved(code: &'static str, detail: String) -> RecoveryOutcome {
    RecoveryOutcome::Unresolved { code, detail }
}

/// 一个 Root 的写入门状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootAdmission {
    /// 还没跑过启动恢复：写入一律拒绝，先恢复（design §11.3 末段）。
    AwaitingRecovery,
    /// 恢复完成且没有需要暂停的证据：写入可以开放。
    Open,
    /// 恢复给出无法解释/未完成的证据：保持关闭，恢复报告随状态保留。
    Paused(RecoveryReport),
}

/// 写入门的状态（诊断与 readiness 用，不含报告本体）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionState {
    /// 等待启动恢复。
    AwaitingRecovery,
    /// 已开放。
    Open,
    /// 已暂停（只读）。
    Paused,
}

impl AdmissionState {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AdmissionState::AwaitingRecovery => "awaiting_recovery",
            AdmissionState::Open => "open",
            AdmissionState::Paused => "paused",
        }
    }
}

/// 写入被拒绝的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteRefusal {
    /// 该 Root 的启动恢复还没完成：不能先接写入再补恢复。
    RecoveryPending {
        /// 被拒绝的 Root。
        root: RootId,
    },
    /// 该 Root 的恢复给出无法解释或未完成的证据：只读，等人工核对（`NEEDS_RECOVERY`）。
    Paused {
        /// 被拒绝的 Root。
        root: RootId,
    },
}

impl WriteRefusal {
    /// 稳定错误码（`recovery_pending` / `needs_recovery`）。
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            WriteRefusal::RecoveryPending { .. } => "recovery_pending",
            WriteRefusal::Paused { .. } => "needs_recovery",
        }
    }

    /// 被拒绝的 Root。
    #[must_use]
    pub fn root(self) -> RootId {
        match self {
            WriteRefusal::RecoveryPending { root } | WriteRefusal::Paused { root } => root,
        }
    }
}

impl fmt::Display for WriteRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteRefusal::RecoveryPending { root } => {
                write!(f, "Root {root} 的启动恢复还没完成，写入未开放")
            }
            WriteRefusal::Paused { root } => {
                write!(
                    f,
                    "Root {root} 处于 NEEDS_RECOVERY：只读，等人工核对恢复报告"
                )
            }
        }
    }
}

impl std::error::Error for WriteRefusal {}

/// Root 写入准入表：**先恢复再开放**的唯一闸门（design §11.3 末段、I13）。
///
/// 新建时所有 Root 都是 [`RootAdmission::AwaitingRecovery`]（默认拒绝），只有
/// [`record`](WriteAdmission::record) 能把状态推进到 `Open` 或 `Paused`；没有“直接打开”的入口。
/// 因此提交入口只要在写任何东西之前调用 [`admit_write`](WriteAdmission::admit_write)，就不可能
/// 先接写入再补恢复。
///
/// 只做进程内状态：跨进程共享一份数据库时的互斥（例如第二个进程未恢复就写）由数据库侧约束与部署
/// 约定保证，属服务端组装卡。
#[derive(Default)]
pub struct WriteAdmission {
    roots: BTreeMap<RootId, RootAdmission>,
}

impl WriteAdmission {
    /// 建立准入门：所有 Root 默认等待恢复。
    #[must_use]
    pub fn new() -> Self {
        WriteAdmission::default()
    }

    /// 记录一个 Root 的恢复报告并据此推进门状态。
    ///
    /// 报告 [`RecoveryReport::pauses_writes`] 为真时保持关闭（报告随状态保留，供诊断与 API 输出）。
    pub fn record(&mut self, report: RecoveryReport) {
        let root = report.root;
        let admission = if report.pauses_writes() {
            RootAdmission::Paused(report)
        } else {
            RootAdmission::Open
        };
        self.roots.insert(root, admission);
    }

    /// 该 Root 当前的门状态（未知 Root 视为等待恢复）。
    #[must_use]
    pub fn state(&self, root: RootId) -> AdmissionState {
        match self.roots.get(&root) {
            Some(RootAdmission::Open) => AdmissionState::Open,
            Some(RootAdmission::Paused(_)) => AdmissionState::Paused,
            Some(RootAdmission::AwaitingRecovery) | None => AdmissionState::AwaitingRecovery,
        }
    }

    /// 该 Root 的恢复报告（暂停或已完成时保留；等待恢复时为 `None`）。
    #[must_use]
    pub fn report(&self, root: RootId) -> Option<&RecoveryReport> {
        match self.roots.get(&root) {
            Some(RootAdmission::Paused(report)) => Some(report),
            Some(RootAdmission::Open) | Some(RootAdmission::AwaitingRecovery) | None => None,
        }
    }

    /// 写入准入检查：只有 `Open` 放行。
    ///
    /// # Errors
    ///
    /// 见 [`WriteRefusal`]：恢复未完成（`recovery_pending`）或恢复暂停（`needs_recovery`）。
    pub fn admit_write(&self, root: RootId) -> Result<(), WriteRefusal> {
        match self.state(root) {
            AdmissionState::Open => Ok(()),
            AdmissionState::AwaitingRecovery => Err(WriteRefusal::RecoveryPending { root }),
            AdmissionState::Paused => Err(WriteRefusal::Paused { root }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::commit::intent::{NewPutIntent, TargetAfter, TargetSnapshot};
    use crate::objects::ContentDigest;
    use crate::ports::intent_log::IntentLogError;
    use crate::ports::object_store::FileKey;
    use crate::ports::put_store::PutStoreError;
    use crate::ports::target::TargetError;
    use crate::test_support::{
        FakeIntentLog, FakePutStore, FakeTarget, file_baseline, intent_id, revision, root_id,
        target_snapshot, user_time, version_id,
    };

    const HEX_A: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const HEX_B: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const HEX_C: &str = "3333333333333333333333333333333333333333333333333333333333333333";

    fn digest(hex: &str) -> ContentDigest {
        ContentDigest::from_hex(hex).expect("测试用摘要合法")
    }

    /// 崩溃后留下的意图（阶段由调用方指定），前提是覆盖内容 `HEX_B`。
    fn intent(phase: PutPhase) -> PutIntent {
        let mut intent = NewPutIntent {
            intent_id: intent_id(0x41),
            root: root_id(0x21),
            target_path: "docs/note.txt".to_owned(),
            staging_path: "put-0041".to_owned(),
            before: TargetBefore::Present(snapshot(HEX_B, 9)),
            after: TargetAfter {
                digest: digest(HEX_A),
                size: 5,
            },
        }
        .prepared();
        intent.phase = phase;
        intent
    }

    fn snapshot(hex: &str, size: u64) -> TargetSnapshot {
        target_snapshot(hex, size)
    }

    #[tokio::test]
    async fn clean_root_has_no_unfinished_intents_and_opens_writes() {
        let mut log = FakeIntentLog::default();
        let mut target = FakeTarget::absent();
        let mut store = FakePutStore::for_entry(file_baseline(root_id(0x21)));
        let mut admission = WriteAdmission::new();
        assert_eq!(
            admission
                .admit_write(root_id(0x21))
                .expect_err("未恢复不得写入")
                .code(),
            "recovery_pending"
        );

        let report = recover_root(
            &mut log,
            &mut target,
            &mut store,
            user_time(200),
            root_id(0x21),
        )
        .await
        .expect("空 Root 恢复完成");
        assert!(report.intents.is_empty());
        assert!(!report.pauses_writes());
        assert!(report.committed_versions().is_empty());
        admission.record(report);
        assert_eq!(admission.state(root_id(0x21)), AdmissionState::Open);
        assert!(admission.admit_write(root_id(0x21)).is_ok());
        // 别的 Root 还没恢复：门是逐 Root 的，不会顺带打开。
        assert_eq!(
            admission
                .admit_write(root_id(0x22))
                .expect_err("别的 Root 未恢复")
                .code(),
            "recovery_pending"
        );
    }

    #[tokio::test]
    async fn unpublished_intent_is_redone_and_produces_a_proven_version() {
        let mut log = FakeIntentLog {
            intents: vec![intent(PutPhase::Prepared)],
            ..FakeIntentLog::default()
        };
        // 现场仍是意图前提（前 hash）：崩溃发生在发布之前。
        let mut target = FakeTarget::present(&snapshot(HEX_B, 9));
        let mut store = FakePutStore::for_entry(file_baseline(root_id(0x21)));

        let report = recover_root(
            &mut log,
            &mut target,
            &mut store,
            user_time(200),
            root_id(0x21),
        )
        .await
        .expect("恢复完成");
        let recovered = &report.intents[0];
        assert_eq!(recovered.decision, RecoveryDecision::Redo);
        assert_eq!(
            recovered.outcome,
            RecoveryOutcome::Redone {
                version: version_id(0x81),
                published: PublishedTarget {
                    identity: FileKey {
                        device: 7,
                        inode: 43,
                        links: 1,
                        bytes: 5,
                    },
                    digest: digest(HEX_A),
                },
            }
        );
        // 重新发布的是意图目标内容，然后补完元数据事务（同一次操作）。
        assert_eq!(target.publishes.len(), 1);
        assert_eq!(target.publishes[0].digest, digest(HEX_A));
        // 收尾（`published → finalized`）由 `apply_put` 的数据库事务完成，核心不再单独推进阶段。
        assert_eq!(log.intents[0].phase, PutPhase::Published);
        let write = &store.writes[0];
        assert_eq!(write.entry, file_baseline(root_id(0x21)).entry);
        assert_eq!(write.expected_revision, revision(7));
        assert_eq!(write.content.digest, digest(HEX_A));
        assert_eq!(write.content.bytes, 5);
        assert!(!report.pauses_writes());
    }

    #[tokio::test]
    async fn published_intent_completes_the_metadata_transaction_without_republishing() {
        for phase in [PutPhase::Prepared, PutPhase::Published] {
            let mut log = FakeIntentLog {
                intents: vec![intent(phase)],
                ..FakeIntentLog::default()
            };
            // 后 hash 已在工作目录里：崩溃发生在发布之后、事务之前。
            let mut target = FakeTarget::present(&snapshot(HEX_A, 5));
            let mut store = FakePutStore::for_entry(file_baseline(root_id(0x21)));

            let report = recover_root(
                &mut log,
                &mut target,
                &mut store,
                user_time(200),
                root_id(0x21),
            )
            .await
            .expect("恢复完成");
            assert_eq!(
                report.intents[0].outcome,
                RecoveryOutcome::Completed {
                    version: version_id(0x81)
                },
                "阶段 {phase}"
            );
            assert_eq!(report.intents[0].decision, RecoveryDecision::Complete);
            assert!(
                target.publishes.is_empty(),
                "现场已是目标内容时不得再发布一次（阶段 {phase}）"
            );
            assert_eq!(store.writes.len(), 1);
            // 补完的事务就是 put 的元数据事务：输入是发布后的 published 意图，收尾由适配器事务完成。
            assert_eq!(store.writes[0].intent_id, intent_id(0x41));
            assert_eq!(log.intents[0].phase, phase);
        }
    }

    #[tokio::test]
    async fn externally_rewritten_target_keeps_both_versions_and_commits_nothing() {
        let mut log = FakeIntentLog {
            intents: vec![intent(PutPhase::Published)],
            ..FakeIntentLog::default()
        };
        // 第三种内容：既不是前 hash 也不是后 hash（发布后又被外部改写）。
        let mut target = FakeTarget::present(&snapshot(HEX_C, 7));
        let mut store = FakePutStore::for_entry(file_baseline(root_id(0x21)));

        let report = recover_root(
            &mut log,
            &mut target,
            &mut store,
            user_time(200),
            root_id(0x21),
        )
        .await
        .expect("恢复完成");
        assert_eq!(
            report.intents[0].outcome,
            RecoveryOutcome::Preserved {
                reason: ReconcileReason::PublishedTargetChanged
            }
        );
        assert!(report.needs_reconciliation());
        assert!(!report.pauses_writes(), "可解释的外部改写只阻塞该条目");
        assert!(target.publishes.is_empty(), "不得盲目覆盖外部内容");
        assert!(store.writes.is_empty(), "没有发布证据可收尾");
        assert_eq!(log.intents[0].phase, PutPhase::Published, "意图保留为证据");
        // 前/后两个候选版本都在报告里：旧内容对象是 intent.before 的摘要，新内容是 desired_after。
        assert_eq!(
            report.intents[0].intent.before,
            TargetBefore::Present(snapshot(HEX_B, 9))
        );
        assert_eq!(report.intents[0].intent.after.digest, digest(HEX_A));
    }

    #[tokio::test]
    async fn unreadable_target_marks_needs_recovery_and_keeps_writes_closed() {
        let mut log = FakeIntentLog {
            intents: vec![intent(PutPhase::Prepared)],
            ..FakeIntentLog::default()
        };
        let mut target = FakeTarget::absent();
        target.observe_error = Some(TargetError::Unreadable {
            detail: "EACCES".to_owned(),
        });
        let mut store = FakePutStore::for_entry(file_baseline(root_id(0x21)));

        let report = recover_root(
            &mut log,
            &mut target,
            &mut store,
            user_time(200),
            root_id(0x21),
        )
        .await
        .expect("恢复完成");
        assert_eq!(
            report.intents[0].outcome,
            RecoveryOutcome::NeedsRecovery {
                reason: NeedsRecoveryReason::TargetUnreadable
            }
        );
        assert!(report.pauses_writes());
        assert!(target.publishes.is_empty(), "读不到现场不能当成“可以重做”");
        assert!(store.writes.is_empty());
        assert_eq!(
            log.intents[0].phase,
            PutPhase::NeedsRecovery,
            "无法解释的现场必须落盘为 needs_recovery"
        );
        let mut admission = WriteAdmission::new();
        admission.record(report);
        assert_eq!(admission.state(root_id(0x21)), AdmissionState::Paused);
        assert_eq!(
            admission
                .admit_write(root_id(0x21))
                .expect_err("暂停")
                .code(),
            "needs_recovery"
        );
        assert!(admission.report(root_id(0x21)).is_some());
    }

    #[tokio::test]
    async fn already_marked_intent_stays_paused_without_a_second_advance() {
        let mut log = FakeIntentLog {
            intents: vec![intent(PutPhase::NeedsRecovery)],
            ..FakeIntentLog::default()
        };
        let mut target = FakeTarget::present(&snapshot(HEX_A, 5));
        let mut store = FakePutStore::for_entry(file_baseline(root_id(0x21)));
        let report = recover_root(
            &mut log,
            &mut target,
            &mut store,
            user_time(200),
            root_id(0x21),
        )
        .await
        .expect("恢复完成");
        assert_eq!(
            report.intents[0].outcome,
            RecoveryOutcome::NeedsRecovery {
                reason: NeedsRecoveryReason::MarkedNeedsRecovery
            }
        );
        assert!(report.pauses_writes());
        assert!(
            store.writes.is_empty(),
            "标了 needs_recovery 的意图不再自动收尾"
        );
    }

    #[tokio::test]
    async fn failed_republish_leaves_the_intent_and_keeps_writes_closed() {
        let mut log = FakeIntentLog {
            intents: vec![intent(PutPhase::Prepared)],
            ..FakeIntentLog::default()
        };
        let mut target = FakeTarget::present(&snapshot(HEX_B, 9));
        target.publish_error = Some(TargetError::Storage {
            stage: "stage_publish",
            failure: crate::ports::object_store::StorageFailure::NoSpace,
            detail: "No space left on device".to_owned(),
        });
        let mut store = FakePutStore::for_entry(file_baseline(root_id(0x21)));
        let report = recover_root(
            &mut log,
            &mut target,
            &mut store,
            user_time(200),
            root_id(0x21),
        )
        .await
        .expect("恢复完成");
        assert_eq!(
            report.intents[0].outcome,
            RecoveryOutcome::Unresolved {
                code: "no_space",
                detail: target_error_text(),
            }
        );
        assert!(report.pauses_writes());
        assert_eq!(log.intents[0].phase, PutPhase::Prepared, "重做失败不改阶段");
        assert!(store.writes.is_empty(), "没有发布证据不得收尾");
        assert!(!report.intents[0].outcome.is_committed());
    }

    fn target_error_text() -> String {
        TargetError::Storage {
            stage: "stage_publish",
            failure: crate::ports::object_store::StorageFailure::NoSpace,
            detail: "No space left on device".to_owned(),
        }
        .to_string()
    }

    #[tokio::test]
    async fn metadata_failure_does_not_report_a_new_version() {
        for error in [
            PutStoreError::Storage {
                detail: "SQLITE_BUSY".to_owned(),
            },
            PutStoreError::RevisionChanged {
                expected: revision(7),
                actual: revision(9),
            },
        ] {
            let expected_code = error.code();
            let mut log = FakeIntentLog {
                intents: vec![intent(PutPhase::Published)],
                ..FakeIntentLog::default()
            };
            let mut target = FakeTarget::present(&snapshot(HEX_A, 5));
            let mut store = FakePutStore::for_entry(file_baseline(root_id(0x21)));
            store.apply_error = Some(error);
            let report = recover_root(
                &mut log,
                &mut target,
                &mut store,
                user_time(200),
                root_id(0x21),
            )
            .await
            .expect("恢复完成");
            match &report.intents[0].outcome {
                RecoveryOutcome::Unresolved { code, .. } => assert_eq!(*code, expected_code),
                other => panic!("库失败不能报告新版本：{other:?}"),
            }
            assert!(report.pauses_writes());
            assert!(report.committed_versions().is_empty());
            assert_eq!(log.intents[0].phase, PutPhase::Published);
        }
    }

    #[tokio::test]
    async fn transaction_claims_success_with_wrong_evidence_pauses_writes() {
        let mut log = FakeIntentLog {
            intents: vec![intent(PutPhase::Published)],
            ..FakeIntentLog::default()
        };
        let mut target = FakeTarget::present(&snapshot(HEX_A, 5));
        let mut store = FakePutStore::for_entry(file_baseline(root_id(0x21)));
        store.record = Some(crate::ports::put_store::PutRecord {
            intent_id: intent_id(0x41),
            phase: PutPhase::Finalized,
            version: version_id(0x81),
            revision: revision(8),
            seq: crate::test_support::root_seq(11),
            tx: crate::test_support::tx_id(0x71),
            // 事务说提交的内容不是意图目标：适配器违约。
            digest: digest(HEX_C),
            size: 7,
        });
        let report = recover_root(
            &mut log,
            &mut target,
            &mut store,
            user_time(200),
            root_id(0x21),
        )
        .await
        .expect("恢复完成");
        match &report.intents[0].outcome {
            RecoveryOutcome::Unresolved { code, .. } => {
                assert_eq!(*code, "store_contract_violation");
            }
            other => panic!("证据不符必须停写：{other:?}"),
        }
        assert!(report.pauses_writes());
        assert!(report.committed_versions().is_empty());
    }

    #[tokio::test]
    async fn intent_without_entry_baseline_is_not_finalized_with_an_invented_identity() {
        let mut new = NewPutIntent {
            intent_id: intent_id(0x42),
            root: root_id(0x21),
            target_path: "docs/new.txt".to_owned(),
            staging_path: "put-0042".to_owned(),
            before: TargetBefore::Absent,
            after: TargetAfter {
                digest: digest(HEX_A),
                size: 5,
            },
        }
        .prepared();
        new.phase = PutPhase::Published;
        let mut log = FakeIntentLog {
            intents: vec![new],
            ..FakeIntentLog::default()
        };
        let mut target = FakeTarget::present(&snapshot(HEX_A, 5));
        let mut store = FakePutStore::for_entry(file_baseline(root_id(0x21)));
        let report = recover_root(
            &mut log,
            &mut target,
            &mut store,
            user_time(200),
            root_id(0x21),
        )
        .await
        .expect("恢复完成");
        match &report.intents[0].outcome {
            RecoveryOutcome::Unresolved { code, .. } => {
                assert_eq!(*code, "missing_entry_baseline");
            }
            other => panic!("没有条目证据不得收尾：{other:?}"),
        }
        assert!(report.pauses_writes());
        assert!(store.writes.is_empty());
    }

    #[tokio::test]
    async fn unreadable_intent_log_returns_an_error_and_never_opens_writes() {
        // 让 unfinished_puts 失败：用一个读不出清单的日志模拟越权/存储失败。
        struct FailingLog;
        impl IntentLog for FailingLog {
            async fn begin_put(
                &mut self,
                _new: &NewPutIntent,
                _at: UtcTimestamp,
            ) -> Result<PutIntent, IntentLogError> {
                Err(IntentLogError::OutOfScope)
            }
            async fn advance_put(
                &mut self,
                _id: IntentId,
                _next: PutPhase,
                _at: UtcTimestamp,
            ) -> Result<PutIntent, IntentLogError> {
                Err(IntentLogError::OutOfScope)
            }
            async fn load_put(
                &mut self,
                _id: IntentId,
            ) -> Result<Option<PutIntent>, IntentLogError> {
                Err(IntentLogError::OutOfScope)
            }
            async fn unfinished_puts(
                &mut self,
                _root: RootId,
            ) -> Result<Vec<PutIntent>, IntentLogError> {
                Err(IntentLogError::Storage {
                    detail: "SQLITE_BUSY".to_owned(),
                })
            }
        }
        let mut target = FakeTarget::absent();
        let mut store = FakePutStore::for_entry(file_baseline(root_id(0x21)));
        let error = recover_root(
            &mut FailingLog,
            &mut target,
            &mut store,
            user_time(200),
            root_id(0x21),
        )
        .await
        .expect_err("读不出清单");
        assert_eq!(error.code(), "intent_storage_failed");
        let admission = WriteAdmission::new();
        assert_eq!(
            admission
                .admit_write(root_id(0x21))
                .expect_err("保持关闭")
                .code(),
            "recovery_pending"
        );
    }

    #[test]
    fn admitted_states_are_short_named_for_readiness() {
        for (state, name) in [
            (AdmissionState::AwaitingRecovery, "awaiting_recovery"),
            (AdmissionState::Open, "open"),
            (AdmissionState::Paused, "paused"),
        ] {
            assert_eq!(state.as_str(), name);
        }
    }
}
