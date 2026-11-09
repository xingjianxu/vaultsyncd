//! 服务生命周期：readiness 与写入门（design §4.2、§11.3 末段、I13）。
//!
//! 两个事实必须分开：
//!
//! - **readiness**：进程是否已经可以对外声明“可用”。它在启动恢复给出结论前一直是假
//!   （[`Phase::Starting`]），恢复没完成或无法解释时是 [`Phase::NeedsRecovery`]。
//! - **写入准入**：某个 Vault 能不能开始新的提交。它由核心的
//!   [`WriteAdmission`] 逐个 Vault 决定，唯一的推进入口是恢复报告（[`Lifecycle::record_recovery`]）；
//!   本模块只额外加一条“正在停止”的拒绝，不提供任何“直接打开”的旁路。
//!
//! 停止时先 [`begin_shutdown`](Lifecycle::begin_shutdown) 再让 HTTP 服务收尾：这样等待中的请求
//! 已经拿不到新的写入许可，而已经提交的任务证据（意图行、已发布文件）保持原样，由下次启动的
//! 恢复流程处理（design §11.3）。

use std::sync::{Mutex, MutexGuard};

use vaultsyncd_domain::ids::VaultId;
use vaultsyncd_server_core::recovery::startup::{
    AdmissionState, RecoveryReport, WriteAdmission, WriteRefusal,
};

/// 进程生命周期阶段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// 启动中：配置已加载，启动恢复还没有结论。
    Starting,
    /// 可用：所有已注册 Vault 的启动恢复都给出了“不需要停写”的结论。
    Ready,
    /// 恢复未完成或无法解释：可以读，不开放新的写入。
    NeedsRecovery,
    /// 正在停止：不再接受新的提交，等待进行中的请求收尾。
    Draining,
}

impl Phase {
    /// 稳定短名（日志与健康探测共用）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Ready => "ready",
            Phase::NeedsRecovery => "needs_recovery",
            Phase::Draining => "draining",
        }
    }

    /// 是否可以对外声明可用。
    #[must_use]
    pub const fn is_ready(self) -> bool {
        matches!(self, Phase::Ready)
    }
}

/// 一次写入准入请求被拒绝的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmissionRefusal {
    /// 进程正在停止：不再开始新的提交（已开始的由停止流程收尾）。
    ShuttingDown,
    /// 该 Vault 的写入还没开放或已暂停（原因来自核心的写入门）。
    Vault(WriteRefusal),
}

impl SubmissionRefusal {
    /// 稳定错误码（`shutting_down` / `recovery_pending` / `needs_recovery`）。
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            SubmissionRefusal::ShuttingDown => "shutting_down",
            SubmissionRefusal::Vault(refusal) => refusal.code(),
        }
    }
}

impl std::fmt::Display for SubmissionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmissionRefusal::ShuttingDown => f.write_str("服务正在停止，不再接受新的提交"),
            SubmissionRefusal::Vault(refusal) => write!(f, "{refusal}"),
        }
    }
}

impl std::error::Error for SubmissionRefusal {}

/// 进程生命周期与准入状态。HTTP 处理与后续提交入口共享同一个实例。
#[derive(Default)]
pub struct Lifecycle {
    inner: Mutex<Inner>,
}

struct Inner {
    phase: Phase,
    /// 恢复没有完成的说明（不含服务器绝对路径），用于健康探测与日志。
    recovery_detail: Option<String>,
    admission: WriteAdmission,
}

impl Default for Inner {
    fn default() -> Self {
        Inner {
            phase: Phase::Starting,
            recovery_detail: None,
            admission: WriteAdmission::new(),
        }
    }
}

impl Lifecycle {
    /// 新建：阶段为 [`Phase::Starting`]，所有 Vault 的写入都还没开放。
    #[must_use]
    pub fn new() -> Self {
        Lifecycle::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // 生命周期状态只有短小的读改写：持锁期间不可能 panic；即使发生，也继续使用内部值，
        // 因为把进程状态标记为 poisoned 并不会让数据库或文件变得更一致。
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 当前阶段。
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.lock().phase
    }

    /// 恢复未完成的说明（`None` 表示没有记录过）。
    #[must_use]
    pub fn recovery_detail(&self) -> Option<String> {
        self.lock().recovery_detail.clone()
    }

    /// 记录一个 Vault 的启动恢复报告：只有报告不要求停写时才把该 Vault 置为可写。
    pub fn record_recovery(&self, report: RecoveryReport) {
        self.lock().admission.record(report);
    }

    /// 某个 Vault 当前的写入准入状态（诊断用）。
    #[must_use]
    pub fn vault_state(&self, vault: VaultId) -> AdmissionState {
        self.lock().admission.state(vault)
    }

