//! 真实 namespace 发布后、元数据事务提交前的 SQL 故障：保留 pending 与意图，禁止继续写入。
//! 重启必须补完原操作、开放写入，同一 OpId 仍返回同一条目和事件。
mod common;

use common::{Lab, Running, create_snapshot, identity, login, register_vault, snapshot_entries};
use reqwest::StatusCode;
use serde_json::json;
use sqlx::Connection;

#[tokio::test]
async fn published_namespace_failure_recovers_the_original_operation_after_restart() {
    let lab = Lab::new("command-failure", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动隔离实例");
    let token = login(&running).await.expect("登录");
    let vault = register_vault(&running, &token, &lab.work())
        .await
        .expect("注册");
    let snapshot = create_snapshot(&running, &token, &vault)
        .await
        .expect("快照");
    let entries = snapshot_entries(
        &running,
        &token,
        &vault,
        snapshot["snapshot_id"].as_str().expect("快照身份"),
    )
    .await
    .expect("清单");
    let parent = entries
        .iter()
        .find(|entry| entry["parent_id"].is_null())
        .expect("根条目")["entry_id"]
        .clone();
    let mut conn = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(lab.state().join("server.sqlite")),
    )
    .await
    .expect("只对本测试数据库注入故障");
    sqlx::query("CREATE TRIGGER s052_fail_commit BEFORE UPDATE OF state ON operations WHEN NEW.state = 'committed' BEGIN SELECT RAISE(ABORT, 'S052 injected transaction failure'); END")
        .execute(&mut conn).await.expect("设置事务故障点");
    let op = identity(0x91);
    let request = json!({
        "op_id": op,
        "device_id": identity(0x92),
        "server_epoch": snapshot["server_epoch"],
        "kind": "create",
        "parent_id": parent,
        "name": "published-only",
        "expected_absent": true,
    });
    let uri = format!("/v1/vaults/{vault}/operations");
    let (status, response) = running
        .json("POST", &uri, Some(&token), Some(&request))
        .await
        .expect("提交");
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
    assert!(
        lab.work().join("published-only").is_dir(),
        "故障确在文件系统发布之后"
    );
    let state: String = sqlx::query_scalar("SELECT state FROM operations WHERE hex(op_id) = ?1")
        .bind(op.to_uppercase())
        .fetch_one(&mut conn)
        .await
        .expect("操作记录");
    assert_eq!(state, "pending", "恢复证据不能被写成稳定拒绝");
    let phase: String = sqlx::query_scalar("SELECT phase FROM fs_intents")
        .fetch_one(&mut conn)
        .await
        .expect("持久意图");
    assert_eq!(phase, "published");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM changes")
        .fetch_one(&mut conn)
        .await
        .expect("事件数");
    assert_eq!(count, 0, "业务事务整体回滚，无 committed 事件");
    sqlx::query("DROP TRIGGER s052_fail_commit")
        .execute(&mut conn)
        .await
        .expect("移除故障");

    // 即便库恢复可写，也不能绕过前一个未完成意图继续发布。
    let mut next = request.clone();
    next["op_id"] = json!(identity(0x93));
    next["name"] = json!("must-not-publish");
    let (status, response) = running
        .json("POST", &uri, Some(&token), Some(&next))
        .await
        .expect("第二条命令");
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
    assert!(!lab.work().join("must-not-publish").exists());
    let (status, replay) = running
        .json("POST", &uri, Some(&token), Some(&request))
        .await
        .expect("原操作重放");
    assert_eq!(status, StatusCode::ACCEPTED, "{replay}");
    assert_eq!(replay["state"], "pending");
    assert!(
        replay
            .get("vault_seq")
            .is_none_or(serde_json::Value::is_null)
    );

    conn.close().await.expect("关闭故障连接");
    running.stop().await;
    let restarted = vaultsyncd_server::bootstrap::prepare(lab.config())
        .await
        .expect("重启评估");
    assert!(
        restarted.assessment().is_complete(),
        "已发布且有完整关联的意图应补完：{:?}",
        restarted.assessment()
    );
    drop(restarted);
    let running = Running::start(&lab).await.expect("恢复后开放写入");
    let token = login(&running).await.expect("重新登录");
    let (status, result) = running
        .json("POST", &uri, Some(&token), Some(&request))
        .await
        .expect("重放已补完操作");
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["state"], "committed");
    assert_eq!(result["replayed"], true);
    assert_eq!(result["vault_seq"], "1");
    let mut conn = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(lab.state().join("server.sqlite")),
    )
    .await
    .expect("核对业务库");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM changes")
        .fetch_one(&mut conn)
        .await
        .expect("事件");
    assert_eq!(count, 1, "重启和重试不能追加第二个事件");
    let phase: String = sqlx::query_scalar("SELECT phase FROM fs_intents")
        .fetch_one(&mut conn)
        .await
        .expect("收尾意图");
    assert_eq!(phase, "finalized");
    conn.close().await.expect("关闭核对连接");
    running.stop().await;
}
