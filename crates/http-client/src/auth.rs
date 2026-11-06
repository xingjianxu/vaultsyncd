//! 会话凭据与刷新合并。
//!
//! 认证语义来自 design §15.2 与 S043 的服务端实现：`POST /v1/auth/login` 换一对凭据，
//! `POST /v1/auth/refresh` **原子轮换**——旧刷新凭据随即失效，同一个刷新凭据不能再用，被重放
//! 一律 401 且不回放后继凭据。客户端因此必须保证同一会话只刷新一次：并发 401 共享同一个刷新，
//! 而不是各自把同一张刷新凭据打出去（那会让除一个以外的全部请求被拒，并可能让用户被迫重新登录）。
//!
//! 本模块只做内存状态与合并逻辑：
//!
//! - 凭据的持久化由注入的 [`CredentialSink`] 负责（agent 用 OS 凭据库或受限权限文件，见
//!   `docs/adr/0001-auth-credential-carriers.md`），本 crate 不写文件、不碰平台 API。
//! - 刷新失败**不改动**本地凭据与任何队列：临时网络故障保留状态等下次重试；刷新被拒只置
//!   “需要重新登录”标记，未确认的本地内容仍然保留。
//! - 没有无限刷新：一次请求最多刷新一次，刷新后再 401 由调用方收到
//!   [`ClientError::ReauthRequired`](crate::error::ClientError::ReauthRequired)。

use core::fmt;
use std::sync::{Arc, RwLock};

use tokio::sync::Mutex;
use vaultsyncd_protocol::decimal;
use vaultsyncd_protocol::generated::SessionIssued;

use crate::error::{ClientError, ProtocolError};

/// 一份会话凭据（明文只存在于内存与注入的凭据保管处）。
///
/// `Debug` 隐去两个令牌，只保留会话身份与到期时间，避免日志把秘密写出去（design §17.3）。
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    session_id: String,
    access_token: String,
    refresh_token: String,
    access_expires_at: u64,
    refresh_expires_at: u64,
}

impl Credentials {
    /// 把登录/刷新响应里的 [`SessionIssued`] 校验成内存凭据。
    ///
    /// # Errors
    ///
    /// 会话身份不是 32 位小写十六进制、令牌为空、到期时间不是规范十进制文本或访问凭据晚于
    /// 刷新凭据时返回 [`ClientError::Protocol`]。
    pub fn from_issued(issued: &SessionIssued) -> Result<Self, ClientError> {
        if !is_canonical_session_id(&issued.session_id) {
            return Err(protocol_error("会话身份不是 32 位小写十六进制"));
        }
        if issued.access_token.is_empty() || issued.refresh_token.is_empty() {
            return Err(protocol_error("会话凭据为空"));
        }
        let access_expires_at = decimal_value("访问凭据到期时间", &issued.access_expires_at)?;
        let refresh_expires_at = decimal_value("刷新凭据到期时间", &issued.refresh_expires_at)?;
        if access_expires_at > refresh_expires_at {
            return Err(protocol_error("访问凭据到期时间晚于刷新凭据"));
        }
        Ok(Self {
            session_id: issued.session_id.clone(),
            access_token: issued.access_token.clone(),
            refresh_token: issued.refresh_token.clone(),
            access_expires_at,
            refresh_expires_at,
        })
    }

    /// 会话身份（可安全写日志）。
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 访问凭据（秘密）。
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    /// 刷新凭据（秘密）。
    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    /// 访问凭据到期时间（UTC Unix 秒）。
    pub const fn access_expires_at(&self) -> u64 {
        self.access_expires_at
    }

    /// 刷新凭据到期时间（UTC Unix 秒）。
    pub const fn refresh_expires_at(&self) -> u64 {
        self.refresh_expires_at
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("session_id", &self.session_id)
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("access_expires_at", &self.access_expires_at)
            .field("refresh_expires_at", &self.refresh_expires_at)
            .finish()
    }
}

/// 某个刷新代际的凭据快照。
///
/// `generation` 每次凭据变化（登录、刷新、注销）都加一；请求据此判断“我拿到的是不是已经被
/// 别人换掉的旧凭据”，从而合并并发刷新。
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialSnapshot {
    credentials: Credentials,
    generation: u64,
}

impl CredentialSnapshot {
    /// 凭据。
    pub const fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    /// 凭据代际。
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// 访问凭据（秘密）。
    pub fn access_token(&self) -> &str {
        self.credentials.access_token()
    }

