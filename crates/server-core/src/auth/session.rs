//! 会话用例：签发、校验、刷新轮换与注销（design §17.1、§15.2、I15、T31）。
//!
//! 一次性交付两件事，且都只在这一处决定：
//!
//! 1. **凭据形态**：访问凭据与刷新凭据都是 32 字节 OS 随机数的十六进制文本；服务端只保存
//!    SHA-256 摘要（`sessions.access_digest`/`refresh_digest`），因此库被读走也拿不到可用凭据。
//! 2. **会话生命周期**：访问凭据短期（默认 15 分钟）、刷新凭据长期（默认 30 天）且**单次使用**——
//!    每次刷新都在同一个写事务里撤销旧会话并签发后继会话，旧的刷新凭据随即失效。
//!
//! 不变量：
//!
//! - **到期与撤销每次必检**：`authenticate` 与 `refresh` 都先按摘要查行，再依次判“撤销 → 到期”，
//!   任何一步不满足都返回错误，不存在“库里还有行就放行”。
//! - **刷新是轮换不是续期**：后继会话有新的身份与新的两个摘要；旧行留下 `revoked_at` 与后继身份，
//!   供重放诊断。**重放/并发刷新一律拒绝**（[`SessionError::RefreshReused`]），不做“宽限窗口”或
//!   `replaced_by` 回放——那等于用一次被盗的旧凭据换回有效凭据，需要另行决定产品语义。
//! - **注销只失效凭据**：`logout` 撤销当前会话，不清理任何本地文件（design §15.2）。
//! - **本模块不做 I/O**：存储经 [`SessionStore`] 端口、身份经 [`IdSource`]、时间由调用方给出；
//!   HTTP、cookie、CSRF 与错误码映射在 `apps/server`。

use core::fmt;

use sha2::{Digest as _, Sha256};
use vaultsyncd_domain::ids::{ID_LEN, IdSource, SessionId, UserId};
use vaultsyncd_domain::version::UtcTimestamp;

/// 凭据摘要长度（SHA-256 输出，与内容哈希同一算法）。
pub const TOKEN_DIGEST_LEN: usize = 32;

/// 一个凭据摘要。
pub type TokenDigest = [u8; TOKEN_DIGEST_LEN];

/// 凭据明文的随机字节数。
pub const TOKEN_BYTES: usize = 32;

/// 默认访问凭据有效期（秒）：15 分钟。短期凭据让“令牌被复制”的窗口有限（design §17.1）。
pub const DEFAULT_ACCESS_TTL_SECONDS: i64 = 15 * 60;
/// 默认刷新凭据有效期（秒）：30 天。
pub const DEFAULT_REFRESH_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;
/// 设备标签长度上限（字符）：登录请求里的人类可读标签，不是设备身份。
pub const MAX_DEVICE_LABEL_CHARS: usize = 128;

/// 会话有效期策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionPolicy {
    access_ttl_seconds: i64,
    refresh_ttl_seconds: i64,
}

impl SessionPolicy {
    /// 首发默认值（访问 15 分钟、刷新 30 天）。
    #[must_use]
    pub const fn standard() -> Self {
        Self {
            access_ttl_seconds: DEFAULT_ACCESS_TTL_SECONDS,
            refresh_ttl_seconds: DEFAULT_REFRESH_TTL_SECONDS,
        }
    }

    /// 显式指定有效期。
    ///
    /// # Errors
    ///
    /// 访问有效期必须为正且不长于刷新有效期；`0` 会让“刚签发就过期”。
    pub fn new(access_ttl_seconds: i64, refresh_ttl_seconds: i64) -> Result<Self, SessionError> {
        if access_ttl_seconds <= 0
            || refresh_ttl_seconds <= 0
            || access_ttl_seconds > refresh_ttl_seconds
        {
            return Err(SessionError::InvalidPolicy);
        }
        Ok(Self {
            access_ttl_seconds,
            refresh_ttl_seconds,
        })
    }

    /// 访问凭据有效期（秒）。
    #[must_use]
    pub const fn access_ttl_seconds(self) -> i64 {
        self.access_ttl_seconds
    }

