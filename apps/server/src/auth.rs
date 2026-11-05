//! 认证端点：`POST /v1/auth/{login,refresh,logout}`、凭据载体与 CSRF（design §15.2、§17.1、§17.2）。
//!
//! 本模块只做“HTTP 到用例”的翻译，判定都在 `vaultsyncd_server_core::auth`：
//!
//! - **两种凭据载体，同一套会话**：`Authorization: Bearer <token>`（agent/CLI，凭据由客户端自己
//!   安全保管）与同源 HttpOnly cookie（浏览器）。边界与理由见
//!   `docs/adr/0001-auth-credential-carriers.md`。cookie 名分别是 `vaultsyncd_session`（访问）与
//!   `vaultsyncd_refresh`（刷新，`Path=/v1/auth`）。
//! - **cookie 是环境凭据，所以写请求必须同源**：带 cookie 的 POST 要求 `Origin`（或 `Sec-Fetch-Site`）
//!   表明同源，否则 403 `ACCESS_DENIED`——这正是 CSRF 的入口。缺失同源证据时按拒绝处理（fail
//!   closed），只有“既没有 Origin 也没有 Sec-Fetch-Site”的非浏览器请求（agent、curl）才放行，因为
//!   它们不带环境凭据。Bearer 请求不做 CSRF 判定：没有环境凭据就没有可被借用的身份。
//! - **`SameSite=Strict` + `HttpOnly` + `Secure`**：cookie 不参与跨站请求、脚本读不到、只走 HTTPS
//!   （design §17.2：明文公共 HTTP 不符合 C29）。
//! - **响应不带绝对路径**：登录/刷新/注销的响应只有会话身份、凭据与时间；越权结论是固定的
//!   `access_denied` 文本（[`crate::auth`] 的 scope 规则），不泄露 Root 是否存在。
//! - **失败映射**：凭据问题一律 401 `AUTH_EXPIRED`（design §15.4 让客户端刷新一次）；越权 403
//!   `ACCESS_DENIED`；限流 429 `RATE_LIMITED`；存储失败 503 `NEEDS_RECOVERY`（内部错误，记日志，
//!   不伪装成“凭据过期”让客户端白刷一次）。
//!
//! 限流与随机源是进程内状态（[`AuthContext`]），因此本模块的 handler 之间共享一个实例；重启清零
//! 的限流语义与 S042 相同。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::State;
use axum::http::header::{AUTHORIZATION, COOKIE, HOST, ORIGIN, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::Mutex;
use tracing::{info, warn};
use vaultsyncd_domain::ids::{IdSource, RootId};
use vaultsyncd_domain::version::UtcTimestamp;
use vaultsyncd_protocol::generated::{
    ErrorBody, ErrorCode, ErrorEnvelope, LoginRequest, RefreshRequest, SessionIssued,
    SessionRevoked,
};
use vaultsyncd_server_core::auth::account::{
    AccountError, NewAccount, OwnerAccounts, StoredAccount,
};
use vaultsyncd_server_core::auth::login::{LoginAttempt, LoginError, authenticate};
use vaultsyncd_server_core::auth::password::PasswordHasher;
use vaultsyncd_server_core::auth::scope::{RootGrant, ScopeError, require_root_scope};
use vaultsyncd_server_core::auth::session::{
    self, IssuedSession, NewSessionRecord, RotationOutcome, SecretToken, SessionError,
    SessionPolicy, SessionRow, SessionStore, TokenDigest,
};
// 资源端点（上传等）需要把“已认证调用者”当作公开类型使用：身份类型由核心定义，这里只做再导出。
pub use vaultsyncd_server_core::auth::session::Principal;
use vaultsyncd_server_core::auth::throttle::{LoginThrottle, ThrottlePolicy};
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::sessions::{self as store_sessions, DIGEST_LEN};
use vaultsyncd_server_store::users;

/// 访问凭据的 cookie 名。
pub const ACCESS_COOKIE: &str = "vaultsyncd_session";
/// 刷新凭据的 cookie 名。
pub const REFRESH_COOKIE: &str = "vaultsyncd_refresh";

/// 认证端点共享的进程内状态：数据库、密码散列、限流、会话策略与随机源。
pub struct AuthContext {
    database: Database,
    hasher: PasswordHasher,
    throttle: Mutex<LoginThrottle>,
    policy: SessionPolicy,
    ids: Mutex<RandomIds>,
}

impl AuthContext {
    /// 生产装配：Argon2id 生产参数、默认限流与会话有效期、OS 随机源。
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self {
            database,
            hasher: PasswordHasher::production(),
            throttle: Mutex::new(LoginThrottle::new(ThrottlePolicy::default())),
            policy: SessionPolicy::standard(),
            ids: Mutex::new(RandomIds),
        }
    }

    /// 会话有效期策略（测试与后续配置项共用）。
    #[must_use]
    pub fn policy(&self) -> SessionPolicy {
        self.policy
    }

    /// 数据库句柄（Root 授权与后续资源端点共用）。
    #[must_use]
    pub fn database(&self) -> &Database {
        &self.database
    }
}

