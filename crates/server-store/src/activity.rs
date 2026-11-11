//! 文件活动账本。记录身份用于倒序分页；业务提交证据与业务事务一起落盘。
use sqlx::{Row, Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{SessionId, UploadId, UserId, VaultId};
use vaultsyncd_domain::version::UtcTimestamp;

use crate::operations::{OperationKey, RequestDigest};

/// 仅含认证连接的公开属性，不持有令牌。
#[derive(Clone, Debug)]
pub struct Client {
    /// 稳定登录连接身份。
    pub id: Option<Vec<u8>>,
    /// 登录标签快照，不用于授权。
    pub label: String,
    /// 实际登录入口类别。
    pub kind: String,
}

/// 由已认证会话解析连接；旧数据缺少连接时明确未知。
pub async fn client(
    conn: &mut SqliteConnection,
    owner: UserId,
    session: SessionId,
) -> Result<Client, sqlx::Error> {
    // refresh可在请求认证后推进session_id；沿同账号刷新链追到稳定连接，保留在途请求归属。
    let row = sqlx::query("WITH RECURSIVE chain(id,replaced_by,depth) AS (SELECT id,replaced_by,0 FROM sessions WHERE user_id=?1 AND id=?2 UNION ALL SELECT s.id,s.replaced_by,c.depth+1 FROM sessions s JOIN chain c ON s.id=c.replaced_by WHERE s.user_id=?1 AND c.depth<1024) SELECT id,device_label,client_kind FROM client_connections WHERE user_id=?1 AND session_id IN(SELECT id FROM chain)")
        .bind(owner.as_bytes().as_slice()).bind(session.as_bytes().as_slice()).fetch_optional(conn).await?;
    row.map(|r| {
        Ok(Client {
            id: Some(r.try_get("id")?),
            label: r.try_get("device_label")?,
            kind: r.try_get("client_kind")?,
        })
    })
    .unwrap_or_else(|| {
        Ok(Client {
            id: None,
            label: "未知客户端".into(),
            kind: "unknown".into(),
        })
    })
}

/// 在执行前持久化认证归属；相同键首次归属不可被重试覆盖。false表示OpId已用于不同语义。
pub async fn bind_operation(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    session: SessionId,
    key: &OperationKey,
    digest: &RequestDigest,
    upload: Option<UploadId>,
) -> Result<bool, sqlx::Error> {
    let caller = client(tx, owner, session).await?;
    sqlx::query("INSERT INTO operation_activity_sources(vault_id,device_id,op_id,request_digest,client_id,client_label,client_kind,upload_id) SELECT ?1,?2,?3,?4,?5,?6,?7,?8 WHERE EXISTS(SELECT 1 FROM vaults WHERE id=?1 AND owner_id=?9) ON CONFLICT DO NOTHING")
        .bind(key.vault.as_bytes().as_slice()).bind(key.device.as_bytes().as_slice()).bind(key.op.as_bytes().as_slice()).bind(digest.as_bytes().as_slice())
        .bind(caller.id).bind(caller.label).bind(caller.kind).bind(upload.map(|id| id.as_bytes().to_vec())).bind(owner.as_bytes().as_slice()).execute(&mut **tx).await?;
    let stored: Vec<u8> = sqlx::query_scalar("SELECT request_digest FROM operation_activity_sources WHERE vault_id=?1 AND device_id=?2 AND op_id=?3")
        .bind(key.vault.as_bytes().as_slice()).bind(key.device.as_bytes().as_slice()).bind(key.op.as_bytes().as_slice()).fetch_one(&mut **tx).await?;
    Ok(stored == digest.as_bytes())
}

/// commit_operation 的同事务钩子；只记录已捕获认证归属的新操作。
pub(crate) async fn committed(
    tx: &mut Transaction<'_, Sqlite>,
    key: &OperationKey,
    events: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO file_activity(vault_id,occurred_at,client_id,client_label,client_kind,device_id,direction,action,status,entry_id,version_id,path,size,operation_id,upload_id) SELECT s.vault_id,unixepoch(),s.client_id,s.client_label,s.client_kind,s.device_id,'upload','sync_commit','committed',json_extract(e.value,'$.entry_id'),json_extract(e.value,'$.version_id'),json_extract(e.value,'$.path'),CAST(json_extract(e.value,'$.size') AS INTEGER),s.op_id,s.upload_id FROM operation_activity_sources s JOIN operations o USING(vault_id,device_id,op_id),json_each(?4) e WHERE s.vault_id=?1 AND s.device_id=?2 AND s.op_id=?3 AND s.request_digest=o.request_digest AND o.state='committed' AND s.upload_id IS NOT NULL AND json_extract(e.value,'$.version_id') IS NOT NULL")
        .bind(key.vault.as_bytes().as_slice()).bind(key.device.as_bytes().as_slice()).bind(key.op.as_bytes().as_slice()).bind(events).execute(&mut **tx).await?;
    Ok(())
}

/// 一次请求已知的传输事实；不把响应准备好等同客户端接收成功。
pub struct Transfer<'a> {
    /// Vault。
    pub vault: VaultId,
    /// 观测时间。
    pub at: UtcTimestamp,
    /// 认证连接快照。
    pub client: &'a Client,
    /// upload_finalize/download。
    pub action: &'a str,
    /// ready/served/failed。
    pub status: &'a str,
    /// 固定条目身份。
    pub entry: Option<&'a str>,
    /// 固定版本身份。
    pub version: Option<&'a str>,
    /// 当时路径。
    pub path: Option<&'a str>,
    /// 整份大小。
    pub size: Option<u64>,
    /// 本次提供的字节。
    pub bytes: Option<u64>,
    /// 本次提供的偏移。
    pub offset: Option<u64>,
    /// 上传身份。
    pub upload: Option<UploadId>,
    /// 失败码。
    pub error: Option<&'a str>,
}

