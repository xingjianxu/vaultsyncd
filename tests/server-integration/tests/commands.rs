//! S052 续接验收：create / move / delete 命令的真实执行（JSON 事务 + 工作目录动作）。
//!
//! 与 `http_loop.rs` 一样启动真实监听的服务端（真实 SQLite + btrfs 工作目录），验证：
//!
//! 1. `create` 在父目录下建目录（条目、事件与操作结果同一事务；工作目录真的出现目录）；
//! 2. `move` 只改父关系与名称：`EntryId`、内容版本与字节都不变，工作目录里是一次 `rename`；
//! 3. `delete` 删文件（无 token）与目录（带服务端 `subtree` token）：工作目录内容消失、回收站清单
//!    与墓碑落库、版本行与对象保留、`changes` 有 delete 事件；
//! 4. 目录 token 在子树变化后失效（409 `SUBTREE_CHANGED`），不会删掉并发新增；
//! 5. 同一 OpId 的重试返回同一个结果（replayed），不同摘要返回 409 `OP_ID_REUSED`。

mod common;

use common::{
    Lab, Running, all_changes, create_snapshot, identity, login, register_vault, snapshot_entries,
    wait_for,
};
use reqwest::StatusCode;
use serde_json::{Value, json};

const CONTENT: &[u8] = b"note content for namespace commands\n";

async fn submit(running: &Running, token: &str, vault: &str, body: Value) -> (StatusCode, Value) {
    running
        .json(
            "POST",
            &format!("/v1/vaults/{vault}/operations"),
            Some(token),
            Some(&body),
        )
        .await
        .expect("提交操作")
}

fn entry_named<'a>(entries: &'a [Value], name: &str) -> &'a Value {
    entries
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
        .expect("条目存在")
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

async fn epoch_of(running: &Running, token: &str, vault: &str) -> String {
    create_snapshot(running, token, vault)
        .await
        .expect("快照")
        .get("server_epoch")
        .and_then(Value::as_str)
        .expect("epoch")
        .to_owned()
}

async fn subtree_token(
    running: &Running,
    token: &str,
    vault: &str,
    entry: &str,
) -> (StatusCode, Value) {
    running
        .json(
            "GET",
            &format!("/v1/vaults/{vault}/entries/{entry}/subtree"),
            Some(token),
            None,
        )
        .await
        .expect("取子树 token")
}