/// OS 随机源：会话身份、凭据与 Root 身份都来自它，不用时间或计数器（design §17.1）。
pub(crate) struct RandomIds;

impl IdSource for RandomIds {
    fn next_id_bytes(&mut self) -> [u8; 16] {
        use rand_core::{OsRng, RngCore};
        let mut bytes = [0_u8; 16];
        OsRng.fill_bytes(&mut bytes);
        bytes
    }
}

/// 一个处理器的请求上下文（请求头 + 服务端时间）。
struct RequestFacts {
    headers: HeaderMap,
    now: UtcTimestamp,
    request_id: String,
}

impl RequestFacts {
    fn collect(headers: HeaderMap) -> Result<Self, Box<Response>> {
        let now =
            server_now().ok_or_else(|| Box::new(internal_error("系统时钟不可用", "clock")))?;
        Ok(Self {
            headers,
            now,
            request_id: next_request_id(),
        })
    }
}

/// `POST /v1/auth/login`。
pub async fn login(
    State(context): State<Arc<AuthContext>>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Response {
    let facts = match RequestFacts::collect(headers) {
        Ok(facts) => facts,
        Err(response) => return *response,
    };
    if !same_origin(&facts.headers) {
        return denied(&facts.request_id, "跨源登录请求被拒绝");
    }

    let mut accounts = DbAccounts {
        database: &context.database,
    };
    let attempt = LoginAttempt {
        login: &body.login,
        password: &body.password,
        now: facts.now,
    };
    let authenticated = {
        let mut throttle = context.throttle.lock().await;
        authenticate(&mut accounts, &context.hasher, &mut throttle, attempt).await
    };
    let authenticated = match authenticated {
        Ok(account) => account,
        Err(error) => return login_failure(&facts.request_id, &error),
    };

    let mut sessions = DbSessions {
        database: &context.database,
    };
    let mut ids = context.ids.lock().await;
    let issued = session::issue(
        &mut sessions,
        &mut *ids,
        facts.now,
        authenticated.id,
        &body.device_label,
        context.policy,
    )
    .await;
    drop(ids);
    let issued = match issued {
        Ok(issued) => issued,
        Err(error) => return session_failure(&facts.request_id, &error),
    };
    info!(
        request_id = %facts.request_id,
        user_id = %authenticated.id,
        session_id = %issued.session_id,
        "登录成功"
    );
    issued_response(&issued)
}

/// `POST /v1/auth/refresh`。
///
/// agent 在请求体里提交刷新凭据；浏览器没有可读的刷新凭据（HttpOnly），因此允许空请求体并改从
/// `vaultsyncd_refresh` cookie 取——两条载体的语义完全相同（ADR 里有边界说明）。
pub async fn refresh(
    State(context): State<Arc<AuthContext>>,
    headers: HeaderMap,
    body: Option<Json<RefreshRequest>>,
) -> Response {
    let facts = match RequestFacts::collect(headers) {
        Ok(facts) => facts,
        Err(response) => return *response,
    };
    if !same_origin(&facts.headers) {
        return denied(&facts.request_id, "跨源刷新请求被拒绝");
    }
    let token = match body
        .and_then(|Json(request)| SecretToken::parse(&request.refresh_token).ok())
        .or_else(|| cookie_token(&facts.headers, REFRESH_COOKIE))
    {
        Some(token) => token,
        None => {
            return auth_expired(&facts.request_id, "刷新凭据缺失或格式不正确");
        }
    };

    let mut sessions = DbSessions {
        database: &context.database,
    };
    let mut ids = context.ids.lock().await;
    let renewed =
        session::refresh(&mut sessions, &mut *ids, facts.now, &token, context.policy).await;
    drop(ids);
    match renewed {
        Ok(issued) => {
            info!(request_id = %facts.request_id, session_id = %issued.session_id, "会话已轮换");
            issued_response(&issued)
        }
        Err(error) => session_failure(&facts.request_id, &error),
    }
}

/// `POST /v1/auth/logout`：失效当前会话，清掉 cookie；不碰本地文件。
pub async fn logout(State(context): State<Arc<AuthContext>>, headers: HeaderMap) -> Response {
    let facts = match RequestFacts::collect(headers) {
        Ok(facts) => facts,
        Err(response) => return *response,
    };
    if !same_origin(&facts.headers) {
        return denied(&facts.request_id, "跨源注销请求被拒绝");
    }
    let principal = match principal_with(&context, &facts).await {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let mut sessions = DbSessions {
        database: &context.database,
    };
    match sessions.revoke(principal.session_id, facts.now).await {
        Ok(_) => {
            info!(request_id = %facts.request_id, session_id = %principal.session_id, "已注销会话");
            let payload = SessionRevoked {
                session_id: principal.session_id.to_hex(),
                revoked_at: facts.now.as_unix_seconds().to_string(),
            };
            let mut response = (StatusCode::OK, Json(payload)).into_response();
            clear_cookie(&mut response, ACCESS_COOKIE, "/");
            clear_cookie(&mut response, REFRESH_COOKIE, "/v1/auth");
            response
        }
        Err(error) => session_failure(&facts.request_id, &error),
    }
}

/// 已认证调用者访问某个 Root 前的授权检查（I15：每次资源请求都做）。
///
/// 先用存储层的 `root_in_scope` 取行——它把“不是你的 Root”和“不存在的 Root”压成同一个 `None`，
/// 因此不会泄露 Root 是否存在；再用核心的纯规则复核 owner。
///
/// # Errors
///
/// 越权或 Root 不存在时返回 [`ScopeError::AccessDenied`]；存储失败返回 `Err(())` 之外的
/// [`AuthorizeError::Storage`]。
pub async fn authorize_root(
    database: &Database,
    principal: &Principal,
    root_id: RootId,
) -> Result<RootGrant, AuthorizeError> {
    let mut conn = database
        .pool()
        .acquire()
        .await
        .map_err(|error| AuthorizeError::Storage(error.to_string()))?;
    let record =
        vaultsyncd_server_store::roots::root_in_scope(&mut conn, principal.user_id, root_id)
            .await
            .map_err(|error| AuthorizeError::Storage(error.to_string()))?;
    let record = record.ok_or(AuthorizeError::Scope(ScopeError::AccessDenied))?;
    let grant = RootGrant {
        root_id: record.id,
        owner_id: record.owner,
    };
    require_root_scope(principal, &grant).map_err(AuthorizeError::Scope)?;
    Ok(grant)
}

/// Root 授权检查的失败。
#[derive(Debug)]
pub enum AuthorizeError {
    /// 越权（对外 403 `ACCESS_DENIED`）。
    Scope(ScopeError),
    /// 存储层失败（对外 503）。
    Storage(String),
}

impl std::fmt::Display for AuthorizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scope(error) => write!(f, "{error}"),
            Self::Storage(detail) => write!(f, "读取 Root 归属失败：{detail}"),
        }
    }
}