    /// 刷新凭据有效期（秒）。
    #[must_use]
    pub const fn refresh_ttl_seconds(self) -> i64 {
        self.refresh_ttl_seconds
    }
}

/// 一张明文凭据。
///
/// 只在“刚签发”或“刚收到”的调用内存在：`Debug` 不打印内容，比较用摘要，不把明文交给日志。
#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(String);

impl SecretToken {
    /// 用给定的十六进制文本构造（HTTP 层解析 bearer/cookie 时用）。
    ///
    /// # Errors
    ///
    /// 文本不是恰好 [`TOKEN_BYTES`] 字节的十六进制（长度不符或含非十六进制字符）。
    pub fn parse(text: &str) -> Result<Self, SessionError> {
        if text.len() != TOKEN_BYTES * 2 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(SessionError::MalformedToken);
        }
        Ok(Self(text.to_ascii_lowercase()))
    }

    /// 凭据正文（写进响应、cookie 或发给客户端时用）。
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// 凭据摘要（只有它进数据库）。
    #[must_use]
    pub fn digest(&self) -> TokenDigest {
        let mut hasher = Sha256::new();
        hasher.update(self.0.as_bytes());
        hasher.finalize().into()
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken(<已隐去>)")
    }
}

impl fmt::Display for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<已隐去>")
    }
}

/// 已认出的调用者身份：每次资源请求都从访问凭据重新得到它（I15）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Principal {
    /// 凭据所属账号。
    pub user_id: UserId,
    /// 承载凭据的会话。
    pub session_id: SessionId,
}

/// 要写入的一行会话（端口输入，不含明文）。
#[derive(Clone, Debug)]
pub struct NewSessionRecord {
    /// 会话身份。
    pub id: SessionId,
    /// 账号。
    pub user_id: UserId,
    /// 设备标签。
    pub device_label: String,
    /// 访问凭据摘要。
    pub access_digest: TokenDigest,
    /// 刷新凭据摘要。
    pub refresh_digest: TokenDigest,
    /// 签发时间。
    pub created_at: UtcTimestamp,
    /// 访问凭据到期时间。
    pub access_expires_at: UtcTimestamp,
    /// 刷新凭据到期时间。
    pub refresh_expires_at: UtcTimestamp,
}

/// 端口读回的一行会话（只含摘要与时间）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    /// 会话身份。
    pub id: SessionId,
    /// 账号。
    pub user_id: UserId,
    /// 设备标签。
    pub device_label: String,
    /// 访问凭据摘要。
    pub access_digest: TokenDigest,
    /// 刷新凭据摘要。
    pub refresh_digest: TokenDigest,
    /// 签发时间。
    pub created_at: UtcTimestamp,
    /// 访问凭据到期时间。
    pub access_expires_at: UtcTimestamp,
    /// 刷新凭据到期时间。
    pub refresh_expires_at: UtcTimestamp,
    /// 撤销时间；`None` 表示未撤销。
    pub revoked_at: Option<UtcTimestamp>,
    /// 轮换出的后继会话。
    pub replaced_by: Option<SessionId>,
}

/// 刷新轮换的端口结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RotationOutcome {
    /// 抢占旧行并写入后继行。
    Rotated,
    /// 旧行已被撤销（并发刷新中的失败者或旧凭据重放）。
    Consumed,
    /// 旧行不存在。
    Missing,
}

/// 会话存储端口（服务端存储层 `sessions` 表的适配器实现）。
// `#[allow(async_fn_in_trait)]`：与 `auth::account::OwnerAccounts` 一样只在编排函数里以泛型使用。
#[allow(async_fn_in_trait)]
pub trait SessionStore {
    /// 写入一条会话。
    ///
    /// # Errors
    ///
    /// 存储层失败（含唯一约束冲突）。
    async fn insert(&mut self, new: NewSessionRecord) -> Result<(), SessionError>;