/// 持久化传输事实；ready按上传幂等。
pub async fn record(conn: &mut SqliteConnection, item: &Transfer<'_>) -> Result<(), sqlx::Error> {
    let number = |value: Option<u64>| {
        value
            .map(i64::try_from)
            .transpose()
            .map_err(|_| sqlx::Error::Protocol("活动字节数越界".into()))
    };
    sqlx::query("INSERT INTO file_activity(vault_id,occurred_at,client_id,client_label,client_kind,direction,action,status,entry_id,version_id,path,size,bytes,offset,upload_id,error_code) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16) ON CONFLICT DO NOTHING")
        .bind(item.vault.as_bytes().as_slice()).bind(item.at.to_db_integer()).bind(&item.client.id).bind(&item.client.label).bind(&item.client.kind)
        .bind(if item.action=="download" { "download" } else { "upload" }).bind(item.action).bind(item.status).bind(item.entry).bind(item.version).bind(item.path)
        .bind(number(item.size)?).bind(number(item.bytes)?).bind(number(item.offset)?).bind(item.upload.map(|id| id.as_bytes().to_vec())).bind(item.error).execute(conn).await?;
    Ok(())
}

/// 只读账本行，数量/身份保留为SQLite整数，在wire层转十进制文本。
#[derive(Debug, sqlx::FromRow)]
pub struct Record {
    /// 记录身份。
    pub id: i64,
    /// UTC秒。
    pub occurred_at: i64,
    /// 稳定连接身份。
    pub client_id: Option<String>,
    /// 标签快照。
    pub client_label: String,
    /// 登录时声明的主机名；旧记录未知。
    pub client_hostname: Option<String>,
    /// 登录时声明的操作系统。
    pub client_os: Option<String>,
    /// 首次登录来源IP；不代表实时地址。
    pub client_ip: Option<String>,
    /// 登录入口类别。
    pub client_kind: String,
    /// 操作声明的设备身份。
    pub device_id: Option<String>,
    /// upload/download。
    pub direction: String,
    /// 动作。
    pub action: String,
    /// 结果证据阶段。
    pub status: String,
    /// 条目。
    pub entry_id: Option<String>,
    /// 版本。
    pub version_id: Option<String>,
    /// 当时路径。
    pub path: Option<String>,
    /// 整份大小。
    pub size: Option<i64>,
    /// 本次字节。
    pub bytes: Option<i64>,
    /// 偏移。
    pub offset: Option<i64>,
    /// 业务操作。
    pub operation_id: Option<String>,
    /// 上传身份。
    pub upload_id: Option<String>,
    /// 失败码。
    pub error_code: Option<String>,
}

/// 先由API验证Vault归属，再按账号再次限制查询；读取51条以判断下一页。
pub async fn list(
    conn: &mut SqliteConnection,
    owner: UserId,
    vault: VaultId,
    before: Option<i64>,
    direction: Option<&str>,
) -> Result<Vec<Record>, sqlx::Error> {
    sqlx::query_as("SELECT a.id,a.occurred_at,CASE WHEN a.client_id IS NOT NULL THEN lower(hex(a.client_id)) END client_id,a.client_label,c.hostname AS client_hostname,c.operating_system AS client_os,c.login_ip AS client_ip,a.client_kind,CASE WHEN a.device_id IS NOT NULL THEN lower(hex(a.device_id)) END device_id,a.direction,a.action,a.status,a.entry_id,a.version_id,a.path,a.size,a.bytes,a.offset,CASE WHEN a.operation_id IS NOT NULL THEN lower(hex(a.operation_id)) END operation_id,CASE WHEN a.upload_id IS NOT NULL THEN lower(hex(a.upload_id)) END upload_id,a.error_code FROM file_activity a JOIN vaults v ON v.id=a.vault_id LEFT JOIN client_connections c ON c.id=a.client_id AND c.user_id=v.owner_id WHERE v.owner_id=?1 AND a.vault_id=?2 AND (?3 IS NULL OR a.id<?3) AND (?4 IS NULL OR a.direction=?4) ORDER BY a.id DESC LIMIT 51")
        .bind(owner.as_bytes().as_slice()).bind(vault.as_bytes().as_slice()).bind(before).bind(direction).fetch_all(conn).await
}

/// 固定条目的当前路径快照；删除条目仍可解析其原命名空间。
pub async fn path(
    conn: &mut SqliteConnection,
    vault: VaultId,
    entry: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("WITH RECURSIVE p(id,parent_id,path,depth) AS (SELECT id,parent_id,name,0 FROM entries WHERE vault_id=?1 AND lower(hex(id))=?2 UNION ALL SELECT e.id,e.parent_id,CASE WHEN e.parent_id IS NULL THEN p.path ELSE e.name||'/'||p.path END,p.depth+1 FROM entries e JOIN p ON p.parent_id=e.id WHERE e.vault_id=?1 AND p.depth<1024) SELECT path FROM p WHERE parent_id IS NULL")
        .bind(vault.as_bytes().as_slice()).bind(entry).fetch_optional(conn).await
}

#[cfg(test)]
mod tests;