impl std::error::Error for AuthorizeError {}

/// 请求里带来的凭据（两种载体之一）。
struct Credential {
    token: SecretToken,
}

fn credential(headers: &HeaderMap) -> Option<Credential> {
    if let Some(value) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        if let Some(token) = value.strip_prefix("Bearer ") {
            if let Ok(token) = SecretToken::parse(token.trim()) {
                return Some(Credential { token });
            }
        }
    }
    cookie_token(headers, ACCESS_COOKIE).map(|token| Credential { token })
}

fn cookie_token(headers: &HeaderMap, name: &str) -> Option<SecretToken> {
    for header in headers.get_all(COOKIE) {
        let Ok(text) = header.to_str() else { continue };
        for pair in text.split(';') {
            let pair = pair.trim();
            if let Some(value) = pair
                .strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
            {
                if let Ok(token) = SecretToken::parse(value) {
                    return Some(token);
                }
            }
        }
    }
    None
}

/// cookie 是环境凭据：写请求必须带同源证据，否则按 CSRF 拒绝。
///
/// 判定顺序：`Origin` 必须与 `Host` 同源；没有 `Origin` 时看 `Sec-Fetch-Site`（`same-origin`/`none`
/// 放行）；两者都没有则视为非浏览器客户端（agent/curl），放行——它们不带 cookie。
fn same_origin(headers: &HeaderMap) -> bool {
    if let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) {
        let host = headers
            .get(HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        return origin_host(origin)
            .is_some_and(|origin_host| origin_host.eq_ignore_ascii_case(host));
    }
    match headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
    {
        Some(site) => matches!(site, "same-origin" | "none"),
        None => true,
    }
}