    /// 按访问凭据摘要读会话。
    ///
    /// # Errors
    ///
    /// 存储层失败。
    async fn find_by_access(
        &mut self,
        digest: &TokenDigest,
    ) -> Result<Option<SessionRow>, SessionError>;

    /// 按刷新凭据摘要读会话。
    ///
    /// # Errors
    ///
    /// 存储层失败。
    async fn find_by_refresh(
        &mut self,
        digest: &TokenDigest,
    ) -> Result<Option<SessionRow>, SessionError>;

    /// 在同一个写事务里撤销旧会话并写入后继会话。
    ///
    /// # Errors
    ///
    /// 存储层失败。
    async fn rotate(
        &mut self,
        current: SessionId,
        successor: NewSessionRecord,
        at: UtcTimestamp,
    ) -> Result<RotationOutcome, SessionError>;

    /// 撤销一条会话；返回是否真的改了行。
    ///
    /// # Errors
    ///
    /// 存储层失败。
    async fn revoke(&mut self, id: SessionId, at: UtcTimestamp) -> Result<bool, SessionError>;
}

/// 一次签发的凭据与有效期（明文只在此结构里）。
#[derive(Clone, Debug)]
pub struct IssuedSession {
    /// 会话身份。
    pub session_id: SessionId,
    /// 访问凭据（放 `Authorization: Bearer`，或浏览器的 HttpOnly cookie）。
    pub access: SecretToken,
    /// 刷新凭据（只在刷新端点使用）。
    pub refresh: SecretToken,
    /// 访问凭据到期时间。
    pub access_expires_at: UtcTimestamp,
    /// 刷新凭据到期时间。
    pub refresh_expires_at: UtcTimestamp,
}

/// 会话用例的失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// 凭据格式不对（长度或字符）。
    MalformedToken,
    /// 没有这个凭据：不是“过期”，而是根本不认识。
    UnknownCredential,
    /// 凭据到期。
    Expired {
        /// 到期时间。
        expired_at: UtcTimestamp,
    },
    /// 会话已撤销（注销、或刷新凭据已被用过）。
    Revoked,
    /// 刷新凭据被重复提交（并发刷新中的失败者或旧凭据重放）。
    RefreshReused {
        /// 该凭据已经被哪个后继会话取代。
        replaced_by: SessionId,
    },
    /// 设备标签不合法（空、超长或含控制字符）。
    InvalidDeviceLabel,
    /// 有效期策略不合法。
    InvalidPolicy,
    /// 存储层失败。
    Storage {
        /// 原始原因。
        detail: String,
    },
}

