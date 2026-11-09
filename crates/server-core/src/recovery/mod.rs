//! 未完成 put 意图的恢复判定（design §11.3 恢复矩阵）。
//!
//! 恢复只读两种持久证据：`fs_intents` 行（阶段 + `expected_before`/`desired_after`）与工作目录现场
//! （目标是否还在、是什么内容）。据此给出四个可执行结论：
//!
//! | 证据 | 结论 |
//! | --- | --- |
//! | `prepared` 且现场满足 `expected_before` | [`RecoveryDecision::Redo`]：重做发布（§11.3 第 3 行） |
//! | 现场已是 `desired_after` 内容（`prepared` 或 `published`） | [`RecoveryDecision::Complete`]：完成同一操作（第 4 行） |
//! | 现场是第三种状态 | [`RecoveryDecision::Reconcile`]：保留两个可读版本，等协调，不盲目覆盖 |
//! | 拿不到可证明的现场（读不到）或意图已被标 `needs_recovery` | [`RecoveryDecision::NeedsRecovery`]：暂停该 Vault 的破坏性写入 |
//!
//! 三条不变量：
//!
//! - **故障不是删除**：目标不可读（权限/挂载/IO）只走向 `NeedsRecovery`，绝不解释为“目标不存在”
//!   再去重做发布；
//! - **不盲目覆盖**：现场既非前提也非目标时只产出协调结论，不产生新的写入计划；
//! - **判定不产生提交**：本模块只给结论，不写数据库、不动工作文件；`Complete` 也必须由提交路径在
//!   一次事务里收尾，失败即回滚（S035/S037）。
//!
//! [`startup`] 在这套判定之上执行恢复：按未完成意图重做或补完元数据事务、把无法解释的现场落盘为
//! `needs_recovery`，并由 [`startup::WriteAdmission`] 保证“先恢复再开放写入口”。本模块只覆盖 put
//! 的未完成意图；create/move/delete 的意图日志由后续卡接入同一条启动顺序。

use std::fmt;

use vaultsyncd_domain::ids::IntentId;

use crate::commit::intent::{PutIntent, PutPhase};
use crate::ports::target::{TargetError, TargetObservation};

pub mod startup;

/// 现场是第三种状态时，需要协调的具体情形。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileReason {
    /// 意图停在 `prepared`，但目标既不是 `expected_before` 也不是 `desired_after`：意图之前/之后有人
    /// 改过它，不能重做覆盖。
    PreparedTargetChanged,
    /// 意图停在 `published`，目标既不是发布内容也不是原内容：发布后又被外部修改。
    PublishedTargetChanged,
    /// 意图停在 `published`，目标却回到了 `expected_before`：发布内容已被外部回退，数据库阶段与现场
    /// 矛盾，不能当成“已发布”。
    PublishedTargetReverted,
}

impl ReconcileReason {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReconcileReason::PreparedTargetChanged => "prepared_target_changed",
            ReconcileReason::PublishedTargetChanged => "published_target_changed",
            ReconcileReason::PublishedTargetReverted => "published_target_reverted",
        }
    }
}

impl fmt::Display for ReconcileReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 只能暂停、不能自动推进的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeedsRecoveryReason {
    /// 目标读不到（权限、挂载、IO）：没有可证明的现场。
    TargetUnreadable,
    /// 意图已被（人工或上轮恢复）标成 `needs_recovery`。
    MarkedNeedsRecovery,
}

impl NeedsRecoveryReason {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NeedsRecoveryReason::TargetUnreadable => "target_unreadable",
            NeedsRecoveryReason::MarkedNeedsRecovery => "marked_needs_recovery",
        }
    }
}

impl fmt::Display for NeedsRecoveryReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条未完成意图的恢复结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// 意图已 `finalized`：不在待恢复清单里，不需要动作。
    AlreadyFinished,
    /// 现场满足 `expected_before`：按同一意图重做发布。
    Redo,
    /// 现场已是 `desired_after`：完成同一操作的元数据事务并收尾。
    Complete,
    /// 现场无法用意图解释但可以保留双方：等协调，不覆盖。
    Reconcile {
        /// 具体情形。
        reason: ReconcileReason,
    },
    /// 没有可证明的现场：暂停该 Vault 的破坏性写入，输出恢复报告。
    NeedsRecovery {
        /// 具体原因。
        reason: NeedsRecoveryReason,
    },
}