    /// 刷新凭据（秘密）。
    pub fn refresh_token(&self) -> &str {
        self.credentials.refresh_token()
    }
}

impl fmt::Debug for CredentialSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialSnapshot")
            .field("credentials", &self.credentials)
            .field("generation", &self.generation)
            .finish()
    }
}

/// 凭据保管端口：`agent-core` 用它写 OS 凭据库或受限权限文件。
///
/// 实现必须是同步、快速且不失败的（写失败时由实现记录并重试，不在请求路径上抛错）。
pub trait CredentialSink: Send + Sync {
    /// 新的凭据已装进会话（登录或刷新成功）。
    fn credentials_stored(&self, credentials: &Credentials);

    /// 本地凭据已清除（注销成功）。
    fn credentials_erased(&self);
}

/// 会话状态与刷新合并。
///
/// 克隆共享同一份状态，因此同一个会话的所有请求看到同一个刷新代际。
#[derive(Clone, Default)]
pub struct AuthSession {
    state: Arc<RwLock<AuthState>>,
    refresh_lock: Arc<Mutex<()>>,
    sink: Option<Arc<dyn CredentialSink>>,
}

#[derive(Default)]
struct AuthState {
    credentials: Option<Credentials>,
    generation: u64,
    reauth_required: bool,
}

impl fmt::Debug for AuthSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.read();
        f.debug_struct("AuthSession")
            .field(
                "session_id",
                &state.credentials.as_ref().map(Credentials::session_id),
            )
            .field("generation", &state.generation)
            .field("reauth_required", &state.reauth_required)
            .finish()
    }
}

impl AuthSession {
    /// 新建一个没有凭据的会话。
    pub fn new() -> Self {
        Self::default()
    }

    /// 新建会话并注入凭据保管端口。
    pub fn with_sink(sink: Arc<dyn CredentialSink>) -> Self {
        Self {
            state: Arc::default(),
            refresh_lock: Arc::default(),
            sink: Some(sink),
        }
    }

    /// 装入凭据（登录成功、刷新成功，或启动时从 OS 凭据库恢复）。
    ///
    /// 返回新代际的快照，并通知保管端口。
    pub fn install(&self, credentials: Credentials) -> CredentialSnapshot {
        let snapshot = {
            let mut state = self.write();
            state.generation = state.generation.saturating_add(1);
            state.reauth_required = false;
            state.credentials = Some(credentials);
            CredentialSnapshot {
                credentials: state.credentials.clone().expect("刚刚写入的凭据必然存在"),
                generation: state.generation,
            }
        };
        if let Some(sink) = &self.sink {
            sink.credentials_stored(snapshot.credentials());
        }
        snapshot
    }

    /// 清除本地凭据（注销成功后调用）。
    ///
    /// 只改内存与通知保管端口；不删除任何同步数据。
    pub fn clear(&self) {
        {
            let mut state = self.write();
            state.generation = state.generation.saturating_add(1);
            state.credentials = None;
            state.reauth_required = false;
        }
        if let Some(sink) = &self.sink {
            sink.credentials_erased();
        }
    }

    /// 当前凭据快照；没有凭据时返回 `None`。
    pub fn snapshot(&self) -> Option<CredentialSnapshot> {
        let state = self.read();
        state
            .credentials
            .as_ref()
            .map(|credentials| CredentialSnapshot {
                credentials: credentials.clone(),
                generation: state.generation,
            })
    }

    /// 当前代际。
    pub fn generation(&self) -> u64 {
        self.read().generation
    }

    /// 是否已经判定需要用户重新登录。
    pub fn needs_reauth(&self) -> bool {
        self.read().reauth_required
    }

    /// 是否有凭据（不判断是否过期：过期由服务端判定）。
    pub fn has_credentials(&self) -> bool {
        self.read().credentials.is_some()
    }

    /// 标记需要重新登录；不改动已有凭据（保留给诊断与“重新登录后可覆盖”）。
    pub(crate) fn mark_reauth_required(&self) {
        self.write().reauth_required = true;
    }