impl SessionError {
    /// 稳定错误码（日志与验收脚本共用；HTTP 映射在 `apps/server`）。
    ///
    /// 除 [`Self::Storage`] 外都映射 401 `AUTH_EXPIRED`（design §15.4：客户端据此刷新一次）。
    /// `storage` 是服务端内部错误，映射 500 并记日志，不能伪装成“凭据过期”让客户端白刷一次。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MalformedToken | Self::UnknownCredential => "credential_invalid",
            Self::Expired { .. } => "credential_expired",
            Self::Revoked => "session_revoked",
            Self::RefreshReused { .. } => "refresh_reused",
            Self::InvalidDeviceLabel => "invalid_device_label",
            Self::InvalidPolicy => "invalid_session_policy",
            Self::Storage { .. } => "session_storage_failed",
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedToken => f.write_str("凭据格式不正确"),
            Self::UnknownCredential => f.write_str("凭据无法识别"),
            Self::Expired { .. } => f.write_str("凭据已到期"),
            Self::Revoked => f.write_str("会话已失效"),
            Self::RefreshReused { .. } => f.write_str("刷新凭据已被使用过"),
            Self::InvalidDeviceLabel => f.write_str("设备标签不合法"),
            Self::InvalidPolicy => f.write_str("会话有效期策略不合法"),
            Self::Storage { detail } => write!(f, "会话存储失败：{detail}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// 签发一个新会话；设备标签只做形态校验，不作设备身份。
///
/// # Errors
///
/// 设备标签不合法、时间超出可表示范围或存储失败时返回错误，且不返回任何凭据。
pub async fn issue(
    store: &mut impl SessionStore,
    ids: &mut impl IdSource,
    now: UtcTimestamp,
    user_id: UserId,
    device_label: &str,
    policy: SessionPolicy,
) -> Result<IssuedSession, SessionError> {
    validate_device_label(device_label)?;
    let issued = prepare(ids, now, policy)?;
    store
        .insert(NewSessionRecord {
            id: issued.session_id,
            user_id,
            device_label: device_label.to_owned(),
            access_digest: issued.access.digest(),
            refresh_digest: issued.refresh.digest(),
            created_at: now,
            access_expires_at: issued.access_expires_at,
            refresh_expires_at: issued.refresh_expires_at,
        })
        .await?;
    Ok(issued)
}

/// 用访问凭据认出调用者：先判撤销、再判到期（撤销状态优先，免得过期提示掩盖了失效）。
///
/// # Errors
///
/// 凭据格式不对、无法识别、会话已撤销或已到期时返回错误。
pub async fn authenticate(
    store: &mut impl SessionStore,
    access: &SecretToken,
    now: UtcTimestamp,
) -> Result<Principal, SessionError> {
    let row = store
        .find_by_access(&access.digest())
        .await?
        .ok_or(SessionError::UnknownCredential)?;
    if row.revoked_at.is_some() {
        return Err(SessionError::Revoked);
    }
    if row.access_expires_at.as_unix_seconds() <= now.as_unix_seconds() {
        return Err(SessionError::Expired {
            expired_at: row.access_expires_at,
        });
    }
    Ok(Principal {
        user_id: row.user_id,
        session_id: row.id,
    })
}

/// 用刷新凭据换一对新凭据：旧会话被撤销，新会话带新的身份与两个新摘要。
///
/// # Errors
///
/// 凭据格式不对、无法识别、已撤销、已到期或已被使用过（[`SessionError::RefreshReused`]）时返回
/// 错误，且不签发任何新凭据。
pub async fn refresh(
    store: &mut impl SessionStore,
    ids: &mut impl IdSource,
    now: UtcTimestamp,
    refresh: &SecretToken,
    policy: SessionPolicy,
) -> Result<IssuedSession, SessionError> {
    let row = store
        .find_by_refresh(&refresh.digest())
        .await?
        .ok_or(SessionError::UnknownCredential)?;
    if let Some(replaced_by) = row.replaced_by {
        // 旧刷新凭据：已经被轮换过，绝不回放后继会话的凭据。
        return Err(SessionError::RefreshReused { replaced_by });
    }
    if row.revoked_at.is_some() {
        return Err(SessionError::Revoked);
    }
    if row.refresh_expires_at.as_unix_seconds() <= now.as_unix_seconds() {
        return Err(SessionError::Expired {
            expired_at: row.refresh_expires_at,
        });
    }
    let issued = prepare(ids, now, policy)?;
    let successor = NewSessionRecord {
        id: issued.session_id,
        user_id: row.user_id,
        device_label: row.device_label.clone(),
        access_digest: issued.access.digest(),
        refresh_digest: issued.refresh.digest(),
        created_at: now,
        access_expires_at: issued.access_expires_at,
        refresh_expires_at: issued.refresh_expires_at,
    };
    match store.rotate(row.id, successor, now).await? {
        RotationOutcome::Rotated => Ok(issued),
        RotationOutcome::Consumed => Err(SessionError::RefreshReused {
            replaced_by: row.replaced_by.unwrap_or(row.id),
        }),
        RotationOutcome::Missing => Err(SessionError::UnknownCredential),
    }
}

/// 注销当前会话；已失效的会话重复注销不算错误（注销是幂等的）。
///
/// # Errors
///
/// 凭据格式不对、无法识别或存储失败时返回错误。已撤销/已过期的会话返回 `Ok(())`。
pub async fn logout(
    store: &mut impl SessionStore,
    access: &SecretToken,
    now: UtcTimestamp,
) -> Result<(), SessionError> {
    let row = store
        .find_by_access(&access.digest())
        .await?
        .ok_or(SessionError::UnknownCredential)?;
    store.revoke(row.id, now).await?;
    Ok(())
}

/// 生成一套新凭据与有效期（签发与刷新共用，使两者形态完全一致）。
fn prepare(
    ids: &mut impl IdSource,
    now: UtcTimestamp,
    policy: SessionPolicy,
) -> Result<IssuedSession, SessionError> {
    let access_expires_at = add_seconds(now, policy.access_ttl_seconds)?;
    let refresh_expires_at = add_seconds(now, policy.refresh_ttl_seconds)?;
    Ok(IssuedSession {
        session_id: SessionId::generate(ids),
        access: mint_token(ids),
        refresh: mint_token(ids),
        access_expires_at,
        refresh_expires_at,
    })
}

/// 加一段时间并检查可表示性（`UtcTimestamp` 不接受负值，溢出也不得静默截断）。
fn add_seconds(now: UtcTimestamp, seconds: i64) -> Result<UtcTimestamp, SessionError> {
    let total = now
        .as_unix_seconds()
        .checked_add(seconds)
        .ok_or(SessionError::InvalidPolicy)?;
    UtcTimestamp::from_unix_seconds(total).map_err(|_| SessionError::InvalidPolicy)
}

/// 由注入的随机源取 [`TOKEN_BYTES`] 字节并写成小写十六进制文本。
///
/// `IdSource` 一次只给 [`ID_LEN`]（16）字节，所以取两次拼成 32 字节；凭据长度不因此缩短。
fn mint_token(ids: &mut impl IdSource) -> SecretToken {
    let mut text = String::with_capacity(TOKEN_BYTES * 2);
    for _ in 0..(TOKEN_BYTES / ID_LEN) {
        for byte in ids.next_id_bytes() {
            text.push_str(&format!("{byte:02x}"));
        }
    }
    SecretToken(text)
}

fn validate_device_label(label: &str) -> Result<(), SessionError> {
    if label.is_empty()
        || label.chars().count() > MAX_DEVICE_LABEL_CHARS
        || label.chars().any(char::is_control)
    {
        return Err(SessionError::InvalidDeviceLabel);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use vaultsyncd_domain::ids::ID_LEN;

    /// 递增随机源：每次调用给不同的 16 字节，测试因此可复现且不重复。
    struct SeqIds(u8);

    impl IdSource for SeqIds {
        fn next_id_bytes(&mut self) -> [u8; ID_LEN] {
            self.0 = self.0.wrapping_add(1);
            let mut bytes = [self.0; ID_LEN];
            bytes[ID_LEN - 1] = self.0 ^ 0x5a;
            bytes
        }
    }

    /// 内存版会话端口：只做与存储层相同的“抢占 + 写新行”语义，判业务留给被测代码。
    #[derive(Default)]
    struct FakeSessions {
        rows: Vec<SessionRow>,
    }

    impl FakeSessions {
        fn promote(&mut self, new: NewSessionRecord) -> SessionRow {
            SessionRow {
                id: new.id,
                user_id: new.user_id,
                device_label: new.device_label,
                access_digest: new.access_digest,
                refresh_digest: new.refresh_digest,
                created_at: new.created_at,
                access_expires_at: new.access_expires_at,
                refresh_expires_at: new.refresh_expires_at,
                revoked_at: None,
                replaced_by: None,
            }
        }
    }

    #[allow(async_fn_in_trait)]
    impl SessionStore for FakeSessions {
        async fn insert(&mut self, new: NewSessionRecord) -> Result<(), SessionError> {
            let row = self.promote(new);
            self.rows.push(row);
            Ok(())
        }

        async fn find_by_access(
            &mut self,
            digest: &TokenDigest,
        ) -> Result<Option<SessionRow>, SessionError> {
            Ok(self
                .rows
                .iter()
                .find(|row| &row.access_digest == digest)
                .cloned())
        }

        async fn find_by_refresh(
            &mut self,
            digest: &TokenDigest,
        ) -> Result<Option<SessionRow>, SessionError> {
            Ok(self
                .rows
                .iter()
                .find(|row| &row.refresh_digest == digest)
                .cloned())
        }

        async fn rotate(
            &mut self,
            current: SessionId,
            successor: NewSessionRecord,
            at: UtcTimestamp,
        ) -> Result<RotationOutcome, SessionError> {
            let Some(index) = self.rows.iter().position(|row| row.id == current) else {
                return Ok(RotationOutcome::Missing);
            };
            if self.rows[index].revoked_at.is_some() {
                return Ok(RotationOutcome::Consumed);
            }
            let successor_id = successor.id;
            self.rows[index].revoked_at = Some(at);
            self.rows[index].replaced_by = Some(successor_id);
            let row = self.promote(successor);
            self.rows.push(row);
            Ok(RotationOutcome::Rotated)
        }

        async fn revoke(&mut self, id: SessionId, at: UtcTimestamp) -> Result<bool, SessionError> {
            let Some(row) = self.rows.iter_mut().find(|row| row.id == id) else {
                return Ok(false);
            };
            if row.revoked_at.is_some() {
                return Ok(false);
            }
            row.revoked_at = Some(at);
            Ok(true)
        }
    }

    fn at(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_seconds(seconds).expect("时间戳")
    }

    fn owner() -> UserId {
        UserId::from_bytes([0x11; ID_LEN])
    }

    async fn signed_in() -> (FakeSessions, SeqIds, IssuedSession) {
        let mut store = FakeSessions::default();
        let mut ids = SeqIds(0);
        let issued = issue(
            &mut store,
            &mut ids,
            at(1_000),
            owner(),
            "Windows 11 桌面",
            SessionPolicy::standard(),
        )
        .await
        .expect("签发会话");
        (store, ids, issued)
    }

    #[tokio::test]
    async fn issued_session_authenticates_and_stores_only_digests() {
        let (mut store, _ids, issued) = signed_in().await;
        assert_eq!(issued.session_id, store.rows[0].id);
        assert_eq!(store.rows[0].access_digest, issued.access.digest());
        assert_eq!(store.rows[0].refresh_digest, issued.refresh.digest());
        assert_eq!(issued.access.expose().len(), TOKEN_BYTES * 2);
        assert_ne!(issued.access.expose(), issued.refresh.expose());
        assert!(
            !store.rows[0]
                .access_digest
                .as_slice()
                .windows(issued.access.expose().len())
                .any(|window| window == issued.access.expose().as_bytes()),
            "库里不得出现凭据明文"
        );
        assert_eq!(
            authenticate(&mut store, &issued.access, at(1_010))
                .await
                .expect("未到期凭据应可用"),
            Principal {
                user_id: owner(),
                session_id: issued.session_id,
            }
        );
        assert_eq!(issued.access_expires_at, at(1_900));
        assert_eq!(
            issued.refresh_expires_at,
            at(1_000 + DEFAULT_REFRESH_TTL_SECONDS)
        );
    }

    #[tokio::test]
    async fn expired_access_credential_is_rejected_at_its_deadline() {
        let (mut store, _ids, issued) = signed_in().await;
        // 到期时间点本身即失效（不靠 `>` 多给一秒）。
        let error = authenticate(&mut store, &issued.access, issued.access_expires_at)
            .await
            .expect_err("到期必须拒绝");
        assert_eq!(
            error,
            SessionError::Expired {
                expired_at: issued.access_expires_at
            }
        );
        assert_eq!(
            authenticate(&mut store, &issued.access, at(1_899))
                .await
                .map(|principal| principal.session_id),
            Ok(issued.session_id)
        );
    }

    #[tokio::test]
    async fn logout_revokes_the_session_and_is_idempotent() {
        let (mut store, _ids, issued) = signed_in().await;
        logout(&mut store, &issued.access, at(1_100))
            .await
            .expect("注销");
        assert_eq!(
            authenticate(&mut store, &issued.access, at(1_101))
                .await
                .expect_err("注销后必须拒绝"),
            SessionError::Revoked
        );
        // 重复注销不报错（客户端重试或并发注销都不该变成 401 风暴）。
        logout(&mut store, &issued.access, at(1_102))
            .await
            .expect("重复注销");
        assert_eq!(store.rows[0].revoked_at, Some(at(1_100)));
    }

    #[tokio::test]
    async fn refresh_rotates_tokens_and_old_refresh_credential_is_refused() {
        let (mut store, mut ids, issued) = signed_in().await;
        let renewed = refresh(
            &mut store,
            &mut ids,
            at(1_200),
            &issued.refresh,
            SessionPolicy::standard(),
        )
        .await
        .expect("刷新");
        assert_ne!(renewed.session_id, issued.session_id);
        assert_ne!(renewed.access.expose(), issued.access.expose());
        assert_ne!(renewed.refresh.expose(), issued.refresh.expose());
        // 旧访问凭据随旧会话一起失效。
        assert_eq!(
            authenticate(&mut store, &issued.access, at(1_201))
                .await
                .expect_err("旧访问凭据必须失效"),
            SessionError::Revoked
        );
        // 旧刷新凭据被再次提交（重放或并发失败者）：拒绝，且不回放后继凭据。
        let reused = refresh(
            &mut store,
            &mut ids,
            at(1_202),
            &issued.refresh,
            SessionPolicy::standard(),
        )
        .await
        .expect_err("旧刷新凭据必须被拒绝");
        assert_eq!(
            reused,
            SessionError::RefreshReused {
                replaced_by: renewed.session_id
            }
        );
        assert_eq!(reused.code(), "refresh_reused");
        // 新凭据可用。
        assert_eq!(
            authenticate(&mut store, &renewed.access, at(1_203))
                .await
                .map(|principal| principal.session_id),
            Ok(renewed.session_id)
        );
    }

    #[tokio::test]
    async fn expired_refresh_credential_and_unknown_tokens_are_refused() {
        let (mut store, mut ids, issued) = signed_in().await;
        let error = refresh(
            &mut store,
            &mut ids,
            issued.refresh_expires_at,
            &issued.refresh,
            SessionPolicy::standard(),
        )
        .await
        .expect_err("刷新凭据到期必须拒绝");
        assert_eq!(
            error,
            SessionError::Expired {
                expired_at: issued.refresh_expires_at
            }
        );
        let unknown = SecretToken::parse(&"ab".repeat(TOKEN_BYTES)).expect("形态合法但未知");
        assert_eq!(
            authenticate(&mut store, &unknown, at(1_000))
                .await
                .expect_err("未知凭据"),
            SessionError::UnknownCredential
        );
        assert_eq!(
            SecretToken::parse("short"),
            Err(SessionError::MalformedToken)
        );
        assert!(SecretToken::parse(&"zz".repeat(TOKEN_BYTES)).is_err());
        assert!(!format!("{issued:?}").contains(issued.access.expose()));
    }

    #[tokio::test]
    async fn device_label_and_policy_are_validated_before_issuing() {
        let mut store = FakeSessions::default();
        let mut ids = SeqIds(0);
        for label in ["", "\n", &"测".repeat(MAX_DEVICE_LABEL_CHARS + 1)] {
            assert_eq!(
                issue(
                    &mut store,
                    &mut ids,
                    at(1_000),
                    owner(),
                    label,
                    SessionPolicy::standard(),
                )
                .await
                .expect_err("不合法设备标签"),
                SessionError::InvalidDeviceLabel
            );
        }
        assert!(store.rows.is_empty(), "被拒绝的签发不得留下会话");
        assert_eq!(
            SessionPolicy::new(0, 1).expect_err("零访问有效期"),
            SessionError::InvalidPolicy
        );
        assert_eq!(
            SessionPolicy::new(600, 300).expect_err("访问长于刷新"),
            SessionError::InvalidPolicy
        );
        assert_eq!(
            SessionPolicy::new(60, 60)
                .expect("相等也可以")
                .access_ttl_seconds(),
            60
        );
    }
}
