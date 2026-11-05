//! 密码散列：Argon2id 的 PHC 文本产出与校验（design §17.1、C27、[R12]）。
//!
//! 只做一件事：把一个密码变成可入库的 PHC 文本，并把“这个密码对得上吗”回答成三分结论。业务判断
//! （能否初始化、登录失败怎么统一、限流怎么算）在 [`crate::auth::init`] 与 [`crate::auth::login`]。
//!
//! 不变量：
//!
//! - **只支持 Argon2id**：校验时按 PHC 文本里的算法字段核对，读到 md5/sha*/pbkdf2/argon2i/argon2d 一律
//!   [`PasswordVerdict::Malformed`]。这是“不自行设计密码学”的落实：允许哪些算法是白名单，不是“库认识
//!   哪些就用哪些”。
//! - **每个散列独立随机盐**：盐由操作系统随机源生成（[`SaltString::generate`] 与 `OsRng`），不复用、
//!   不派生自登录名或时间戳。
//! - **失败不猜**：解析不出的散列不是“密码错”，`Malformed` 与 `Mismatched` 分开返回——前者说明库里
//!   的行坏了，需要运维处理，而不是让用户重试密码。
//! - **没有明文日志**：本模块不写日志、不含 `Debug` 打印密码的结构；错误文本只带 PHC 解析原因。
//!
//! 参数取 RFC 9106 的第二推荐配置方向（Argon2id、v19、单线程）落到可实测的基线
//! `m=19456 KiB, t=2, p=1`。design §17.1 要求“参数参考 RFC 9106 并结合目标服务器实测后写入 ADR”：
//! 实测与 ADR 尚未执行，因此参数集中在本文件的常量里，ADR 结论落地时只改这一处（已有散列自带参数，
//! 校验不依赖这里的常量，改参数不需要重算旧散列）。

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};

/// Argon2id 的内存代价（KiB）：64 MiB 以下、RFC 9106 第二推荐配置的量级。
pub const M_COST_KIB: u32 = 19_456;

/// Argon2id 的时间代价（迭代次数）。
pub const T_COST: u32 = 2;

/// Argon2id 的并行度。
pub const P_COST: u32 = 1;

/// 校验未知账号时使用的固定盐：只用于消耗与真实校验相同的 CPU/内存，不参与任何真实散列。
const DUMMY_SALT: [u8; 16] = [0x5a; 16];

/// 密码散列器。生产路径用 [`Self::production`]，参数由本文件的常量决定。
#[derive(Debug)]
pub struct PasswordHasher {
    argon2: Argon2<'static>,
}

impl PasswordHasher {
    /// 用本卡的生产参数（[`M_COST_KIB`]/[`T_COST`]/[`P_COST`]、Argon2id v19）。
    ///
    /// # Panics
    ///
    /// 常量组合必须能被 Argon2 接受；这是编译期常量，出错属于编码缺陷，测试
    /// `production_parameters_are_accepted` 会在 CI 里立刻暴露。
    #[must_use]
    pub fn production() -> Self {
        Self::with_params(M_COST_KIB, T_COST, P_COST).expect("生产 Argon2id 参数必须被库接受")
    }

    /// 用显式参数构造（测试用小参数，避免每个用例花几十毫秒）。
    ///
    /// # Errors
    ///
    /// 参数越界（Argon2 的上限）时返回 [`PasswordError::InvalidParams`]。
    pub fn with_params(m_cost_kib: u32, t_cost: u32, p_cost: u32) -> Result<Self, PasswordError> {
        let params = Params::new(m_cost_kib, t_cost, p_cost, None).map_err(|source| {
            PasswordError::InvalidParams {
                detail: source.to_string(),
            }
        })?;
        Ok(Self {
            argon2: Argon2::new(Algorithm::Argon2id, Version::V0x13, params),
        })
    }

    /// 计算一个新的 PHC 文本（含独立随机盐）。
    ///
    /// # Errors
    ///
    /// 随机源不可用、盐生成失败或散列计算失败时返回 [`PasswordError::HashFailed`]——此时绝不能
    /// 用固定值或空串兜底写入数据库。
    pub fn hash(&self, password: &str) -> Result<String, PasswordError> {
        let salt = SaltString::generate(&mut OsRng);
        let hash = self
            .argon2
            .hash_password(password.as_bytes(), &salt)
            .map_err(|source| PasswordError::HashFailed {
                detail: source.to_string(),
            })?;
        Ok(hash.to_string())
    }

    /// 校验一个密码与已存的 PHC 文本。
    #[must_use]
    pub fn verify(&self, stored: &str, password: &str) -> PasswordVerdict {
        let parsed = match PasswordHash::new(stored) {
            Ok(parsed) => parsed,
            Err(source) => {
                return PasswordVerdict::Malformed {
                    detail: source.to_string(),
                };
            }
        };
        if parsed.algorithm != Algorithm::Argon2id.ident() {
            return PasswordVerdict::Malformed {
                detail: format!("只接受 Argon2id 散列，读到的算法是 {}", parsed.algorithm),
            };
        }
        match self.argon2.verify_password(password.as_bytes(), &parsed) {
            Ok(()) => PasswordVerdict::Matched,
            Err(argon2::password_hash::Error::Password) => PasswordVerdict::Mismatched,
            Err(source) => PasswordVerdict::Malformed {
                detail: source.to_string(),
            },
        }
    }

