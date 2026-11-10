//! 同源账号管理；复用已认证身份、密码规则和数据库事务，不直写在线库。

use super::*;
use vaultsyncd_protocol::generated::{AccountChanged, AccountInfo, PasswordChangeRequest};
use vaultsyncd_server_core::auth::manage::{ChangeError, prepare_change};

/// 当前会话对应的账号信息，不包含令牌或散列。
pub async fn account(State(context): State<Arc<AuthContext>>, headers: HeaderMap) -> Response {
    let principal = match principal_from(&context, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let mut conn = match context.database.pool().acquire().await {
        Ok(conn) => conn,
        Err(_) => return internal_error("读取账号失败", "account"),
    };
    match users::find_by_id(&mut conn, principal.user_id).await {
        Ok(Some(user)) if user.status == users::UserStatus::Active => Json(AccountInfo {
            user_id: user.id.to_hex(),
            login: user.login,
            session_id: principal.session_id.to_hex(),
        })
        .into_response(),
        Ok(_) => auth_expired("account", "账号已不可用"),
        Err(_) => internal_error("读取账号失败", "account"),
    }
}

/// 修改当前账号密码，并原子撤销包含当前会话在内的全部会话。
pub async fn password(
    State(context): State<Arc<AuthContext>>,
    headers: HeaderMap,
    Json(body): Json<PasswordChangeRequest>,
) -> Response {
    let _gate = context.account_gate.lock().await;
    let principal = match principal_from(&context, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(now) = server_now() else {
        return internal_error("系统时钟不可用", "password");
    };
    let mut tx = match context.database.begin_write().await {
        Ok(tx) => tx,
        Err(_) => return internal_error("账号存储失败", "password"),
    };
    let user = match users::find_by_id(&mut tx, principal.user_id).await {
        Ok(Some(user)) if user.status == users::UserStatus::Active => user,
        Ok(_) => return auth_expired("password", "账号已不可用"),
        Err(_) => return internal_error("读取账号失败", "password"),
    };
    // 沿用登录限流，错误当前密码不能绕开失败计数。
    let mut accounts = DbAccounts {
        database: &context.database,
    };
    let mut throttle = context.throttle.lock().await;
    if let Err(error) = authenticate(
        &mut accounts,
        &context.hasher,
        &mut throttle,
        LoginAttempt {
            login: &user.login,
            password: &body.current_password,
            now,
        },
    )
    .await
    {
        return match error {
            LoginError::InvalidCredentials => error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidRequest,
                "password",
                "当前密码不正确",
                false,
                None,
            ),
            other => login_failure("password", &other),
        };
    }
    drop(throttle);
    let hash = match prepare_change(
        &context.hasher,
        &user.password_hash,
        &body.current_password,
        &body.new_password,
    ) {
        Ok(hash) => hash,
        Err(ChangeError::InvalidCurrent) => return auth_expired("password", "当前密码不正确"),
        Err(ChangeError::InvalidNew) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidRequest,
                "password",
                "新密码不能为空且不超过 1024 字节",
                false,
                None,
            );
        }
        Err(ChangeError::Unavailable) => return internal_error("密码服务不可用", "password"),
    };
    let count =
        match store_sessions::change_password(&mut tx, user.id, &user.password_hash, &hash, now)
            .await
        {
            Ok(Some(count)) => count,
            Ok(None) => return auth_expired("password", "账号已变更，请重新登录"),
            Err(_) => return internal_error("账号存储失败", "password"),
        };
    if tx.commit().await.is_err() {
        return internal_error("账号提交失败", "password");
    }
    changed(count)
}

/// 撤销当前账号全部会话（包括当前浏览器与刷新后继），不影响其他账号。
pub async fn revoke_all(State(context): State<Arc<AuthContext>>, headers: HeaderMap) -> Response {
    let _gate = context.account_gate.lock().await;
    let principal = match principal_from(&context, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(now) = server_now() else {
        return internal_error("系统时钟不可用", "revoke");
    };
    let mut tx = match context.database.begin_write().await {
        Ok(tx) => tx,
        Err(_) => return internal_error("会话存储失败", "revoke"),
    };
    let count = match store_sessions::revoke_all(&mut tx, principal.user_id, now).await {
        Ok(count) => count,
        Err(_) => return internal_error("会话存储失败", "revoke"),
    };
    if tx.commit().await.is_err() {
        return internal_error("会话提交失败", "revoke");
    }
    changed(count)
}

fn changed(count: u64) -> Response {
    let mut response = Json(AccountChanged {
        sessions_revoked: count.to_string(),
    })
    .into_response();
    clear_cookie(&mut response, ACCESS_COOKIE, "/");
    clear_cookie(&mut response, REFRESH_COOKIE, "/v1/auth");
    response
}
