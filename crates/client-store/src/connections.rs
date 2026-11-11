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