#[tokio::test]
async fn create_move_and_delete_are_executed_and_replay_stably() {
    let lab = Lab::new("namespace", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let vault = register_vault(&running, &token, &lab.work())
        .await
        .expect("注册 Vault");

    // 初始文件：外部写入 → 观察循环采纳（首个版本）。
    std::fs::write(lab.work().join("note.txt"), CONTENT).expect("写文件");
    wait_for("初始采纳", || async {
        let entries = snapshot(&running, &token, &vault).await;
        Ok(entries
            .iter()
            .any(|entry| entry.get("name").and_then(Value::as_str) == Some("note.txt")))
    })
    .await
    .expect("初始导入");
    let epoch = epoch_of(&running, &token, &vault).await;
    let entries = snapshot(&running, &token, &vault).await;
    let vault_entry = entries
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(""))
        .expect("根条目")["entry_id"]
        .as_str()
        .expect("根条目身份")
        .to_owned();
    let note = entry_named(&entries, "note.txt");
    let note_entry = note["entry_id"].as_str().expect("文件身份").to_owned();
    let note_version = note["version_id"].as_str().expect("版本").to_owned();
    let note_revision = note["revision"].as_str().expect("revision").to_owned();

    // 1. create：在根下建目录 docs。
    let device = identity(0x52);
    let (status, created) = submit(
        &running,
        &token,
        &vault,
        json!({
            "op_id": identity(0x41),
            "device_id": device,
            "server_epoch": epoch,
            "kind": "create",
            "parent_id": vault_entry,
            "name": "docs",
            "expected_absent": true,
            "local_generation": "3",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create 应提交：{created}");
    assert_eq!(created["state"], "committed");
    let docs_entry = created["entry_id"].as_str().expect("新目录身份").to_owned();
    assert!(
        lab.work().join("docs").is_dir(),
        "工作目录里必须真的出现 docs/（不是只写数据库）"
    );
    let entries = snapshot(&running, &token, &vault).await;
    assert_eq!(entry_named(&entries, "docs")["entry_id"], docs_entry);
    assert_eq!(entry_named(&entries, "docs")["kind"], "directory");

    // create 重试：同一 OpId 同一请求返回同一结果。
    let (status, replay) = submit(
        &running,
        &token,
        &vault,
        json!({
            "op_id": identity(0x41),
            "device_id": device,
            "server_epoch": epoch,
            "kind": "create",
            "parent_id": vault_entry,
            "name": "docs",
            "expected_absent": true,
            "local_generation": "3",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["entry_id"], docs_entry);

    // 2. move：把 note.txt 移进 docs/，身份与内容版本不变。
    let (status, moved) = submit(
        &running,
        &token,
        &vault,
        json!({
            "op_id": identity(0x42),
            "device_id": device,
            "server_epoch": epoch,
            "kind": "move",
            "entry_id": note_entry,
            "base_revision": note_revision,
            "parent_id": docs_entry,
            "name": "note.txt",
            "local_generation": "3",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "move 应提交：{moved}");
    assert_eq!(moved["entry_id"], note_entry, "move 不换 EntryId");
    assert!(
        lab.work().join("docs/note.txt").is_file(),
        "移动后工作目录里必须在 docs/note.txt"
    );
    assert!(
        !lab.work().join("note.txt").exists(),
        "源路径不得留下副本（同一次 rename）"
    );
    let entries = snapshot(&running, &token, &vault).await;
    let moved_note = entry_named(&entries, "note.txt");
    assert_eq!(moved_note["entry_id"], note_entry);
    assert_eq!(moved_note["version_id"], note_version, "移动不写新版本");
    assert_eq!(
        std::fs::read(lab.work().join("docs/note.txt")).expect("读文件"),
        CONTENT
    );

    // 3. 目录 token：docs 现在有 2 条（目录自身 + note.txt）。
    let (status, subtree) = subtree_token(&running, &token, &vault, &docs_entry).await;
    assert_eq!(status, StatusCode::OK, "{subtree}");
    assert_eq!(subtree["entries"], "2");
    let stale_token = subtree["token"].as_str().expect("token").to_owned();

    // 4. 文件删除不带 token：工作目录与命名空间都消失，但版本/回收站保留。
    let (status, deleted) = submit(
        &running,
        &token,
        &vault,
        json!({
            "op_id": identity(0x43),
            "device_id": device,
            "server_epoch": epoch,
            "kind": "delete",
            "entry_id": note_entry,
            "base_revision": moved["revision"],
            "local_generation": "3",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete 应提交：{deleted}");
    assert!(!lab.work().join("docs/note.txt").exists());
    let entries = snapshot(&running, &token, &vault).await;
    assert!(
        !entries
            .iter()
            .any(|entry| entry.get("entry_id").and_then(Value::as_str) == Some(&note_entry)),
        "软删除后条目不在活跃清单里"
    );
    let batches = all_changes(&running, &token, &vault)
        .await
        .expect("changes");
    assert!(
        batches.iter().any(|batch| {
            batch
                .get("events_json")
                .and_then(Value::as_str)
                .is_some_and(|events| {
                    events.contains("\"kind\":\"delete\"") && events.contains(&note_entry)
                })
        }),
        "删除必须进 changes 批次"
    );
    // 旧版本内容仍可下载（历史不因删除丢失）。
    let (status, bytes, _) = running
        .download(
            &format!("/v1/vaults/{vault}/entries/{note_entry}/versions/{note_version}/content"),
            &token,
        )
        .await
        .expect("下载历史版本");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, CONTENT, "删除不丢历史内容");

    // 5. 目录删除：先在 docs 里新增一个文件，让之前拿到的 token 失效。
    std::fs::write(lab.work().join("docs/new.txt"), b"concurrent addition\n").expect("写新文件");
    let (status, refused) = submit(
        &running,
        &token,
        &vault,
        json!({
            "op_id": identity(0x44),
            "device_id": device,
            "server_epoch": epoch,
            "kind": "delete",
            "entry_id": docs_entry,
            "base_revision": created["revision"],
            "subtree_token": stale_token,
            "local_generation": "3",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "过期 token 必须拒绝：{refused}"
    );
    // 观察循环已采纳新文件 → token 失效（SUBTREE_CHANGED）；尚未采纳 → 现场含冻结清单之外的内容
    // （RECONCILE_REQUIRED）。两者都要求客户端先刷新事实，且都不得动工作目录。
    assert!(
        ["SUBTREE_CHANGED", "RECONCILE_REQUIRED"]
            .contains(&refused["error"]["code"].as_str().unwrap_or_default()),
        "过期 token 的错误码：{refused}"
    );
    assert!(
        lab.work().join("docs/new.txt").is_file(),
        "被拒绝的删除不得动工作目录"
    );

    // 取新 token（等观察循环把新文件采纳后才稳定）再删。
    wait_for("新文件采纳", || async {
        let (status, value) = subtree_token(&running, &token, &vault, &docs_entry).await;
        Ok(status == StatusCode::OK && value["entries"] == "2")
    })
    .await
    .expect("新文件采纳");
    let (_, fresh) = subtree_token(&running, &token, &vault, &docs_entry).await;
    let fresh_token = fresh["token"].as_str().expect("token").to_owned();
    let entries = snapshot(&running, &token, &vault).await;
    let docs_revision = entry_named(&entries, "docs")["revision"]
        .as_str()
        .expect("revision")
        .to_owned();
    let (status, deleted_dir) = submit(
        &running,
        &token,
        &vault,
        json!({
            "op_id": identity(0x45),
            "device_id": device,
            "server_epoch": epoch,
            "kind": "delete",
            "entry_id": docs_entry,
            "base_revision": docs_revision,
            "subtree_token": fresh_token,
            "local_generation": "3",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "目录删除应提交：{deleted_dir}");
    assert!(!lab.work().join("docs").exists(), "目录树必须真的删掉");
    let (status, _) = subtree_token(&running, &token, &vault, &docs_entry).await;
    assert_ne!(status, StatusCode::OK, "已删除目录不再提供 token");

    // 6. 同一 OpId 不同请求 → OP_ID_REUSED（幂等键语义）。
    let (status, reused) = submit(
        &running,
        &token,
        &vault,
        json!({
            "op_id": identity(0x45),
            "device_id": device,
            "server_epoch": epoch,
            "kind": "delete",
            "entry_id": vault_entry,
            "base_revision": "0",
            "local_generation": "3",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{reused}");
    assert_eq!(reused["error"]["code"], "OP_ID_REUSED");

    running.stop().await;
}