    /// 标记启动恢复全部完成、可以对外声明可用。仍在停止流程时不会回退到可用。
    pub fn mark_ready(&self) {
        let mut inner = self.lock();
        if inner.phase == Phase::Starting {
            inner.phase = Phase::Ready;
            inner.recovery_detail = None;
        }
    }

    /// 标记启动恢复没有完成（有未完成意图或读不出结论）：不开放写入，readiness 保持为假。
    pub fn mark_needs_recovery(&self, detail: impl Into<String>) {
        let mut inner = self.lock();
        if inner.phase != Phase::Draining {
            inner.phase = Phase::NeedsRecovery;
            inner.recovery_detail = Some(detail.into());
        }
    }

    /// 进入停止流程：此后任何新的提交都被拒绝，直到进程退出。
    pub fn begin_shutdown(&self) {
        self.lock().phase = Phase::Draining;
    }

    /// 提交入口的写入门：先看是否正在停止，再问核心的逐 Vault 写入门。
    ///
    /// 后续接入提交接口时，任何写入都必须在做实际工作之前调用它（design §11.3、I13）。
    ///
    /// # Errors
    ///
    /// 见 [`SubmissionRefusal`]。
    pub fn admit_write(&self, vault: VaultId) -> Result<(), SubmissionRefusal> {
        let inner = self.lock();
        if inner.phase == Phase::Draining {
            return Err(SubmissionRefusal::ShuttingDown);
        }
        inner
            .admission
            .admit_write(vault)
            .map_err(SubmissionRefusal::Vault)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use vaultsyncd_domain::ids::ID_LEN;

    fn vault(marker: u8) -> VaultId {
        VaultId::from_bytes([marker; ID_LEN])
    }

    fn clean_report(vault: VaultId) -> RecoveryReport {
        RecoveryReport {
            vault,
            intents: Vec::new(),
        }
    }

    #[test]
    fn writes_stay_closed_until_every_vault_has_a_recovery_report() {
        let lifecycle = Lifecycle::new();
        assert_eq!(lifecycle.phase(), Phase::Starting);
        assert!(!lifecycle.phase().is_ready());
        assert_eq!(
            lifecycle.admit_write(vault(1)).expect_err("未恢复不得写入"),
            SubmissionRefusal::Vault(WriteRefusal::RecoveryPending { vault: vault(1) })
        );
        assert_eq!(
            lifecycle.admit_write(vault(1)).unwrap_err().code(),
            "recovery_pending"
        );

        lifecycle.record_recovery(clean_report(vault(1)));
        assert_eq!(lifecycle.vault_state(vault(1)), AdmissionState::Open);
        assert!(lifecycle.admit_write(vault(1)).is_ok());
        // 另一个 Vault 没有报告，仍然关闭：门是逐 Vault 的。
        assert!(lifecycle.admit_write(vault(2)).is_err());
        assert_eq!(lifecycle.phase(), Phase::Starting, "记录报告本身不等于可用");
    }

    #[test]
    fn readiness_is_only_reached_after_recovery_completes() {
        let lifecycle = Lifecycle::new();
        lifecycle.record_recovery(clean_report(vault(1)));
        lifecycle.mark_ready();
        assert_eq!(lifecycle.phase(), Phase::Ready);
        assert!(lifecycle.phase().is_ready());
        assert!(lifecycle.admit_write(vault(1)).is_ok());
    }

    #[test]
    fn unfinished_recovery_keeps_readiness_false_and_writes_closed() {
        let lifecycle = Lifecycle::new();
        lifecycle.mark_needs_recovery("1 个 Vault 还有未完成意图");
        assert_eq!(lifecycle.phase(), Phase::NeedsRecovery);
        assert!(!lifecycle.phase().is_ready());
        assert!(lifecycle.recovery_detail().is_some());
        assert!(lifecycle.admit_write(vault(1)).is_err());
        // 事后也不能“补一个 mark_ready 就放行”。
        lifecycle.mark_ready();
        assert_eq!(lifecycle.phase(), Phase::NeedsRecovery);
        assert!(lifecycle.admit_write(vault(1)).is_err());
    }

    #[test]
    fn shutdown_refuses_new_submissions_even_for_open_vaults() {
        let lifecycle = Lifecycle::new();
        lifecycle.record_recovery(clean_report(vault(1)));
        lifecycle.mark_ready();
        assert!(lifecycle.admit_write(vault(1)).is_ok());

        lifecycle.begin_shutdown();
        assert_eq!(lifecycle.phase(), Phase::Draining);
        assert_eq!(
            lifecycle
                .admit_write(vault(1))
                .expect_err("停止中不得开始新提交"),
            SubmissionRefusal::ShuttingDown
        );
        assert_eq!(
            lifecycle.admit_write(vault(1)).unwrap_err().code(),
            "shutting_down"
        );
        // 停止之后不再回到可用。
        lifecycle.mark_ready();
        assert_eq!(lifecycle.phase(), Phase::Draining);
    }
}