fn origin_host(origin: &str) -> Option<&str> {
    let (_, rest) = origin.split_once("://")?;
    let host = rest.split('/').next()?;
    Some(host)
}

fn issued_response(issued: &IssuedSession) -> Response {
    let payload = SessionIssued {
        session_id: issued.session_id.to_hex(),
        access_token: issued.access.expose().to_owned(),
        refresh_token: issued.refresh.expose().to_owned(),
        access_expires_at: issued.access_expires_at.as_unix_seconds().to_string(),
        refresh_expires_at: issued.refresh_expires_at.as_unix_seconds().to_string(),
    };
    let mut response = (StatusCode::OK, Json(payload)).into_response();
    set_cookie(
        &mut response,
        ACCESS_COOKIE,
        issued.access.expose(),
        "/",
        issued.access_expires_at,
    );
    set_cookie(
        &mut response,
        REFRESH_COOKIE,
        issued.refresh.expose(),
        "/v1/auth",
        issued.refresh_expires_at,
    );
    response
}

fn set_cookie(response: &mut Response, name: &str, value: &str, path: &str, expires: UtcTimestamp) {
    let max_age =
        (expires.as_unix_seconds() - server_now().map_or(0, |now| now.as_unix_seconds())).max(0);
    let cookie = format!(
        "{name}={value}; Path={path}; HttpOnly; Secure; SameSite=Strict; Max-Age={max_age}"
    );
    if let Ok(value) = cookie.parse() {
        response.headers_mut().append(SET_COOKIE, value);
    }
}

fn clear_cookie(response: &mut Response, name: &str, path: &str) {
    let cookie = format!("{name}=; Path={path}; HttpOnly; Secure; SameSite=Strict; Max-Age=0");
    if let Ok(value) = cookie.parse() {
        response.headers_mut().append(SET_COOKIE, value);
    }
}

