//! 在线本机管理适配器；身份来自 Unix peer credentials，不创建会话、不接受 SQL/凭据。
//! 文档形状唯一来源为 `api/admin-socket.schema.json`，动态 JSON 不另维护 DTO。

use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::body::to_bytes;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::warn;
use vaultsyncd_domain::ids::{EntryId, RootId, UserId};
use vaultsyncd_platform_linux_server::ipc::{self, AdminSocket};
use vaultsyncd_protocol::generated::RootRegisterRequest;
use vaultsyncd_server_store::{db::Database, roots};

use crate::lifecycle::Lifecycle;
use crate::roots::RootContext;

const PAGE_SIZE: i64 = 100;

/// 服务内状态；仅本机 UID 认证成功后进入管理用例。
pub(crate) struct AdminContext {
    pub database: Database,
    pub roots: Arc<RootContext>,
    pub lifecycle: Arc<Lifecycle>,
}

/// 有界并发（16）、单请求连接、10 秒截止；停止时等待现有请求，不让慢客户端拖住进程。
pub(crate) async fn serve(
    socket: AdminSocket,
    context: Arc<AdminContext>,
    mut stop: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut requests = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break,
            Some(_) = requests.join_next(), if !requests.is_empty() => {},
            accepted = socket.listener.accept(), if requests.len() < 16 => {
                let (mut stream, _) = accepted?;
                if ipc::authorize_peer(&stream).is_err() { continue; }
                let context = Arc::clone(&context);
                requests.spawn(async move {
                    let exchange = async {
                        let bytes = ipc::read_frame(&mut stream).await?;
                        let result = match serde_json::from_slice::<Vec<String>>(&bytes) {
                            Ok(args) if args.first().is_some_and(|v| v == "v1") && args.len() <= 5 => dispatch(&context, &args[1..]).await,
                            Ok(_) => failure(400, "INVALID_REQUEST", "不支持的协议版本或命令过长"),
                            _ => failure(400, "INVALID_REQUEST", "需要命令字符串数组"),
                        };
                        let mut bytes = serde_json::to_vec(&result).map_err(io::Error::other)?;
                        if bytes.len() > ipc::MAX_FRAME {
                            bytes = serde_json::to_vec(&failure(413, "RESPONSE_TOO_LARGE", "单页过大，请缩小管理数据后重试或使用受认证 HTTPS")).map_err(io::Error::other)?;
                        }
                        ipc::write_frame(&mut stream, &bytes).await
                    };
                    if let Ok(Err(error)) = tokio::time::timeout(Duration::from_secs(10), exchange).await {
                        warn!(error = %error, "本机管理连接失败");
                    }
                });
            }
        }
    }
    while requests.join_next().await.is_some() {}
    Ok(())
}

fn failure(status: u16, code: &str, message: &str) -> Value {
    json!({"status":status,"error":{"code":code,"message":message}})
}

fn success(result: Value) -> Value {
    json!({"status":200,"result":result})
}

async fn response(response: axum::response::Response) -> Value {
    let status = response.status().as_u16();
    match to_bytes(response.into_body(), ipc::MAX_FRAME).await {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) if status < 400 => json!({"status":status,"result":value}),
            Ok(value) => json!({"status":status,"error":value["error"]}),
            Err(_) => failure(503, "NEEDS_RECOVERY", "管理用例返回了非法 JSON"),
        },
        Err(_) => failure(413, "RESPONSE_TOO_LARGE", "管理响应超过帧上限"),
    }
}

async fn dispatch(context: &AdminContext, args: &[String]) -> Value {
    match execute(context, args).await {
        Ok(value) => value,
        Err(error) => {
            warn!(error = %error, "读取在线管理状态失败");
            failure(503, "NEEDS_RECOVERY", "读取在线管理状态失败，详见服务日志")
        }
    }
}