impl RecoveryDecision {
    /// 稳定短名（恢复报告与日志用）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RecoveryDecision::AlreadyFinished => "already_finished",
            RecoveryDecision::Redo => "redo",
            RecoveryDecision::Complete => "complete",
            RecoveryDecision::Reconcile { reason } => reason.as_str(),
            RecoveryDecision::NeedsRecovery { reason } => reason.as_str(),
        }
    }

    /// 是否必须暂停该 Vault 的破坏性写入（只有 `NeedsRecovery`）。
    #[must_use]
    pub fn pauses_writes(self) -> bool {
        matches!(self, RecoveryDecision::NeedsRecovery { .. })
    }

    /// 是否需要恢复流程做事（`AlreadyFinished` 以外都要）。
    #[must_use]
    pub fn requires_action(self) -> bool {
        !matches!(self, RecoveryDecision::AlreadyFinished)
    }
}

impl fmt::Display for RecoveryDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 按持久阶段与现场判定一条意图该怎么处理。
///
/// `observation` 是重新观察目标的结果；错误必须原样传进来（而不是先变成
/// [`TargetObservation::Absent`]），否则故障会被读成“目标不存在”。
#[must_use]
pub fn classify_put(
    intent: &PutIntent,
    observation: Result<TargetObservation, TargetError>,
) -> RecoveryDecision {
    if intent.phase == PutPhase::Finalized {
        return RecoveryDecision::AlreadyFinished;
    }
    if intent.phase == PutPhase::NeedsRecovery {
        return RecoveryDecision::NeedsRecovery {
            reason: NeedsRecoveryReason::MarkedNeedsRecovery,
        };
    }
    let observed = match observation {
        Ok(observed) => observed,
        Err(_) => {
            return RecoveryDecision::NeedsRecovery {
                reason: NeedsRecoveryReason::TargetUnreadable,
            };
        }
    };
    match intent.phase {
        PutPhase::Prepared => {
            if intent.before.satisfied_by(&observed) {
                RecoveryDecision::Redo
            } else if intent.after.satisfied_by(&observed) {
                RecoveryDecision::Complete
            } else {
                RecoveryDecision::Reconcile {
                    reason: ReconcileReason::PreparedTargetChanged,
                }
            }
        }
        PutPhase::Published => {
            if intent.after.satisfied_by(&observed) {
                RecoveryDecision::Complete
            } else if intent.before.satisfied_by(&observed) {
                RecoveryDecision::Reconcile {
                    reason: ReconcileReason::PublishedTargetReverted,
                }
            } else {
                RecoveryDecision::Reconcile {
                    reason: ReconcileReason::PublishedTargetChanged,
                }
            }
        }
        PutPhase::Finalized | PutPhase::NeedsRecovery => RecoveryDecision::AlreadyFinished,
    }
}

/// 一条意图的恢复结论（按意图写入顺序排列，便于按序处理）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifiedIntent {
    /// 意图身份。
    pub intent_id: IntentId,
    /// 目标在 Vault 内的相对路径。
    pub target_path: String,
    /// 持久化阶段。
    pub phase: PutPhase,
    /// 恢复结论。
    pub decision: RecoveryDecision,
}

/// 一个 Vault 的恢复计划：按写入顺序的结论 + 是否必须暂停写入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultRecovery {
    /// 每条未完成意图的结论（保持传入顺序）。
    pub intents: Vec<ClassifiedIntent>,
    /// 是否必须暂停该 Vault 的破坏性写入（任一意图 `NeedsRecovery` 即真）。
    pub pauses_writes: bool,
}

impl VaultRecovery {
    /// 按身份查结论。
    #[must_use]
    pub fn decision_for(&self, id: IntentId) -> Option<&ClassifiedIntent> {
        self.intents.iter().find(|entry| entry.intent_id == id)
    }