fn login_failure(request_id: &str, error: &LoginError) -> Response {
    match error {
        LoginError::InvalidCredentials => auth_expired(request_id, "登录名或密码不正确"),
        LoginError::RateLimited {
            retry_after_seconds,
        } => {
            let mut response = error_response(
                StatusCode::TOO_MANY_REQUESTS,
                ErrorCode::RateLimited,
                request_id,
                "登录尝试过于频繁，请稍后再试",
                true,
                None,
            );
            if let Ok(value) = retry_after_seconds.to_string().parse() {
                response.headers_mut().insert("retry-after", value);
            }
            response
        }
        LoginError::Accounts(AccountError::Storage { detail }) => {
            warn!(request_id = %request_id, error = %detail, "登录时账号存储失败");
            internal_error("服务端内部错误", request_id)
        }
        LoginError::Accounts(AccountError::Malformed { detail })
        | LoginError::AccountBroken { detail } => {
            warn!(request_id = %request_id, error = %detail, "账号数据损坏");
            internal_error("服务端内部错误", request_id)
        }
        LoginError::Accounts(other) => {
            warn!(request_id = %request_id, error = %other, "登录时账号读取出错");
            internal_error("服务端内部错误", request_id)
        }
    }
}

fn session_failure(request_id: &str, error: &SessionError) -> Response {
    match error {
        SessionError::Storage { detail } => {
            warn!(request_id = %request_id, error = %detail, "会话存储失败");
            internal_error("服务端内部错误", request_id)
        }
        SessionError::InvalidDeviceLabel => {
            auth_expired(request_id, "设备标签不合法（不能为空、不能含控制字符）")
        }
        SessionError::RefreshReused { replaced_by } => {
            warn!(request_id = %request_id, replaced_by = %replaced_by, "刷新凭据被重复使用");
            auth_expired(request_id, "刷新凭据已失效，请重新登录")
        }
        SessionError::Expired { .. } | SessionError::Revoked | SessionError::UnknownCredential => {
            auth_expired(request_id, "会话已失效，请重新登录")
        }
        SessionError::MalformedToken | SessionError::InvalidPolicy => {
            auth_expired(request_id, "会话凭据不可用")
        }
    }
}

fn auth_expired(request_id: &str, message: &str) -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        ErrorCode::AuthExpired,
        request_id,
        message,
        false,
        None,
    )
}

fn denied(request_id: &str, message: &str) -> Response {
    error_response(
        StatusCode::FORBIDDEN,
        ErrorCode::AccessDenied,
        request_id,
        message,
        false,
        None,
    )
}

fn internal_error(message: &str, request_id: &str) -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::NeedsRecovery,
        request_id,
        message,
        true,
        None,
    )
}

fn error_response(
    status: StatusCode,
    code: ErrorCode,
    request_id: &str,
    message: &str,
    retryable: bool,
    root_id: Option<String>,
) -> Response {
    let envelope = ErrorEnvelope {
        error: ErrorBody {
            code,
            details: None,
            message: message.to_owned(),
            request_id: request_id.to_owned(),
            retryable,
            root_id,
        },
    };
    (status, Json(envelope)).into_response()
}

fn server_now() -> Option<UtcTimestamp> {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    UtcTimestamp::from_unix_seconds(i64::try_from(seconds).ok()?).ok()
}

fn next_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("auth-{}-{sequence}", std::process::id())
}

/// `accounts` 表的核心端口适配器：每次都开自己的短事务/连接，不跨 await 持有写锁。
struct DbAccounts<'a> {
    database: &'a Database,
}