async fn execute(
    context: &AdminContext,
    args: &[String],
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = context.database.pool().acquire().await?;
    // 首发单 owner 模型；不猜 login，不加载密码/散列/会话。多 owner 时明确拒绝而不是冒用第一人。
    let owners: Vec<String> =
        sqlx::query_scalar("SELECT lower(hex(id)) FROM users WHERE status = 'active' LIMIT 2")
            .fetch_all(&mut *conn)
            .await?;
    if owners.len() != 1 {
        return Ok(failure(
            409,
            "OWNER_REQUIRED",
            "本机管理要求唯一 active owner；请先离线 init，或明确配置多账号管理策略",
        ));
    }
    let owner = UserId::from_hex(&owners[0])?;
    let Some(command) = args.first().map(String::as_str) else {
        return Ok(failure(400, "INVALID_REQUEST", "缺少命令"));
    };
    if command == "status" && args.len() == 1 {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM roots WHERE owner_id = ?")
            .bind(owner.as_bytes().as_slice())
            .fetch_one(&mut *conn)
            .await?;
        return Ok(success(
            json!({"phase":context.lifecycle.phase().as_str(),"root_count":count.to_string(),"recovery_detail":context.lifecycle.recovery_detail()}),
        ));
    }
    drop(conn);
    if command == "roots" && args.len() == 3 && args[1] == "register" {
        if context.lifecycle.phase() != crate::lifecycle::Phase::Ready {
            return Ok(failure(
                503,
                "NEEDS_RECOVERY",
                "服务未就绪，不开放 Root 注册",
            ));
        }
        return Ok(response(
            crate::roots::register_for_owner(
                Arc::clone(&context.roots),
                owner,
                RootRegisterRequest {
                    path: args[2].clone(),
                },
            )
            .await,
        )
        .await);
    }
    // 所有分页命令固定 100 条；page 是十进制偏移页号，不是持久快照，不承诺并发写入期间一致性。
    let expected = match command {
        "roots" | "problems" => 1,
        "trash" => 2,
        "history" => 3,
        _ => 0,
    };
    if expected == 0 || !(args.len() == expected || args.len() == expected + 1) {
        return Ok(failure(400, "INVALID_REQUEST", "未知命令或参数数量不匹配"));
    }
    let page = if args.len() == expected + 1 {
        match args[expected].parse::<u32>() {
            Ok(page) if page <= 1_000_000 && page.to_string() == args[expected] => page,
            _ => {
                return Ok(failure(
                    400,
                    "INVALID_REQUEST",
                    "页号必须在 0..1000000 内且没有前导零",
                ));
            }
        }
    } else {
        0
    };
    if command == "roots" {
        return Ok(response(
            crate::roots::list_for_owner(Arc::clone(&context.roots), owner, Some(page)).await,
        )
        .await);
    }
    let mut conn = context.database.pool().acquire().await?;
    let root = if command != "problems" {
        let root = match RootId::from_hex(&args[1]) {
            Ok(root) if root.to_hex() == args[1] => root,
            _ => return Ok(failure(400, "INVALID_REQUEST", "RootId 格式无效")),
        };
        if roots::root_in_scope(&mut conn, owner, root)
            .await?
            .is_none()
        {
            return Ok(failure(403, "ACCESS_DENIED", "Root 不存在或不属于 owner"));
        }
        Some(root)
    } else {
        None
    };
    if command == "history"
        && !matches!(EntryId::from_hex(&args[2]), Ok(id) if id.to_hex() == args[2])
    {
        return Ok(failure(400, "INVALID_REQUEST", "EntryId 格式无效"));
    }
    // 此处是服务端只读管理查询，不让 CLI 打开数据库；字段白名单绝不 SELECT 用户或会话材料。
    let sql = match command {
        "history" => {
            "SELECT json_object('version_id', lower(hex(v.id)), 'entry_id', lower(hex(v.entry_id)), 'size', cast(v.size AS TEXT), 'created_seq', cast(v.created_seq AS TEXT), 'origin', v.origin, 'superseded_at', v.superseded_at) FROM versions v WHERE v.root_id = ?1 AND lower(hex(v.entry_id)) = ?2 ORDER BY v.created_seq DESC, v.id LIMIT 101 OFFSET ?3"
        }
        "trash" => {
            "SELECT json_object('entry_id', lower(hex(t.entry_id)), 'path', t.path, 'kind', t.kind, 'version_id', CASE WHEN t.content_version_id IS NULL THEN NULL ELSE lower(hex(t.content_version_id)) END, 'deleted_at', t.deleted_at, 'expires_at', t.expires_at) FROM trash_entries t WHERE t.root_id = ?1 AND ?2 IS NULL ORDER BY t.entry_id LIMIT 101 OFFSET ?3"
        }
        "problems" => {
            "SELECT document FROM (SELECT 'root:' || lower(hex(r.id)) AS key, json_object('kind', 'root_health', 'root_id', lower(hex(r.id)), 'health', r.health, '_path', r.absolute_path, '_identity', cast(r.root_file_identity AS TEXT)) AS document FROM roots r WHERE r.owner_id = ?1 UNION ALL SELECT 'conflict:' || lower(hex(c.root_id)) || lower(hex(c.sibling_entry_id)), json_object('kind', 'conflict', 'root_id', lower(hex(c.root_id)), 'entry_id', lower(hex(c.source_entry_id)), 'copy_entry_id', lower(hex(c.sibling_entry_id)), 'path', c.path, 'reason', c.reason) FROM conflicts c JOIN roots r ON r.id = c.root_id WHERE r.owner_id = ?1) WHERE ?2 IS NULL ORDER BY key LIMIT 101 OFFSET ?3"
        }
        _ => unreachable!(),
    };
    let scope = root.map_or_else(
        || owner.as_bytes().to_vec(),
        |root| root.as_bytes().to_vec(),
    );
    let rows: Vec<String> = sqlx::query_scalar(sql)
        .bind(scope)
        .bind(if command == "history" {
            Some(args[2].as_str())
        } else {
            None
        })
        .bind(i64::from(page) * PAGE_SIZE)
        .fetch_all(&mut *conn)
        .await?;
    let has_more = rows.len() > PAGE_SIZE as usize;
    let mut items = Vec::new();
    for row in rows.into_iter().take(PAGE_SIZE as usize) {
        let mut value: Value = serde_json::from_str(&row)?;
        if value["kind"] == "root_health" {
            let path = value
                .as_object_mut()
                .expect("query emits object")
                .remove("_path")
                .ok_or("missing Root path")?;
            let identity = value
                .as_object_mut()
                .expect("query emits object")
                .remove("_identity")
                .ok_or("missing Root identity")?;
            let expected = vaultsyncd_platform_linux_server::files::root::RootSnapshot::parse_spec(
                identity.as_str().ok_or("invalid Root identity")?,
            )?;
            let path = std::path::PathBuf::from(path.as_str().ok_or("invalid Root path")?);
            let reason = tokio::task::spawn_blocking(move || {
                vaultsyncd_platform_linux_server::mounts::health::verify_root_mount(
                    &path, &expected,
                )
                .err()
                .map(|error| error.detail_code())
            })
            .await?;
            if let Some(reason) = reason {
                value["health"] = json!("unavailable");
                value["reason"] = json!(reason);
            } else if value["health"] == "healthy" {
                continue;
            }
        }
        items.push(value);
    }
    Ok(success(
        json!({"items":items,"next_page":if has_more { Some(page + 1) } else { None },"recovery_detail":if command == "problems" { context.lifecycle.recovery_detail() } else { None }}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vaultsyncd_server_store::users::{self, NewUser};

    #[tokio::test]
    async fn owner_and_storage_failures_never_become_empty_success_or_sessions() {
        let path =
            std::env::temp_dir().join(format!("vaultsyncd-s075-usecases-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        let database = Database::open(path.join("server.sqlite")).await.unwrap();
        let lifecycle = Arc::new(Lifecycle::new());
        lifecycle.mark_ready();
        let context = AdminContext {
            roots: Arc::new(RootContext::new(
                Arc::new(crate::auth::AuthContext::new(database.clone())),
                None,
                path.clone(),
                Default::default(),
            )),
            database: database.clone(),
            lifecycle,
        };
        let status = vec!["status".to_owned()];
        assert_eq!(
            dispatch(&context, &status).await["error"]["code"],
            "OWNER_REQUIRED"
        );
        for (id, login) in [(1, "owner"), (2, "other")] {
            let mut tx = database.begin_write().await.unwrap();
            users::insert_user(
                &mut tx,
                &NewUser {
                    id: UserId::from_bytes([id; 16]),
                    login: login.to_owned(),
                    password_hash: "test-unused-hash".to_owned(),
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            if id == 1 {
                assert_eq!(
                    dispatch(&context, &status).await["result"]["root_count"],
                    "0"
                );
                assert_eq!(
                    dispatch(&context, &["unknown".to_owned()]).await["status"],
                    400
                );
                let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
                    .fetch_one(database.pool())
                    .await
                    .unwrap();
                assert_eq!(count, 0, "本机身份不得签发或持久化会话");
            } else {
                assert_eq!(
                    dispatch(&context, &status).await["error"]["code"],
                    "OWNER_REQUIRED"
                );
            }
        }
        database.close().await;
        assert_eq!(
            dispatch(&context, &status).await["error"]["code"],
            "NEEDS_RECOVERY"
        );
        std::fs::remove_dir_all(path).unwrap();
    }
}