    /// 单飞刷新：看见 `observed_generation` 的调用者共享同一次刷新。
    ///
    /// 拿到刷新锁后先比对代际：如果别人已经刷新过，直接返回新凭据，不再发请求。刷新被服务端
    /// 拒绝时返回 [`ClientError::ReauthRequired`] 并置“需要重新登录”；临时失败（超时、连接、
    /// 证书）原样返回且**不改动**任何凭据。
    pub(crate) async fn refresh<F, Fut>(
        &self,
        observed_generation: u64,
        refresher: F,
    ) -> Result<CredentialSnapshot, ClientError>
    where
        F: FnOnce(String) -> Fut,
        Fut: core::future::Future<Output = Result<SessionIssued, ClientError>>,
    {
        let _guard = self.refresh_lock.lock().await;
        if let Some(snapshot) = self.snapshot() {
            if snapshot.generation != observed_generation {
                // 等锁期间别人已经刷新：合并，不再打一次刷新请求。
                return Ok(snapshot);
            }
        }
        let Some(current) = self.snapshot() else {
            return Err(ClientError::NotAuthenticated);
        };
        let issued = refresher(current.refresh_token().to_owned()).await;
        match issued {
            Ok(issued) => {
                let credentials = Credentials::from_issued(&issued)?;
                Ok(self.install(credentials))
            }
            Err(error) => {
                if matches!(error, ClientError::ReauthRequired { .. }) {
                    self.mark_reauth_required();
                }
                Err(error)
            }
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, AuthState> {
        // 状态里只有普通数据，持锁期间不做 I/O：中毒只可能来自测试代码的 panic，
        // 这里继续使用内部值，避免把一次 panic 变成整个会话不可用。
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, AuthState> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn protocol_error(message: impl Into<String>) -> ClientError {
    ClientError::Protocol(ProtocolError::new(message))
}

fn decimal_value(label: &str, text: &str) -> Result<u64, ClientError> {
    decimal::validate(text).map_err(|error| protocol_error(format!("{label}不合规：{error}")))
}

fn is_canonical_session_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex as StdMutex;

    use crate::error::ReauthReason;

    const SESSION_ID: &str = "0123456789abcdef0123456789abcdef";

    fn issued(access: &str, refresh: &str) -> SessionIssued {
        SessionIssued {
            access_expires_at: "1000".to_owned(),
            access_token: access.to_owned(),
            refresh_expires_at: "2000".to_owned(),
            refresh_token: refresh.to_owned(),
            session_id: SESSION_ID.to_owned(),
        }
    }

    fn credentials(access: &str, refresh: &str) -> Credentials {
        Credentials::from_issued(&issued(access, refresh)).expect("合法凭据")
    }

    #[derive(Default)]
    struct RecordingSink {
        stored: StdMutex<Vec<String>>,
        erased: StdMutex<u32>,
    }

    impl CredentialSink for RecordingSink {
        fn credentials_stored(&self, credentials: &Credentials) {
            self.stored
                .lock()
                .expect("记录锁")
                .push(credentials.session_id().to_owned());
        }

        fn credentials_erased(&self) {
            *self.erased.lock().expect("记录锁") += 1;
        }
    }

    #[test]
    fn credentials_are_validated_and_redacted() {
        assert!(Credentials::from_issued(&issued("access", "refresh")).is_ok());

        let mut bad_id = issued("access", "refresh");
        bad_id.session_id = "ABCDEF".to_owned();
        assert!(matches!(
            Credentials::from_issued(&bad_id),
            Err(ClientError::Protocol(_))
        ));

        let empty_token = issued("", "refresh");
        assert!(matches!(
            Credentials::from_issued(&empty_token),
            Err(ClientError::Protocol(_))
        ));

        let mut inverted = issued("access", "refresh");
        inverted.access_expires_at = "3000".to_owned();
        assert!(matches!(
            Credentials::from_issued(&inverted),
            Err(ClientError::Protocol(_))
        ));

        let mut unbalanced = issued("access", "refresh");
        unbalanced.refresh_expires_at = "01".to_owned();
        assert!(matches!(
            Credentials::from_issued(&unbalanced),
            Err(ClientError::Protocol(_))
        ));

        let credentials = credentials("access-secret", "refresh-secret");
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
        assert!(debug.contains(SESSION_ID));

        let session = AuthSession::new();
        let snapshot = session.install(credentials);
        let debug = format!("{snapshot:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
    }

    #[test]
    fn installing_credentials_bumps_the_generation_and_notifies_the_sink() {
        let sink = Arc::new(RecordingSink::default());
        let session = AuthSession::with_sink(sink.clone());
        assert!(session.snapshot().is_none());

        let first = session.install(credentials("access-1", "refresh-1"));
        assert_eq!(first.generation(), 1);
        assert_eq!(session.generation(), 1);

        let second = session.install(credentials("access-2", "refresh-2"));
        assert_eq!(second.generation(), 2);
        assert_eq!(
            session.snapshot().expect("有凭据").access_token(),
            "access-2"
        );

        session.clear();
        assert!(session.snapshot().is_none());
        assert_eq!(session.generation(), 3);

        let stored = sink.stored.lock().expect("记录锁");
        assert_eq!(stored.len(), 2);
        assert_eq!(*sink.erased.lock().expect("记录锁"), 1);
    }

    #[tokio::test]
    async fn a_refresh_installs_the_rotated_credentials() {
        let session = AuthSession::new();
        let observed = session.install(credentials("access-1", "refresh-1"));
        let seen = Arc::new(StdMutex::new(Vec::new()));

        let refreshed = {
            let seen = seen.clone();
            session
                .refresh(observed.generation(), move |refresh_token| async move {
                    seen.lock().expect("记录锁").push(refresh_token);
                    Ok(issued("access-2", "refresh-2"))
                })
                .await
                .expect("刷新成功")
        };
        assert_eq!(refreshed.access_token(), "access-2");
        assert_eq!(refreshed.generation(), observed.generation() + 1);
        let seen = seen.lock().expect("记录锁");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], "refresh-1");
        assert!(!session.needs_reauth());
    }

    #[tokio::test]
    async fn a_stale_generation_reuses_the_already_rotated_credentials() {
        let session = AuthSession::new();
        let observed = session.install(credentials("access-1", "refresh-1"));
        session.install(credentials("access-2", "refresh-2"));

        let mut called = false;
        let snapshot = session
            .refresh(observed.generation(), |_| {
                called = true;
                async { Ok(issued("access-3", "refresh-3")) }
            })
            .await
            .expect("合并刷新");
        assert!(!called, "代际已变时不应再发刷新请求");
        assert_eq!(snapshot.access_token(), "access-2");
    }

    #[tokio::test]
    async fn a_rejected_refresh_keeps_the_credentials_and_marks_reauth() {
        let session = AuthSession::new();
        let observed = session.install(credentials("access-1", "refresh-1"));

        let error = session
            .refresh(observed.generation(), |_| async {
                Err(ClientError::ReauthRequired {
                    reason: ReauthReason::RefreshRejected,
                    cause: None,
                })
            })
            .await
            .expect_err("刷新被拒");
        assert!(matches!(
            error,
            ClientError::ReauthRequired {
                reason: ReauthReason::RefreshRejected,
                ..
            }
        ));
        assert!(session.needs_reauth());
        let kept = session.snapshot().expect("凭据不因刷新失败被丢弃");
        assert_eq!(kept.access_token(), "access-1");
    }

    #[tokio::test]
    async fn a_transient_refresh_failure_changes_nothing() {
        let session = AuthSession::new();
        let observed = session.install(credentials("access-1", "refresh-1"));

        let error = session
            .refresh(observed.generation(), |_| async {
                Err(ClientError::Timeout {
                    phase: crate::error::TimeoutPhase::Refresh,
                })
            })
            .await
            .expect_err("临时失败");
        assert!(error.is_retryable());
        assert!(!session.needs_reauth());
        assert_eq!(session.generation(), observed.generation());
        assert_eq!(
            session.snapshot().expect("凭据保留").access_token(),
            "access-1"
        );
    }

    #[tokio::test]
    async fn concurrent_refreshes_share_one_request() {
        let session = AuthSession::new();
        let observed = session.install(credentials("access-1", "refresh-1"));
        let calls = Arc::new(StdMutex::new(0_u32));

        let mut tasks = Vec::new();
        for _ in 0..5 {
            let session = session.clone();
            let calls = calls.clone();
            let generation = observed.generation();
            tasks.push(tokio::spawn(async move {
                session
                    .refresh(generation, move |_| async move {
                        *calls.lock().expect("记录锁") += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        Ok(issued("access-2", "refresh-2"))
                    })
                    .await
            }));
        }
        for task in tasks {
            assert_eq!(
                task.await
                    .expect("刷新任务")
                    .expect("刷新成功")
                    .access_token(),
                "access-2"
            );
        }
        assert_eq!(*calls.lock().expect("记录锁"), 1, "并发刷新只发一次");
    }
}