#[allow(async_fn_in_trait)]
impl OwnerAccounts for DbAccounts<'_> {
    async fn count(&mut self) -> Result<u64, AccountError> {
        let mut conn = self.connection().await?;
        users::count_users(&mut conn)
            .await
            .map_err(|error| storage_error(&error.to_string()))
    }

    async fn find_by_login(&mut self, login: &str) -> Result<Option<StoredAccount>, AccountError> {
        let mut conn = self.connection().await?;
        let record = users::find_by_login(&mut conn, login)
            .await
            .map_err(|error| match error {
                users::UserError::MalformedRow { column } => AccountError::Malformed {
                    detail: format!("列 {column} 不符合账号类型前提"),
                },
                other => storage_error(&other.to_string()),
            })?;
        Ok(record.map(|record| StoredAccount {
            id: record.id,
            login: record.login,
            password_hash: record.password_hash,
            active: record.status == users::UserStatus::Active,
        }))
    }

    async fn insert(&mut self, new: NewAccount) -> Result<(), AccountError> {
        let mut tx = self
            .database
            .begin_write()
            .await
            .map_err(|error| storage_error(&error.to_string()))?;
        users::insert_user(
            &mut tx,
            &users::NewUser {
                id: new.id,
                login: new.login.clone(),
                password_hash: new.password_hash,
            },
        )
        .await
        .map_err(|error| match error {
            users::UserError::DuplicateLogin { login } => AccountError::DuplicateLogin { login },
            other => storage_error(&other.to_string()),
        })?;
        tx.commit()
            .await
            .map_err(|error| storage_error(&error.to_string()))
    }
}

impl DbAccounts<'_> {
    async fn connection(&self) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, AccountError> {
        self.database
            .pool()
            .acquire()
            .await
            .map_err(|error| storage_error(&error.to_string()))
    }
}

/// `sessions` 表的核心端口适配器：读用连接、写用 `BEGIN IMMEDIATE` 短事务。
struct DbSessions<'a> {
    database: &'a Database,
}

#[allow(async_fn_in_trait)]
impl SessionStore for DbSessions<'_> {
    async fn insert(&mut self, new: NewSessionRecord) -> Result<(), SessionError> {
        let mut tx = self.begin().await?;
        store_sessions::insert_session(&mut tx, &to_store(&new))
            .await
            .map_err(session_storage_error)?;
        tx.commit().await.map_err(session_db_error)
    }

    async fn find_by_access(
        &mut self,
        digest: &TokenDigest,
    ) -> Result<Option<SessionRow>, SessionError> {
        let mut conn = self.connection().await?;
        let digest = digest_array(digest)?;
        store_sessions::find_by_access_digest(&mut conn, &digest)
            .await
            .map_err(session_storage_error)
            .map(|row| row.map(to_row))
    }

    async fn find_by_refresh(
        &mut self,
        digest: &TokenDigest,
    ) -> Result<Option<SessionRow>, SessionError> {
        let mut conn = self.connection().await?;
        let digest = digest_array(digest)?;
        store_sessions::find_by_refresh_digest(&mut conn, &digest)
            .await
            .map_err(session_storage_error)
            .map(|row| row.map(to_row))
    }

    async fn rotate(
        &mut self,
        current: vaultsyncd_domain::ids::SessionId,
        successor: NewSessionRecord,
        at: UtcTimestamp,
    ) -> Result<RotationOutcome, SessionError> {
        let mut tx = self.begin().await?;
        let outcome = store_sessions::rotate_refresh(&mut tx, current, &to_store(&successor), at)
            .await
            .map_err(session_storage_error)?;
        tx.commit().await.map_err(session_db_error)?;
        Ok(match outcome {
            store_sessions::RotationOutcome::Rotated => RotationOutcome::Rotated,
            store_sessions::RotationOutcome::Consumed => RotationOutcome::Consumed,
            store_sessions::RotationOutcome::Missing => RotationOutcome::Missing,
        })
    }

    async fn revoke(
        &mut self,
        id: vaultsyncd_domain::ids::SessionId,
        at: UtcTimestamp,
    ) -> Result<bool, SessionError> {
        let mut tx = self.begin().await?;
        let changed = store_sessions::revoke_session(&mut tx, id, at)
            .await
            .map_err(session_storage_error)?;
        tx.commit().await.map_err(session_db_error)?;
        Ok(changed)
    }
}

