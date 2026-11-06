//! 账号改密的纯规则；更新散列与撤销全部会话必须由适配器同事务提交。

use super::init::validate_password;
use super::password::{PasswordHasher, PasswordVerdict};

/// 改密校验失败，不携带明文或散列。
#[derive(Debug, PartialEq, Eq)]
pub enum ChangeError {
    /// 当前密码不匹配。
    InvalidCurrent,
    /// 新密码不满足初始化时相同的规则。
    InvalidNew,
    /// 已存散列损坏或计算失败。
    Unavailable,
}

/// 验证当前密码并生成独立盐的新散列。
///
/// # Errors
/// 当前密码、新密码规则或散列服务不满足要求时拒绝，不产生任何持久更改。
pub fn prepare_change(
    hasher: &PasswordHasher,
    stored: &str,
    current: &str,
    new: &str,
) -> Result<String, ChangeError> {
    match hasher.verify(stored, current) {
        PasswordVerdict::Matched => {}
        PasswordVerdict::Mismatched => return Err(ChangeError::InvalidCurrent),
        PasswordVerdict::Malformed { .. } => return Err(ChangeError::Unavailable),
    }
    validate_password(new).map_err(|_| ChangeError::InvalidNew)?;
    hasher.hash(new).map_err(|_| ChangeError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_reuses_password_policy_and_never_accepts_broken_hash() {
        let hasher = PasswordHasher::with_params(8, 1, 1).unwrap();
        let stored = hasher.hash("old-password-long").unwrap();
        assert_eq!(
            prepare_change(&hasher, &stored, "wrong", "new-password-long"),
            Err(ChangeError::InvalidCurrent)
        );
        assert_eq!(
            prepare_change(&hasher, &stored, "old-password-long", "short"),
            Err(ChangeError::InvalidNew)
        );
        assert_eq!(
            prepare_change(&hasher, "broken", "old-password-long", "new-password-long"),
            Err(ChangeError::Unavailable)
        );
        let new =
            prepare_change(&hasher, &stored, "old-password-long", "new-password-long").unwrap();
        assert_ne!(stored, new);
        assert_eq!(
            hasher.verify(&new, "new-password-long"),
            PasswordVerdict::Matched
        );
        assert_eq!(
            hasher.verify(&new, "old-password-long"),
            PasswordVerdict::Mismatched
        );
    }
}
