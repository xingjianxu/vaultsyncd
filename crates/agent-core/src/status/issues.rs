//! 问题按Vault与原因持久去重；不保存自由诊断文本或绑定外键，解绑不丢其他绑定的问题。
use vaultsyncd_client_store::{
    db::Database,
    notifications::{self, NewNotification, NotificationState},
};
use vaultsyncd_domain::{error::ErrorCode, ids::VaultId};
use vaultsyncd_transfer::ports::PortError;
/// 一次上报的通知决策；系统通知仍由桌面入口发送。
#[derive(Debug, PartialEq, Eq)]
pub struct Decision {
    /// 本次用户打开失败须立即反馈。
    pub explain_open: bool,
    /// 严重问题首次发生或恢复后复发，仅一次通知。
    pub notify: bool,
}
/// 普通网络故障静默；严重原因同Vault合并。输入code由可信错误分类产生。
pub async fn report(
    db: &Database,
    vault: VaultId,
    code: ErrorCode,
    user_open: bool,
) -> Result<Decision, PortError> {
    if !code.class().is_severe() {
        return Ok(Decision {
            explain_open: user_open,
            notify: false,
        });
    }
    let key = format!("vault:{vault}:{code}");
    let mut tx = db.begin_write().await.map_err(super::error)?;
    let old = notifications::notification(&mut tx, &key)
        .await
        .map_err(super::error)?;
    let notify = old.is_none_or(|n| n.state == NotificationState::Resolved);
    notifications::raise(
        &mut tx,
        &NewNotification {
            dedupe_key: key,
            binding: None,
            entry: None,
            code,
        },
    )
    .await
    .map_err(super::error)?;
    tx.commit().await.map_err(super::error)?;
    Ok(Decision {
        explain_open: user_open,
        notify,
    })
}
/// 仅当Vault级聚合探测确认所有绑定同原因已恢复后调用，不能以单绑定成功替代。
pub async fn resolve_vault(
    db: &Database,
    vault: VaultId,
    code: ErrorCode,
) -> Result<bool, PortError> {
    let mut tx = db.begin_write().await.map_err(super::error)?;
    let result = notifications::resolve(&mut tx, &format!("vault:{vault}:{code}"))
        .await
        .map_err(super::error)?;
    tx.commit().await.map_err(super::error)?;
    Ok(result)
}
/// A committed conflict copy requires review; its explicit cause is separate from transient CAS retry.
pub async fn report_conflict(db: &Database, vault: VaultId) -> Result<(), PortError> {
    let mut tx = db.begin_write().await.map_err(super::error)?;
    notifications::raise(
        &mut tx,
        &NewNotification {
            dedupe_key: format!("vault:{vault}:CONFLICT_COPY"),
            binding: None,
            entry: None,
            code: ErrorCode::NeedsRecovery,
        },
    )
    .await
    .map_err(super::error)?;
    tx.commit().await.map_err(super::error)
}
/// Conflict policy for application adapters that must atomically persist it with a committed ACK.
pub fn conflict_problem(vault: VaultId) -> NewNotification {
    NewNotification {
        dedupe_key: format!("vault:{vault}:CONFLICT_COPY"),
        binding: None,
        entry: None,
        code: ErrorCode::NeedsRecovery,
    }
}
