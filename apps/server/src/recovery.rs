//! 启动恢复的进程级编排：先把每个已注册 Root 的未完成意图查清楚，再决定 readiness。
//!
//! 判定与执行仍在 `vaultsyncd-server-core::recovery`（S034/S037）：本模块只做三件事——
//! 从数据库取出已注册 Root、逐 Root 读未完成意图清单、把结论写进 [`Lifecycle`]。
//!
//! | 清单 | 处理 | readiness |
//! | --- | --- | --- |
//! | 空 | 给该 Root 记录一份“无需停写”的恢复报告（开放写入） | 继续看下一个 Root |
//! | 非空 | **不重放**：真实重放要核对现场并补元数据事务，需要 `PutCommitStore` 的真实存储适配（S035/S037 的已知缺口，属后续组装卡）；这里如实报告“恢复没完成” | 保持为假 |
//! | 读不出来 | 直接失败并拒绝启动：`IntentLog` 的契约是“读不出就不能假设没有未完成意图” | 不会启动 |
//!
//! 因此本卡不会把“有未完成意图”的 Root 声称为已恢复，也不会替它清空证据：意图行、已发布文件与
//! 对象都保持原样，交由后续的存储适配卡按 §11.3 的矩阵收尾。

use std::fmt;

use vaultsyncd_domain::ids::RootId;
use vaultsyncd_server_core::recovery::startup::RecoveryReport;
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::intents::{self, IntentError};
use vaultsyncd_server_store::roots::{self, RootError};

use crate::lifecycle::Lifecycle;

/// 一个因未完成意图而没有开放写入的 Root。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockedRoot {
    /// Root 身份。
    pub root: RootId,
    /// 该 Root 下尚未收尾的意图条数（`prepared`/`published`/`needs_recovery`）。
    pub unfinished: usize,
}

/// 启动恢复的结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryAssessment {
    /// 恢复给出“不需要停写”结论的 Root 条数（含报告为空的情况）。
    pub open_roots: usize,
    /// 仍未开放写入的 Root。
    pub blocked_roots: Vec<BlockedRoot>,
}

impl RecoveryAssessment {
    /// 是否所有 Root 都可以开放写入。
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.blocked_roots.is_empty()
    }

    /// 给健康探测与日志用的简短说明：只说身份与条数，不含服务器绝对路径。
    #[must_use]
    pub fn detail(&self) -> String {
        if self.blocked_roots.is_empty() {
            return format!("{} 个 Root 的启动恢复已完成", self.open_roots);
        }
        let list = self
            .blocked_roots
            .iter()
            .map(|blocked| format!("{}（{} 条未完成意图）", blocked.root, blocked.unfinished))
            .collect::<Vec<_>>()
            .join("、");
        format!(
            "{} 个 Root 还有未完成的启动恢复：{list}",
            self.blocked_roots.len()
        )
    }
}

/// 启动恢复编排失败：不能启动，也不能假设“没有未完成意图”。
#[derive(Debug)]
pub enum RecoveryAssessmentError {
    /// 读已注册 Root 失败。
    Roots(RootError),
    /// 某个 Root 的未完成意图读不出来（存储失败或行不合规）。
    Intents {
        /// 出问题的 Root。
        root: RootId,
        /// 原始错误。
        source: IntentError,
    },
}

impl fmt::Display for RecoveryAssessmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecoveryAssessmentError::Roots(error) => {
                write!(f, "读不出已注册 Root：{error}")
            }
            RecoveryAssessmentError::Intents { root, source } => {
                write!(f, "Root {root} 的未完成意图读不出来：{source}")
            }
        }
    }
}

impl std::error::Error for RecoveryAssessmentError {}

/// 逐个已注册 Root 评估启动恢复，并把结论写进 [`Lifecycle`]。
///
/// 有未完成意图时调用方仍可继续运行（只读 + 健康探测报 `needs_recovery`），但写入保持关闭。
///
/// # Errors
///
/// 见 [`RecoveryAssessmentError`]：拿不到已注册 Root 或拿不到某个 Root 的未完成清单时，
/// 不能假设“没有未完成意图”而继续开放写入。
pub async fn assess_registered_roots(
    db: &Database,
    lifecycle: &Lifecycle,
) -> Result<RecoveryAssessment, RecoveryAssessmentError> {
    let mut conn = db
        .pool()
        .acquire()
        .await
        .map_err(|source| RecoveryAssessmentError::Roots(RootError::Db(source)))?;

    let registered = roots::all_roots(&mut conn)
        .await
        .map_err(RecoveryAssessmentError::Roots)?;

    let mut open_roots = 0;
    let mut blocked_roots = Vec::new();
    for root in registered {
        let unfinished = intents::unfinished_intents(&mut conn, root.owner, root.id)
            .await
            .map_err(|source| RecoveryAssessmentError::Intents {
                root: root.id,
                source,
            })?;
        if unfinished.is_empty() {
            // 空清单是“已核对过、没有需要收尾的意图”的证据，不是猜的默认值。
            lifecycle.record_recovery(RecoveryReport {
                root: root.id,
                intents: Vec::new(),
            });
            open_roots += 1;
        } else {
            blocked_roots.push(BlockedRoot {
                root: root.id,
                unfinished: unfinished.len(),
            });
        }
    }

    Ok(RecoveryAssessment {
        open_roots,
        blocked_roots,
    })
}

/// 统计全部 Root 下尚未收尾的意图条数（停止流程用它证明“证据仍然保留”）。
///
/// # Errors
///
/// 与 [`assess_registered_roots`] 相同的读失败。
pub async fn count_unfinished_intents(db: &Database) -> Result<usize, RecoveryAssessmentError> {
    let mut conn = db
        .pool()
        .acquire()
        .await
        .map_err(|source| RecoveryAssessmentError::Roots(RootError::Db(source)))?;
    let registered = roots::all_roots(&mut conn)
        .await
        .map_err(RecoveryAssessmentError::Roots)?;
    let mut total = 0;
    for root in registered {
        let unfinished = intents::unfinished_intents(&mut conn, root.owner, root.id)
            .await
            .map_err(|source| RecoveryAssessmentError::Intents {
                root: root.id,
                source,
            })?;
        total += unfinished.len();
    }
    Ok(total)
}
