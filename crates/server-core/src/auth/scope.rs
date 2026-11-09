//! Vault 授权：每次资源请求按 owner/vault scope 检查（design §17.1、I15、T31）。
//!
//! design §17.1 的要求是“即使只有一个用户，数据访问也走统一授权接口，不能写死‘登录成功即可访问磁盘
//! 任意路径’”。因此授权分成两个必须同时成立的判定，缺一不可：
//!
//! 1. **凭据 → 账号**：由 [`crate::auth::session::authenticate`] 完成（会话有效、未到期、未撤销），
//!    产出 [`Principal`]。这一步只回答“你是谁”。
//! 2. **账号 → Vault**：由本模块的 [`require_vault_scope`] 判定。这一步只回答“这个 Vault 是不是你的”。
//!
//! 不变量：
//!
//! - **纯规则、无 I/O**：本模块只比较 owner，不查库。适配器必须先用
//!   `vaults::vault_in_scope(conn, vault_id, principal.user_id)` 取行——那次查询把“不是你
//!   的 Vault”和“不存在的 Vault”压成同一个 `None`，因此不会因为“先取行再比较”而泄露 Vault 是否存在。
//! - **失败只有一种对外结论**：越权一律 [`ScopeError::AccessDenied`]（design §15.4 的 403
//!   `ACCESS_DENIED`），不区分“别人的 Vault”和“没有这个 Vault”。
//! - **首发只有 owner**：`vaults.owner_id` 是唯一授权来源（design §7.2、§17.1），没有成员表；
//!   后续共享目录增加成员权限表时改这一处，不改凭据与会话。
//! - **响应不泄露绝对路径**：越权结论里不含路径或 Vault 名，日志里也只记身份，不记 `absolute_path`
//!   （design §17.3：详细文件路径只在受控本地诊断中使用）。

use core::fmt;

use vaultsyncd_domain::ids::{UserId, VaultId};

use crate::auth::session::Principal;

/// Vault 授权的结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeError {
    /// 当前账号对该 Vault 没有权限（含 Vault 不存在：两者对外不可区分）。
    AccessDenied,
}

impl ScopeError {
    /// 稳定错误码：映射 HTTP 403 `ACCESS_DENIED`（design §15.4）。
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::AccessDenied => "access_denied",
        }
    }
}

impl fmt::Display for ScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccessDenied => f.write_str("当前账号对该 Vault 没有权限"),
        }
    }
}

impl std::error::Error for ScopeError {}

/// 已确认归属的一个 Vault（端口/适配器查出来的事实）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VaultGrant {
    /// Vault 身份。
    pub vault_id: VaultId,
    /// Vault 的 owner（`vaults.owner_id`）。
    pub owner_id: UserId,
}

/// 检查当前调用者是否可以使用这个 Vault。
///
/// # Errors
///
/// Vault 不属于当前账号时返回 [`ScopeError::AccessDenied`]；成功时不返回额外信息——调用者已经
/// 持有 [`VaultGrant`]，不需要再被“授权令牌”二次确认。
pub fn require_vault_scope(principal: &Principal, grant: &VaultGrant) -> Result<(), ScopeError> {
    if grant.owner_id == principal.user_id {
        Ok(())
    } else {
        Err(ScopeError::AccessDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use vaultsyncd_domain::ids::SessionId;

    fn principal(marker: u8) -> Principal {
        Principal {
            user_id: UserId::from_bytes([marker; 16]),
            session_id: SessionId::from_bytes([0x77; 16]),
        }
    }

    fn grant(owner: u8) -> VaultGrant {
        VaultGrant {
            vault_id: VaultId::from_bytes([0x21; 16]),
            owner_id: UserId::from_bytes([owner; 16]),
        }
    }

    #[test]
    fn own_vault_is_allowed_and_another_owners_vault_is_denied() {
        assert_eq!(require_vault_scope(&principal(0x11), &grant(0x11)), Ok(()));
        assert_eq!(
            require_vault_scope(&principal(0x11), &grant(0x22)),
            Err(ScopeError::AccessDenied)
        );
    }

    #[test]
    fn denial_carries_no_path_or_vault_identity() {
        let error = require_vault_scope(&principal(0x11), &grant(0x22)).expect_err("越权必须拒绝");
        // 对外只有短码与固定文本：不含 Vault 身份、也不含任何路径（T31“日志不泄露凭据”的反面：
        // 越权结论也不泄露服务器目录结构）。
        assert_eq!(error.code(), "access_denied");
        assert_eq!(error.to_string(), "当前账号对该 Vault 没有权限");
        assert!(!error.to_string().contains('/'));
    }
}