    /// 是否需要协调（保留双方版本）。
    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        self.intents
            .iter()
            .any(|entry| matches!(entry.decision, RecoveryDecision::Reconcile { .. }))
    }
}

/// 为一个 Vault 的未完成意图生成恢复计划。
///
/// `intents` 必须是意图日志给出的写入顺序（[`crate::ports::intent_log::IntentLog::unfinished_puts`]）；
/// `observe` 由调用方接到目标端口上。只要有任意一条拿不到可证明的现场，
/// [`VaultRecovery::pauses_writes`] 即为真，调用方必须保持写入关闭（design §11.3 末段）。
pub fn plan_vault<F>(intents: &[PutIntent], mut observe: F) -> VaultRecovery
where
    F: FnMut(&PutIntent) -> Result<TargetObservation, TargetError>,
{
    let mut classified = Vec::with_capacity(intents.len());
    let mut pauses_writes = false;
    for intent in intents {
        let decision = classify_put(intent, observe(intent));
        pauses_writes |= decision.pauses_writes();
        classified.push(ClassifiedIntent {
            intent_id: intent.intent_id,
            target_path: intent.target_path.clone(),
            phase: intent.phase,
            decision,
        });
    }
    VaultRecovery {
        intents: classified,
        pauses_writes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::commit::intent::{NewPutIntent, TargetAfter, TargetBefore};
    use crate::objects::ContentDigest;
    use crate::ports::object_store::FileKey;
    use crate::test_support::{intent_id, target_snapshot, vault_id};

    const HEX_A: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const HEX_B: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const HEX_C: &str = "3333333333333333333333333333333333333333333333333333333333333333";

    fn digest(hex: &str) -> ContentDigest {
        ContentDigest::from_hex(hex).expect("测试用摘要合法")
    }

    fn snapshot(size: u64) -> crate::commit::intent::TargetSnapshot {
        target_snapshot(HEX_B, size)
    }

    fn intent(marker: u8, before: TargetBefore, phase: PutPhase) -> PutIntent {
        let mut intent = NewPutIntent {
            intent_id: intent_id(marker),
            vault: vault_id(0x21),
            target_path: format!("docs/note-{marker:02x}.txt"),
            staging_path: format!("put-{marker:02x}"),
            before,
            after: TargetAfter {
                digest: digest(HEX_A),
                size: 5,
            },
        }
        .prepared();
        intent.phase = phase;
        intent
    }

    fn present(hex: &str, size: u64) -> TargetObservation {
        TargetObservation::Present {
            identity: FileKey {
                device: 7,
                inode: 41,
                links: 1,
                bytes: size,
            },
            digest: digest(hex),
        }
    }

    fn unreadable() -> TargetError {
        TargetError::Unreadable {
            detail: "EACCES".to_owned(),
        }
    }

    #[test]
    fn prepared_intent_redoes_only_when_the_expected_before_is_still_there() {
        let unchanged = intent(0x41, TargetBefore::Present(snapshot(9)), PutPhase::Prepared);
        assert_eq!(
            classify_put(&unchanged, Ok(present(HEX_B, 9))),
            RecoveryDecision::Redo
        );
        // 新建意图的前提是“当时不存在”。
        let create = intent(0x42, TargetBefore::Absent, PutPhase::Prepared);
        assert_eq!(
            classify_put(&create, Ok(TargetObservation::Absent)),
            RecoveryDecision::Redo
        );
        // 发布其实已经落地（工作文件换了、阶段没推进）：补完而不是重做。
        assert_eq!(
            classify_put(&unchanged, Ok(present(HEX_A, 5))),
            RecoveryDecision::Complete
        );
        // 第三种状态：不覆盖，去协调。
        assert_eq!(
            classify_put(&unchanged, Ok(present(HEX_C, 7))),
            RecoveryDecision::Reconcile {
                reason: ReconcileReason::PreparedTargetChanged
            }
        );
        // 前提要求目标存在，现场却没有了：不是“可以新建”。
        assert_eq!(
            classify_put(&unchanged, Ok(TargetObservation::Absent)),
            RecoveryDecision::Reconcile {
                reason: ReconcileReason::PreparedTargetChanged
            }
        );
    }

    #[test]
    fn published_intent_completes_only_on_the_desired_content() {
        let published = intent(
            0x41,
            TargetBefore::Present(snapshot(9)),
            PutPhase::Published,
        );
        assert_eq!(
            classify_put(&published, Ok(present(HEX_A, 5))),
            RecoveryDecision::Complete
        );
        assert_eq!(
            classify_put(&published, Ok(present(HEX_B, 9))),
            RecoveryDecision::Reconcile {
                reason: ReconcileReason::PublishedTargetReverted
            }
        );
        assert_eq!(
            classify_put(&published, Ok(present(HEX_C, 7))),
            RecoveryDecision::Reconcile {
                reason: ReconcileReason::PublishedTargetChanged
            }
        );
    }

    #[test]
    fn unreadable_target_or_marked_intent_pauses_writes() {
        for phase in [PutPhase::Prepared, PutPhase::Published] {
            let pending = intent(0x41, TargetBefore::Absent, phase);
            let decision = classify_put(&pending, Err(unreadable()));
            assert_eq!(
                decision,
                RecoveryDecision::NeedsRecovery {
                    reason: NeedsRecoveryReason::TargetUnreadable
                }
            );
            assert!(decision.pauses_writes(), "读不到现场不能继续写");
            assert!(decision.requires_action());
        }
        let marked = intent(0x42, TargetBefore::Absent, PutPhase::NeedsRecovery);
        let decision = classify_put(&marked, Ok(TargetObservation::Absent));
        assert_eq!(
            decision,
            RecoveryDecision::NeedsRecovery {
                reason: NeedsRecoveryReason::MarkedNeedsRecovery
            }
        );
        assert!(decision.pauses_writes());
    }

    #[test]
    fn finalized_intents_need_no_action() {
        let done = intent(0x41, TargetBefore::Absent, PutPhase::Finalized);
        let decision = classify_put(&done, Ok(present(HEX_A, 5)));
        assert_eq!(decision, RecoveryDecision::AlreadyFinished);
        assert!(!decision.pauses_writes());
        assert!(!decision.requires_action());
    }

    #[test]
    fn vault_plan_keeps_write_order_and_pauses_on_any_unreadable_target() {
        let intents = vec![
            intent(0x41, TargetBefore::Absent, PutPhase::Prepared),
            intent(
                0x42,
                TargetBefore::Present(snapshot(9)),
                PutPhase::Published,
            ),
            intent(0x43, TargetBefore::Absent, PutPhase::Prepared),
        ];
        let mut calls = 0;
        let plan = plan_vault(&intents, |intent| {
            calls += 1;
            match intent.intent_id {
                id if id == intent_id(0x41) => Ok(TargetObservation::Absent),
                id if id == intent_id(0x42) => Ok(present(HEX_A, 5)),
                _ => Err(unreadable()),
            }
        });
        assert_eq!(calls, 3);
        assert_eq!(
            plan.intents
                .iter()
                .map(|entry| (entry.intent_id, entry.decision))
                .collect::<Vec<_>>(),
            vec![
                (intent_id(0x41), RecoveryDecision::Redo),
                (intent_id(0x42), RecoveryDecision::Complete),
                (
                    intent_id(0x43),
                    RecoveryDecision::NeedsRecovery {
                        reason: NeedsRecoveryReason::TargetUnreadable
                    }
                ),
            ]
        );
        assert!(plan.pauses_writes, "任一条读不到就必须暂停写入");
        assert!(!plan.needs_reconciliation());
        assert_eq!(
            plan.decision_for(intent_id(0x42))
                .map(|entry| entry.decision),
            Some(RecoveryDecision::Complete)
        );
        assert_eq!(
            plan.decision_for(intent_id(0x42)).map(|entry| entry.phase),
            Some(PutPhase::Published)
        );
        assert_eq!(plan.decision_for(intent_id(0x99)), None);

        // 全部可证明时写入可以继续开放。
        let clean = plan_vault(&intents[..2], |intent| {
            if intent.intent_id == intent_id(0x41) {
                Ok(TargetObservation::Absent)
            } else {
                Ok(present(HEX_A, 5))
            }
        });
        assert!(!clean.pauses_writes);
    }
}
