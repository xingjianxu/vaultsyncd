//! S052 续接验收：过期基线的冲突副本（`preserve_both`，design §10.1—10.2、T02）。
//!
//! 迟到分支不覆盖服务器 head，也不被丢弃：它被提交为一个独立条目（冲突副本），服务器主条目的内容、
//! 版本与路径都不变；同一 OpId 重试拿到同一个副本身份。

mod common;

use common::{
    Lab, PutArgs, Running, create_snapshot, identity, login, put_operation, register_vault,
    snapshot_entries, upload, wait_for,
};
use reqwest::StatusCode;
use serde_json::Value;
use sqlx::Connection;

const V1: &[u8] = b"server head version one\n";
const LATE: &[u8] = b"late branch content that must be preserved\n";

fn sha256(bytes: &[u8]) -> String {
    common::hex(&vaultsyncd_platform_linux_server::files::hash::digest(
        bytes,
    ))
}

async fn snapshot(running: &Running, token: &str, vault: &str) -> Vec<Value> {
    let created = create_snapshot(running, token, vault).await.expect("快照");
    let id = created
        .get("snapshot_id")
        .and_then(Value::as_str)
        .expect("snapshot_id")
        .to_owned();
    snapshot_entries(running, token, vault, &id)
        .await
        .expect("快照清单")
}

#[tokio::test]
async fn stale_baseline_is_preserved_as_a_conflict_sibling() {
    let lab = Lab::new("conflict", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let vault = register_vault(&running, &token, &lab.work())
        .await
        .expect("注册 Vault");
    let work_file = lab.work().join("note.txt");
    std::fs::write(&work_file, V1).expect("写 head 内容");
    wait_for("初始采纳", || async {
        let entries = snapshot(&running, &token, &vault).await;
        Ok(entries
            .iter()
            .any(|entry| entry.get("name").and_then(Value::as_str) == Some("note.txt")))
    })
    .await
    .expect("初始采纳");

    let created = create_snapshot(&running, &token, &vault)
        .await
        .expect("快照");
    let epoch = created["server_epoch"].as_str().expect("epoch").to_owned();
    let entries = snapshot(&running, &token, &vault).await;
    let note = entries
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some("note.txt"))
        .expect("note.txt");
    let entry = note["entry_id"].as_str().expect("条目").to_owned();
    // 客户端计划时的基线：之后 head 会前进，这个 revision 就过期了。
    let stale_revision = note["revision"].as_str().expect("revision").to_owned();

    // 服务器 head 前进：外部改写被观察循环采纳（新版本）。
    std::fs::write(&work_file, b"server head advanced\n").expect("外部改写");
    wait_for("head 前进", || async {
        let entries = snapshot(&running, &token, &vault).await;
        Ok(entries.iter().any(|item| {
            item.get("entry_id").and_then(Value::as_str) == Some(&entry)
                && item.get("revision").and_then(Value::as_str) != Some(&stale_revision)
        }))
    })
    .await
    .expect("head 前进");

    // 迟到分支：用过期基线上传另一份内容并提交（preserve_both）。
    let (upload_id, _, _) = upload(&running, &token, &vault, LATE, 32)
        .await
        .expect("上传迟到分支");
    let device = identity(0x52);
    let args = PutArgs {
        op_id: &identity(0x71),
        device_id: &device,
        epoch: &epoch,
        entry: &entry,
        revision: &stale_revision,
        upload: &upload_id,
    };
    let (status, result) = put_operation(&running, &token, &vault, &args)
        .await
        .expect("提交迟到分支");
    assert_eq!(status, StatusCode::OK, "应提交为冲突副本：{result}");
    assert_eq!(result["state"], "committed");
    let conflict = result
        .get("conflict")
        .expect("committed 结果必须给出冲突副本");
    assert_eq!(conflict["reason"], "stale_base");
    assert_eq!(conflict["entry_id"], entry, "冲突映射指向服务器主条目");
    let sibling = conflict["conflict_entry_id"]
        .as_str()
        .expect("副本身份")
        .to_owned();
    assert_ne!(sibling, entry);

    // 服务器 head 不被覆盖：工作文件仍是服务器推进后的内容。
    assert_eq!(
        std::fs::read(&work_file).expect("读工作文件"),
        b"server head advanced\n",
        "迟到分支不得覆盖服务器 head"
    );
    // 副本内容与身份都在：名称带 conflict 标记、哈希等于迟到内容。
    let entries = snapshot(&running, &token, &vault).await;
    let sibling_entry = entries
        .iter()
        .find(|item| item.get("entry_id").and_then(Value::as_str) == Some(&sibling))
        .expect("冲突条目在快照里");
    assert_eq!(sibling_entry["hash"], sha256(LATE));
    let (_, changes) = running
        .json(
            "GET",
            &format!("/v1/vaults/{vault}/changes?after=0"),
            Some(&token),
            None,
        )
        .await
        .unwrap();
    let events: Vec<Value> = changes["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|batch| {
            serde_json::from_str::<Vec<Value>>(batch["events_json"].as_str().unwrap()).unwrap()
        })
        .collect();
    let event = events
        .iter()
        .find(|event| event["entry_id"] == sibling)
        .expect("conflict create event");
    assert_eq!(
        event["version_id"], sibling_entry["version_id"],
        "file create event must pin the exact committed version"
    );
    assert!(
        sibling_entry["name"]
            .as_str()
            .is_some_and(|name| name.contains("冲突")),
        "冲突条目名称应带标记：{}",
        sibling_entry["name"]
    );
    let sibling_path = sibling_entry["name"].as_str().expect("名称").to_owned();
    assert_eq!(
        std::fs::read(lab.work().join(&sibling_path)).expect("读副本"),
        LATE,
        "副本内容必须真的在工作目录里"
    );

    // 同一 OpId 重试：同一份结果与同一个副本（I01/I06）。
    let (status, replay) = put_operation(&running, &token, &vault, &args)
        .await
        .expect("重试");
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["conflict"]["conflict_entry_id"], sibling);
    assert_eq!(replay["vault_seq"], result["vault_seq"]);
    assert_ne!(replay["vault_seq"], "0");

    // 冲突映射落库：同一 OpId 只能产生一个副本（0008 迁移的 `conflicts` 表）。
    let mut conn = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(lab.state().join("server.sqlite"))
            .read_only(true),
    )
    .await
    .expect("只读打开数据库");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM conflicts")
        .fetch_one(&mut conn)
        .await
        .expect("查询冲突映射");
    assert_eq!(count, 1, "同一 OpId 只产生一个冲突副本");
    let (mapped_op, mapped_sibling, mapped_source): (String, String, String) = sqlx::query_as(
        "SELECT hex(op_id), hex(sibling_entry_id), hex(source_entry_id) FROM conflicts",
    )
    .fetch_one(&mut conn)
    .await
    .expect("读冲突映射");
    assert_eq!(mapped_op.to_lowercase(), identity(0x71));
    assert_eq!(mapped_sibling.to_lowercase(), sibling);
    assert_eq!(mapped_source.to_lowercase(), entry);

    // 迟到分支的响应也能查到结果（GET 操作状态）。
    let (status, value) = running
        .json(
            "GET",
            &format!("/v1/vaults/{vault}/operations/{}", identity(0x71)),
            Some(&token),
            None,
        )
        .await
        .expect("查询操作");
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["state"], "committed");
    assert_eq!(value["conflict"]["conflict_entry_id"], sibling);

    running.stop().await;
}
