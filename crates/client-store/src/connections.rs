//! 本机按服务器身份保存的连接地址与信任配置。应用层校验地址和 CA，凭据由 Windows 保管。

use vaultsyncd_domain::ids::ServerId;

use crate::db::{Database, DbError};

/// 已配置的服务器入口。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionRecord {
    /// 服务器实例身份。
    pub server_id: ServerId,
    /// 已确认的服务器地址。
    pub server_url: String,
    /// 可选的本机附加 CA 文件路径。
    pub ca_file: Option<String>,
    /// 是否允许开发用的回环 HTTP 入口。
    pub development_http: bool,
}

/// 按服务器身份返回所有连接。尚未添加服务器时返回空列表。
pub async fn list(db: &Database) -> Result<Vec<ConnectionRecord>, DbError> {
    let rows: Vec<(String, String, Option<String>, bool)> = sqlx::query_as(
        "SELECT server_id, server_url, ca_file, development_http \
         FROM agent_connections ORDER BY server_id",
    )
    .fetch_all(db.pool())
    .await
    .map_err(DbError::Sqlite)?;
    rows.into_iter()
        .map(|(server_id, server_url, ca_file, development_http)| {
            let server_id = ServerId::from_hex(&server_id)
                .map_err(|error| DbError::Sqlite(sqlx::Error::Decode(Box::new(error))))?;
            Ok(ConnectionRecord {
                server_id,
                server_url,
                ca_file,
                development_http,
            })
        })
        .collect()
}

/// 移除一台服务器的本地配置时区分：该身份是否仍被持久绑定引用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// 连接、登录资料已删除，并留存移除证据。
    Removed,
    /// 仍有绑定（含已解除绑定）引用该服务器；本次没有改动任何行。
    HasBindings,
}

/// 该身份是否已被用户移除过。用于阻止 `agent.json` 里的旧单连接被当作后备入口复活。
pub async fn removed(db: &Database, server: &ServerId) -> Result<bool, DbError> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_connection_removals WHERE server_id=?)")
        .bind(server.to_string())
        .fetch_one(db.pool())
        .await
        .map_err(DbError::Sqlite)
}

/// 该身份是否仍被正在管理的绑定引用；移除连接的前置条件。
///
/// 已解除绑定的记录保留恢复证据，不阻止移除服务器，也不会被移除操作删除。
pub async fn in_use(db: &Database, server: &ServerId) -> Result<bool, DbError> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM bindings b LEFT JOIN binding_controls c \
         ON c.binding_id = b.id WHERE b.server_id = ? AND coalesce(c.detached, 0) = 0)",
    )
    .bind(server.as_bytes().as_slice())
    .fetch_one(db.pool())
    .await
    .map_err(DbError::Sqlite)
}

/// 删除已保存的连接与登录资料，并在同一事务里留存移除证据。
///
/// 仍有绑定时返回 [`Removal::HasBindings`] 且不改动任何行：调用方须先解除绑定，
/// 由独立命令保留本地内容与恢复记录。凭据由 agent 在 Windows 凭据库中单独删除。
pub async fn remove(db: &Database, server: &ServerId) -> Result<Removal, DbError> {
    let mut tx = db.begin_write().await?;
    // bindings.server_id 是 16 字节 BLOB，身份文本不能直接比较。已解除绑定的记录保留。
    let in_use: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM bindings b LEFT JOIN binding_controls c \
         ON c.binding_id = b.id WHERE b.server_id = ? AND coalesce(c.detached, 0) = 0)",
    )
    .bind(server.as_bytes().as_slice())
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::Sqlite)?;
    if in_use {
        return Ok(Removal::HasBindings);
    }
    sqlx::query("DELETE FROM agent_login_profiles WHERE server_id=?")
        .bind(server.to_string())
        .execute(&mut *tx)
        .await
        .map_err(DbError::Sqlite)?;
    sqlx::query(
        "INSERT INTO agent_connection_removals (server_id) VALUES (?1) \
         ON CONFLICT(server_id) DO NOTHING",
    )
    .bind(server.to_string())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Sqlite)?;
    sqlx::query("DELETE FROM agent_connections WHERE server_id=?")
        .bind(server.to_string())
        .execute(&mut *tx)
        .await
        .map_err(DbError::Sqlite)?;
    tx.commit().await.map_err(DbError::Sqlite)?;
    Ok(Removal::Removed)
}

/// 写入一台服务器的已确认配置。重复写入同一身份只更新该入口。
pub async fn upsert(db: &Database, record: &ConnectionRecord) -> Result<(), DbError> {
    let mut tx = db.begin_write().await?;
    sqlx::query(
        "INSERT INTO agent_connections (server_id, server_url, ca_file, development_http) \
         VALUES (?1, ?2, ?3, ?4) ON CONFLICT(server_id) DO UPDATE SET \
         server_url = excluded.server_url, ca_file = excluded.ca_file, \
         development_http = excluded.development_http",
    )
    .bind(record.server_id.to_string())
    .bind(&record.server_url)
    .bind(&record.ca_file)
    .bind(record.development_http)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Sqlite)?;
    tx.commit().await.map_err(DbError::Sqlite)
}