    /// 对“账号不存在”的登录做一次等代价计算，让未知账号与密码错误在时间上不可区分。
    ///
    /// 返回值没有意义，也**不**用于任何授权判断；它只为不让“查不到这个登录名”立刻返回。
    pub fn dummy_verify(&self, password: &str) {
        let mut out = [0u8; 32];
        let _ = self
            .argon2
            .hash_password_into(password.as_bytes(), &DUMMY_SALT, &mut out);
    }
}

/// 密码与已存散列的比对结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasswordVerdict {
    /// 密码正确。
    Matched,
    /// 密码不正确。
    Mismatched,
    /// 已存散列不可解析或不是 Argon2id：不是“密码错”，是数据坏了。
    Malformed {
        /// 解析失败的原因（不含散列本身以外的秘密）。
        detail: String,
    },
}

/// 散列与校验的失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasswordError {
    /// Argon2 参数不被接受。
    InvalidParams {
        /// 库给出的原因。
        detail: String,
    },
    /// 散列计算失败（随机源或内存分配问题）。
    HashFailed {
        /// 库给出的原因。
        detail: String,
    },
}

impl std::fmt::Display for PasswordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidParams { detail } => write!(f, "Argon2 参数不可用：{detail}"),
            Self::HashFailed { detail } => write!(f, "密码散列计算失败：{detail}"),
        }
    }
}

impl std::error::Error for PasswordError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn cheap_hasher() -> PasswordHasher {
        PasswordHasher::with_params(8, 1, 1).expect("测试参数")
    }

    #[test]
    fn production_parameters_are_accepted() {
        let hasher = PasswordHasher::production();
        assert_eq!(hasher.argon2.params().m_cost(), M_COST_KIB);
        assert_eq!(hasher.argon2.params().t_cost(), T_COST);
        assert_eq!(hasher.argon2.params().p_cost(), P_COST);
    }

    #[test]
    fn hash_is_argon2id_with_its_own_salt() {
        let hasher = cheap_hasher();
        let first = hasher.hash("correct horse battery staple").expect("散列");
        let second = hasher.hash("correct horse battery staple").expect("散列");
        assert!(first.starts_with("$argon2id$v=19$"), "{first}");
        assert_ne!(first, second, "相同密码必须得到不同散列（每次独立随机盐）");
        assert_eq!(
            hasher.verify(&first, "correct horse battery staple"),
            PasswordVerdict::Matched
        );
    }

    #[test]
    fn wrong_password_is_mismatched_not_malformed() {
        let hasher = cheap_hasher();
        let stored = hasher.hash("right").expect("散列");
        assert_eq!(hasher.verify(&stored, "wrong"), PasswordVerdict::Mismatched);
    }

    #[test]
    fn non_argon2id_hashes_are_rejected() {
        let hasher = cheap_hasher();
        // argon2i 的合法 PHC 文本：算法白名单之外，必须报数据坏了而不是“密码错”。
        let argon2i = "$argon2i$v=19$m=8,t=1,p=1$c29tZXNhbHQ$YWJjZGVmZ2hpamtsbW5vcA";
        match hasher.verify(argon2i, "whatever") {
            PasswordVerdict::Malformed { detail } => {
                assert!(detail.contains("Argon2id"), "{detail}");
            }
            other => panic!("非 Argon2id 散列必须拒绝：{other:?}"),
        }
        match hasher.verify("not-a-phc-string", "whatever") {
            PasswordVerdict::Malformed { .. } => {}
            other => panic!("不可解析的散列必须报 Malformed：{other:?}"),
        }
    }

    #[test]
    fn verification_uses_parameters_stored_in_the_hash_not_the_current_defaults() {
        let old = PasswordHasher::with_params(8, 1, 1).expect("旧参数");
        let stored = old.hash("password-for-old-params").expect("散列");
        let current = PasswordHasher::production();
        assert_eq!(
            current.verify(&stored, "password-for-old-params"),
            PasswordVerdict::Matched,
            "校验必须按散列自带的参数，改默认参数不需要重算旧散列"
        );
    }

    #[test]
    fn invalid_parameters_are_refused() {
        assert!(matches!(
            PasswordHasher::with_params(1, 1, 1),
            Err(PasswordError::InvalidParams { .. })
        ));
    }

    #[test]
    fn dummy_verify_does_not_pretend_to_compare() {
        // 只是“算一遍”，不返回结论；这里锁定它不会 panic、也不会因为输入而变慢到爆。
        cheap_hasher().dummy_verify("anything");
    }
}
