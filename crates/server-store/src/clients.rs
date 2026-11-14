//! 连接观测记录，不参与认证决策；不持有令牌或凭据摘要。
use crate::sessions::NewSession;
use sqlx::{Row, Sqlite, SqliteConnection, Transaction};
use std::net::IpAddr;
use vaultsyncd_domain::ids::{SessionId, UserId};
use vaultsyncd_domain::version::UtcTimestamp;

/// 每页连接数上限。
pub const PAGE_SIZE: usize = 50;

/// 只读连接摘要；时间均为 UTC Unix 秒。
#[derive(Debug)]
pub struct ClientConnection {
    /// 稳定连接身份。
    pub id: String,
    /// 非可信登录标签。
    pub device_label: String,
    /// 服务端登录入口类别。
    pub client_kind: String,
    /// 首次成功登录来源；旧记录未知。
    pub login_ip: Option<String>,
    /// 首次登录时间。
    pub created_at: i64,
    /// 最近认证请求时间；历史记录可未知。
    pub last_seen_at: Option<i64>,
    /// 当前刷新凭据有效期。
    pub refresh_expires_at: Option<i64>,
    /// 当前会话撤销时间。
    pub revoked_at: Option<i64>,
    /// 是否为请求方连接。
    pub current: bool,
}

pub(crate) async fn record_login(
    tx: &mut Transaction<'_, Sqlite>,
    new: &NewSession,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO client_connections(id,user_id,session_id,device_label,client_kind,created_at,last_seen_at) VALUES(?1,?2,?1,?3,'unknown',?4,?4)")
        .bind(new.id.as_bytes().as_slice()).bind(new.user_id.as_bytes().as_slice())
        .bind(&new.device_label).bind(new.created_at.to_db_integer()).execute(&mut **tx).await?;
    Ok(())
}

pub(crate) async fn record_rotation(
    tx: &mut Transaction<'_, Sqlite>,
    old: SessionId,
    new: SessionId,
    at: UtcTimestamp,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE client_connections SET session_id=?2,last_seen_at=max(coalesce(last_seen_at,created_at),?3) WHERE session_id=?1")
        .bind(old.as_bytes().as_slice()).bind(new.as_bytes().as_slice()).bind(at.to_db_integer()).execute(&mut **tx).await?;
    Ok(())
}

/// 成功登录的实际入口决定接入类型，设备标签不用于推断。
pub async fn set_login_details(
    conn: &mut SqliteConnection,
    session: SessionId,
    browser: bool,
    login_ip: Option<IpAddr>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE client_connections SET client_kind=?2,login_ip=?3 WHERE session_id=?1")
        .bind(session.as_bytes().as_slice())
        .bind(if browser { "browser" } else { "client" })
        .bind(login_ip.map(|ip| ip.to_string()))
        .execute(conn)
        .await?;
    Ok(())
}

/// 登录事务中保存客户端声明属性；刷新和请求活动不改写此快照。
pub async fn set_host_properties(
    conn: &mut SqliteConnection,
    session: SessionId,
    hostname: Option<&str>,
    operating_system: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE client_connections SET hostname=?2,operating_system=?3 WHERE session_id=?1",
    )
    .bind(session.as_bytes().as_slice())
    .bind(hostname)
    .bind(operating_system)
    .execute(conn)
    .await?;
    Ok(())
}

/// 仅成功认证后调用；每连接最多30秒更新一次，倒退时钟不覆盖较新证据。
pub async fn touch(
    conn: &mut SqliteConnection,
    session: SessionId,
    at: UtcTimestamp,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE client_connections SET last_seen_at=?2 WHERE session_id=?1 AND (last_seen_at IS NULL OR last_seen_at<=?2-30) AND created_at<=?2")
        .bind(session.as_bytes().as_slice()).bind(at.to_db_integer()).execute(conn).await?;
    Ok(())
}

/// 当前账号按首次连接时间/身份键集分页；cursor 必须由调用者先验证属于该账号。
pub async fn list(
    conn: &mut SqliteConnection,
    user: UserId,
    current: SessionId,
    after: Option<SessionId>,
) -> Result<Vec<ClientConnection>, sqlx::Error> {
    let rows = sqlx::query("SELECT lower(hex(c.id)) AS id,c.device_label,c.client_kind,c.login_ip,c.created_at,c.last_seen_at,s.refresh_expires_at,s.revoked_at,c.session_id=?2 AS current FROM client_connections c LEFT JOIN sessions s ON s.id=c.session_id WHERE c.user_id=?1 AND (?3 IS NULL OR (c.created_at,c.id)<(SELECT created_at,id FROM client_connections WHERE id=?3 AND user_id=?1)) ORDER BY c.created_at DESC,c.id DESC LIMIT 51")
        .bind(user.as_bytes().as_slice()).bind(current.as_bytes().as_slice()).bind(after.map(|id| id.as_bytes().to_vec())).fetch_all(conn).await?;
    rows.into_iter()
        .map(|row| {
            Ok(ClientConnection {
                id: row.try_get("id")?,
                device_label: row.try_get("device_label")?,
                client_kind: row.try_get("client_kind")?,
                login_ip: row.try_get("login_ip")?,
                created_at: row.try_get("created_at")?,
                last_seen_at: row.try_get("last_seen_at")?,
                refresh_expires_at: row.try_get("refresh_expires_at")?,
                revoked_at: row.try_get("revoked_at")?,
                current: row.try_get::<Option<bool>, _>("current")?.unwrap_or(false),
            })
        })
        .collect()
}

/// 检查分页游标是否属于当前账号。
pub async fn cursor_exists(
    conn: &mut SqliteConnection,
    user: UserId,
    id: SessionId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_connections WHERE user_id=?1 AND id=?2)")
        .bind(user.as_bytes().as_slice())
        .bind(id.as_bytes().as_slice())
        .fetch_one(conn)
        .await
}

#[cfg(test)]
mod tests;