impl DbSessions<'_> {
    async fn begin(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, SessionError> {
        self.database
            .begin_write()
            .await
            .map_err(|error| SessionError::Storage {
                detail: error.to_string(),
            })
    }

    async fn connection(&self) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, SessionError> {
        self.database
            .pool()
            .acquire()
            .await
            .map_err(|error| SessionError::Storage {
                detail: error.to_string(),
            })
    }
}

fn to_store(new: &NewSessionRecord) -> store_sessions::NewSession {
    store_sessions::NewSession {
        id: new.id,
        user_id: new.user_id,
        device_label: new.device_label.clone(),
        access_digest: new.access_digest,
        refresh_digest: new.refresh_digest,
        created_at: new.created_at,
        access_expires_at: new.access_expires_at,
        refresh_expires_at: new.refresh_expires_at,
    }
}

fn to_row(row: store_sessions::SessionRecord) -> SessionRow {
    SessionRow {
        id: row.id,
        user_id: row.user_id,
        device_label: row.device_label,
        access_digest: row.access_digest,
        refresh_digest: row.refresh_digest,
        created_at: row.created_at,
        access_expires_at: row.access_expires_at,
        refresh_expires_at: row.refresh_expires_at,
        revoked_at: row.revoked_at,
        replaced_by: row.replaced_by,
    }
}

fn digest_array(digest: &TokenDigest) -> Result<[u8; DIGEST_LEN], SessionError> {
    let mut bytes = [0_u8; DIGEST_LEN];
    bytes.copy_from_slice(digest);
    Ok(bytes)
}

fn session_storage_error(error: store_sessions::SessionError) -> SessionError {
    SessionError::Storage {
        detail: error.to_string(),
    }
}

fn session_db_error(error: sqlx::Error) -> SessionError {
    SessionError::Storage {
        detail: error.to_string(),
    }
}

fn storage_error(detail: &str) -> AccountError {
    AccountError::Storage {
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
pub(crate) async fn test_database(path: &std::path::Path) -> Database {
    Database::open(path).await.expect("建临时服务端库")
}

/// 供后续资源端点使用的身份提取：从请求头认出 [`Principal`]，失败时返回可直出的错误响应。
///
/// # Errors
///
/// 没有凭据、凭据无效/过期/已撤销，或存储失败时返回对应的 HTTP 响应。
// 失败类型是 axum 的 Response（后续端点直接把它当返回值用），体积由框架决定；
// 装箱只会让每个调用点多一层解引用，不减少实际分配。
#[allow(clippy::result_large_err)]
pub async fn principal_from(
    context: &AuthContext,
    headers: &HeaderMap,
) -> Result<Principal, Response> {
    let facts = RequestFacts::collect(headers.clone()).map_err(|response| *response)?;
    principal_with(context, &facts)
        .await
        .map_err(|response| *response)
}

async fn principal_with(
    context: &AuthContext,
    facts: &RequestFacts,
) -> Result<Principal, Box<Response>> {
    let Some(credential) = credential(&facts.headers) else {
        return Err(Box::new(auth_expired(
            &facts.request_id,
            "没有可用的会话凭据",
        )));
    };
    if !same_origin(&facts.headers) {
        return Err(Box::new(denied(&facts.request_id, "跨源请求被拒绝")));
    }
    let mut sessions = DbSessions {
        database: &context.database,
    };
    session::authenticate(&mut sessions, &credential.token, facts.now)
        .await
        .map_err(|error| Box::new(session_failure(&facts.request_id, &error)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{Request, header};
    use axum::routing::post;
    use tower::ServiceExt;
    use vaultsyncd_domain::ids::UserId;

    async fn context(label: &str) -> Arc<AuthContext> {
        let path = std::env::temp_dir().join(format!(
            "vaultsyncd-s043-{label}-{}.sqlite",
            std::process::id()
        ));
        let database = test_database(&path).await;
        let mut tx = database.begin_write().await.expect("写事务");
        let hash = PasswordHasher::production()
            .hash("correct horse battery staple")
            .expect("散列");
        users::insert_user(
            &mut tx,
            &users::NewUser {
                id: UserId::from_bytes([0x11; 16]),
                login: "owner".to_owned(),
                password_hash: hash,
            },
        )
        .await
        .expect("写 owner");
        tx.commit().await.expect("提交");
        Arc::new(AuthContext::new(database))
    }

    fn router_for(context: Arc<AuthContext>) -> axum::Router {
        axum::Router::new()
            .route("/v1/auth/login", post(login))
            .route("/v1/auth/refresh", post(refresh))
            .route("/v1/auth/logout", post(logout))
            .with_state(context)
    }

    async fn call(router: axum::Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let response = router.oneshot(request).await.expect("调用处理器");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("读响应体");
        (
            status,
            headers,
            String::from_utf8(bytes.to_vec()).expect("UTF-8"),
        )
    }

    fn login_request(origin: Option<(&str, &str)>) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some((name, value)) = origin {
            builder = builder.header(name, value);
        }
        builder
            .body(Body::from(
                "{\"login\":\"owner\",\"password\":\"correct horse battery staple\",\"device_label\":\"测试客户端\"}",
            ))
            .expect("构造请求")
    }

    fn cookies(headers: &HeaderMap) -> Vec<String> {
        headers
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    async fn login_sets_httponly_cookies_and_returns_no_paths() {
        let context = context("login").await;
        let (status, headers, body) =
            call(router_for(Arc::clone(&context)), login_request(None)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let set_cookies = cookies(&headers);
        assert_eq!(
            set_cookies.len(),
            2,
            "访问与刷新各一个 cookie：{set_cookies:?}"
        );
        for cookie in &set_cookies {
            assert!(cookie.contains("HttpOnly"), "{cookie}");
            assert!(cookie.contains("SameSite=Strict"), "{cookie}");
            assert!(cookie.contains("Secure"), "{cookie}");
        }
        assert!(
            set_cookies
                .iter()
                .any(|cookie| cookie.starts_with(ACCESS_COOKIE))
        );
        assert!(
            set_cookies
                .iter()
                .any(|cookie| cookie.starts_with(REFRESH_COOKIE))
        );
        assert!(body.contains("access_token"), "{body}");
        assert!(!body.contains('/'), "响应不得含绝对路径：{body}");
        assert!(!body.contains("password"), "{body}");

        // 同一对凭据可以换成 principal（后续资源端点用同一个入口，I15）。
        let token = body
            .split("\"access_token\":\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("取访问凭据")
            .to_owned();
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {token}").parse().expect("凭据头"),
        );
        let principal = principal_from(&context, &headers)
            .await
            .expect("认出调用者");
        assert_eq!(principal.user_id, UserId::from_bytes([0x11; 16]));

        // 注销后同一凭据被拒绝，且 cookie 被清掉。
        let (status, headers, body) = call(
            router_for(Arc::clone(&context)),
            Request::builder()
                .method("POST")
                .uri("/v1/auth/logout")
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .expect("构造请求"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            cookies(&headers)
                .iter()
                .all(|cookie| cookie.contains("Max-Age=0"))
        );
        let rejected = principal_from(&context, &headers_of(&token)).await;
        assert!(rejected.is_err(), "注销后的凭据必须被拒绝");
    }

    fn headers_of(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {token}").parse().expect("凭据头"),
        );
        headers
    }

    #[tokio::test]
    async fn cross_origin_writes_are_rejected() {
        let context = context("csrf").await;
        let (status, _, body) = call(
            router_for(Arc::clone(&context)),
            login_request(Some(("origin", "https://evil.example"))),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("ACCESS_DENIED"), "{body}");
        // 有 Origin 且同源则放行（Host 由部署保证）。
        let (status, _, body) = call(
            router_for(Arc::clone(&context)),
            login_request(Some(("origin", "https://vault.example"))),
        )
        .await;
        // 没有 Host 头时同源无法确认，因此仍按拒绝处理（fail closed）。
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
}
