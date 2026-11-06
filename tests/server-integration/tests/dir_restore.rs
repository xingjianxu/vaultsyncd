//! S052 续接验收：回收站目录整组恢复与启动补完（真实 SQLite + btrfs 工作目录 + 真实重启）。
//!
//! 1. 整组范围由删除批次归属决定：同一路径下另一轮删除的条目不会被旧路径前缀选入；
//! 2. 原父目录在删除后被移动：恢复位置跟随它**现在**的路径；
//! 3. 恢复中途在两个真实边界失败（文件意图提交前、已发布但阶段推进前）：重启后同一操作被补完，
//!    不产生第二副本；现场被改成第三种内容或混入计划外内容时保持停写并保留证据，修复后才补完；
//! 4. 原名称已被新条目占用：另名恢复，不合并；工作目录里已有的未知目录在登记前被拒绝，不留 pending。

mod common;

use common::{
    Lab, Running, db, entry_named, epoch_of, identity, login, register_root, snapshot, submit,
    subtree_token, wait_for,
};
use reqwest::StatusCode;
use serde_json::{Value, json};

async fn adopted(running: &Running, token: &str, root: &str, name: &str) {
    wait_for(&format!("采纳 {name}"), || async {
        Ok(snapshot(running, token, root)
            .await
            .iter()
            .any(|entry| entry.get("name").and_then(Value::as_str) == Some(name)))
    })
    .await
    .expect("外部文件被采纳");
}

fn entry_id(entries: &[Value], name: &str) -> String {
    entry_named(entries, name)["entry_id"]
        .as_str()
        .expect("entry_id")
        .to_owned()
}

fn revision(entries: &[Value], name: &str) -> String {
    entry_named(entries, name)["revision"]
        .as_str()
        .expect("revision")
        .to_owned()
}

async fn delete_directory(
    running: &Running,
    token: &str,
    root: &str,
    epoch: &str,
    op: u8,
    entry: &str,
    revision: &str,
) -> Value {
    let (status, subtree) = subtree_token(running, token, root, entry).await;
    assert_eq!(status, StatusCode::OK, "{subtree}");
    let (status, deleted) = submit(
        running,
        token,
        root,
        json!({
            "op_id": identity(op),
            "device_id": identity(0x71),
            "server_epoch": epoch,
            "kind": "delete",
            "entry_id": entry,
            "base_revision": revision,
            "subtree_token": subtree["token"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    deleted
}

async fn scalar(conn: &mut sqlx::SqliteConnection, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&mut *conn)
        .await
        .expect("查询")
}

#[tokio::test]
async fn directory_group_restore_resumes_after_crash_without_second_copy() {
    let lab = Lab::new("dir-restore", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册");

    // 第 1 轮：area/proj/{keep.txt, sub/old.txt}。
    let sub_dir = lab.work().join("area/proj/sub");
    std::fs::create_dir_all(&sub_dir).expect("建目录");
    std::fs::write(lab.work().join("area/proj/keep.txt"), b"keep v1\n").expect("keep");
    std::fs::write(sub_dir.join("old.txt"), b"old round-1 content\n").expect("old");
    adopted(&running, &token, &root, "old.txt").await;
    let epoch = epoch_of(&running, &token, &root).await;
    let entries = snapshot(&running, &token, &root).await;
    let old_sub = entry_id(&entries, "sub");
    let old_file = entry_id(&entries, "old.txt");
    let proj = entry_id(&entries, "proj");
    let keep = entry_id(&entries, "keep.txt");
    let area = entry_id(&entries, "area");
    let root_entry = entries
        .iter()
        .find(|entry| entry["parent_id"].is_null())
        .expect("根")["entry_id"]
        .clone();

    // 删除 proj/sub（第 1 轮删除：sub、old.txt 进回收站）。
    delete_directory(
        &running,
        &token,
        &root,
        &epoch,
        0xa1,
        &old_sub,
        &revision(&entries, "sub"),
    )
    .await;
    assert!(!sub_dir.exists());

    // 同一路径下出现一个全新的 sub（新身份）和 new.txt，然后整个 proj 被删除（第 2 轮）。
    std::fs::create_dir_all(&sub_dir).expect("重建 sub");
    std::fs::write(sub_dir.join("new.txt"), b"new round-2 content\n").expect("new");
    adopted(&running, &token, &root, "new.txt").await;
    let entries = snapshot(&running, &token, &root).await;
    let new_sub = entries
        .iter()
        .find(|entry| entry["name"] == "sub")
        .expect("新 sub")["entry_id"]
        .as_str()
        .expect("id")
        .to_owned();
    assert_ne!(new_sub, old_sub, "新目录是新身份");
    let new_file = entry_id(&entries, "new.txt");
    let deleted = delete_directory(
        &running,
        &token,
        &root,
        &epoch,
        0xa2,
        &proj,
        &revision(&entries, "proj"),
    )
    .await;
    assert!(!lab.work().join("area/proj").exists());

    // 原父目录 area 被移动成 moved：恢复位置必须跟随它现在的路径。
    let (status, moved) = submit(
        &running,
        &token,
        &root,
        json!({
            "op_id": identity(0xa3),
            "device_id": identity(0x71),
            "server_epoch": epoch,
            "kind": "move",
            "entry_id": area,
            "base_revision": revision(&entries, "area"),
            "parent_id": root_entry,
            "name": "moved",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert!(lab.work().join("moved").is_dir());

    // 两个真实故障边界：keep.txt 正常；new.txt 发布后推进阶段失败。
    let mut conn = db(&lab).await;
    sqlx::query("CREATE TRIGGER s052_fail_advance BEFORE UPDATE OF phase ON fs_intents WHEN NEW.phase = 'published' AND NEW.kind = 'put_content' AND NEW.target_path LIKE '%new.txt' BEGIN SELECT RAISE(ABORT, 'S052 injected phase failure'); END")
        .execute(&mut conn)
        .await
        .expect("设置故障");
    let before_trash = scalar(&mut conn, "SELECT count(*) FROM trash_entries").await;
    assert_eq!(before_trash, 6, "两轮删除一共 2 + 4 条回收站清单");

    let restore = json!({
        "op_id": identity(0xa4),
        "device_id": identity(0x71),
        "server_epoch": epoch,
        "kind": "restore",
        "entry_id": proj,
        "base_revision": deleted["revision"],
    });
    let (status, failed) = submit(&running, &token, &root, restore.clone()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{failed}");
    let restored_dir = lab.work().join("moved/proj");
    assert!(restored_dir.join("keep.txt").is_file());
    assert!(restored_dir.join("sub/new.txt").is_file());
    assert!(
        !lab.work().join("area").exists() && !lab.work().join("moved/proj（恢复）").exists(),
        "位置跟随移动后的父目录，不生成旧路径或第二副本"
    );
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM operations WHERE state = 'pending'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE phase NOT IN ('finalized')"
        )
        .await,
        4,
        "两个目录意图 + keep + new 均未收尾"
    );
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM intent_operations i JOIN execution_journal j USING(root_id,device_id,op_id) WHERE j.kind = 'trash_restore'"
        )
        .await,
        4,
        "全部意图都与完整 (Root,Device,Op) 持久关联"
    );
    // 只有第 2 轮的 4 条被选入计划；第 1 轮的两条仍在回收站。
    let planned: String = sqlx::query_scalar(
        "SELECT context_json FROM execution_journal WHERE kind = 'trash_restore'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("计划");
    let plan: Value = serde_json::from_str(&planned).expect("计划 JSON");
    let ids: Vec<&str> = plan["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| entry["entry"].as_str().expect("id"))
        .collect();
    assert_eq!(ids.len(), 4, "{plan}");
    for expected in [&proj, &keep, &new_sub, &new_file] {
        assert!(ids.contains(&expected.as_str()), "计划应含 {expected}");
    }
    for stale in [&old_sub, &old_file] {
        assert!(
            !ids.contains(&stale.as_str()),
            "旧一轮的 {stale} 不得被选入"
        );
    }
    // 即便库没有故障，也不能绕过未完成操作继续发布。
    let (status, blocked) = submit(
        &running,
        &token,
        &root,
        json!({
            "op_id": identity(0xa5), "device_id": identity(0x71), "server_epoch": epoch,
            "kind": "create", "parent_id": root_entry, "name": "must-not-publish",
            "expected_absent": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{blocked}");
    assert!(!lab.work().join("must-not-publish").exists());
    drop(conn);
    running.stop().await;

    // 重启 1：故障仍在，补完继续失败，必须保持停写且证据不丢。
    let service = vaultsyncd_server::bootstrap::prepare(lab.config())
        .await
        .expect("重启评估");
    assert!(
        !service.assessment().is_complete(),
        "注入故障仍在，不能开放写入"
    );
    drop(service);
    assert!(restored_dir.join("sub/new.txt").is_file(), "证据保留");

    // 重启 2：现场被改成第三种内容 → 不覆盖、保持停写。
    let target = restored_dir.join("sub/new.txt");
    std::fs::write(&target, b"third content from someone else\n").expect("改写");
    let mut conn = db(&lab).await;
    sqlx::query("DROP TRIGGER s052_fail_advance")
        .execute(&mut conn)
        .await
        .expect("移除故障");
    let service = vaultsyncd_server::bootstrap::prepare(lab.config())
        .await
        .expect("重启评估");
    assert!(!service.assessment().is_complete(), "第三种内容必须停写");
    drop(service);
    assert_eq!(
        std::fs::read(&target).expect("读"),
        b"third content from someone else\n",
        "不覆盖第三种内容"
    );
    // 现场改回后混入计划外内容 → 仍停写，不合并。
    std::fs::write(&target, b"new round-2 content\n").expect("改回");
    std::fs::write(restored_dir.join("stranger.txt"), b"x").expect("计划外文件");
    let service = vaultsyncd_server::bootstrap::prepare(lab.config())
        .await
        .expect("重启评估");
    assert!(!service.assessment().is_complete(), "计划外内容必须停写");
    drop(service);
    assert!(restored_dir.join("stranger.txt").exists());
    std::fs::remove_file(restored_dir.join("stranger.txt")).expect("清理");

    // 重启 3：证据一致 → 补完同一操作。
    let service = vaultsyncd_server::bootstrap::prepare(lab.config())
        .await
        .expect("重启评估");
    assert!(
        service.assessment().is_complete(),
        "{:?}",
        service.assessment()
    );
    drop(service);
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE phase != 'finalized'"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM trash_entries").await,
        2,
        "只清掉本轮 4 条，第 1 轮仍在回收站"
    );
    let restore_events: i64 = scalar(
        &mut conn,
        "SELECT count(*) FROM changes WHERE events_json LIKE '%\"kind\":\"restore\"%'",
    )
    .await;
    assert_eq!(restore_events, 1, "补完与重试只产生一个恢复事件批次");
    drop(conn);

    let running = Running::start(&lab).await.expect("补完后开放写入");
    let token = login(&running).await.expect("登录");
    let (status, replay) = submit(&running, &token, &root, restore).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["state"], "committed");
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["entry_id"], proj, "原身份恢复");
    let entries = snapshot(&running, &token, &root).await;
    for (name, id) in [("proj", &proj), ("keep.txt", &keep), ("new.txt", &new_file)] {
        assert_eq!(entry_named(&entries, name)["entry_id"], id.as_str());
    }
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry["name"] == "sub")
            .map(|entry| entry["entry_id"].as_str().expect("id"))
            .collect::<Vec<_>>(),
        vec![new_sub.as_str()],
        "活跃的 sub 是第 2 轮的身份"
    );
    assert!(
        !entries.iter().any(|entry| entry["name"] == "old.txt"),
        "旧一轮条目仍在回收站，不会被带回"
    );
    assert_eq!(
        std::fs::read(restored_dir.join("keep.txt")).expect("读"),
        b"keep v1\n"
    );
    running.stop().await;
}

#[tokio::test]
async fn occupied_name_restores_under_alternate_name_and_foreign_directory_never_merges() {
    let lab = Lab::new("dir-restore-occupied", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册");
    std::fs::create_dir_all(lab.work().join("docs")).expect("docs");
    std::fs::write(lab.work().join("docs/a.txt"), b"first docs\n").expect("a");
    adopted(&running, &token, &root, "a.txt").await;
    let epoch = epoch_of(&running, &token, &root).await;
    let entries = snapshot(&running, &token, &root).await;
    let docs = entry_id(&entries, "docs");
    let deleted = delete_directory(
        &running,
        &token,
        &root,
        &epoch,
        0xb1,
        &docs,
        &revision(&entries, "docs"),
    )
    .await;

    // 新的同名目录被采纳（新身份）：原名称被活跃条目占用 → 另名恢复，不合并。
    std::fs::create_dir_all(lab.work().join("docs")).expect("新 docs");
    std::fs::write(lab.work().join("docs/b.txt"), b"second docs\n").expect("b");
    adopted(&running, &token, &root, "b.txt").await;
    let (status, restored) = submit(
        &running,
        &token,
        &root,
        json!({
            "op_id": identity(0xb2), "device_id": identity(0x71), "server_epoch": epoch,
            "kind": "restore", "entry_id": docs, "base_revision": deleted["revision"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    assert_eq!(restored["entry_id"], docs);
    let alternate = lab.work().join("docs（恢复）");
    assert_eq!(
        std::fs::read(alternate.join("a.txt")).expect("读"),
        b"first docs\n"
    );
    assert!(!alternate.join("b.txt").exists() && !lab.work().join("docs/a.txt").exists());
    assert_eq!(
        std::fs::read(lab.work().join("docs/b.txt")).expect("读"),
        b"second docs\n",
        "新目录原样保留，没有被合并"
    );

    // 工作目录里已存在但尚未采纳的目录：登记前拒绝，没有 pending 也没有意图。
    let entries = snapshot(&running, &token, &root).await;
    let alt_entry = entry_id(&entries, "docs（恢复）");
    let deleted = delete_directory(
        &running,
        &token,
        &root,
        &epoch,
        0xb3,
        &alt_entry,
        &revision(&entries, "docs（恢复）"),
    )
    .await;
    std::fs::create_dir_all(lab.work().join("docs（恢复）")).expect("占位目录");
    std::fs::write(lab.work().join("docs（恢复）/foreign.txt"), b"foreign\n").expect("外部");
    let (status, response) = submit(
        &running,
        &token,
        &root,
        json!({
            "op_id": identity(0xb4), "device_id": identity(0x71), "server_epoch": epoch,
            "kind": "restore", "entry_id": alt_entry, "base_revision": deleted["revision"],
        }),
    )
    .await;
    assert_eq!(
        std::fs::read(lab.work().join("docs（恢复）/foreign.txt")).expect("读"),
        b"foreign\n"
    );
    assert!(
        !lab.work().join("docs（恢复）/a.txt").exists(),
        "任何分支都不得把清单内容合并进外部目录"
    );
    if status == StatusCode::CONFLICT {
        assert_eq!(response["error"]["code"], "SUBTREE_CHANGED", "{response}");
        let mut conn = db(&lab).await;
        assert_eq!(
            scalar(
                &mut conn,
                "SELECT count(*) FROM operations WHERE hex(op_id) = upper('b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4')"
            )
            .await,
            0,
            "被拒绝的恢复没有 pending 记录"
        );
    } else {
        // 观察循环恰好先采纳了外部目录：此时原名被占用，走另名，也不合并。
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_ne!(response["entry_id"], Value::Null);
    }
    running.stop().await;
}

/// 注入点：`intent_insert` 让指定文件的意图创建失败（该文件尚未发布）；`commit` 让操作提交失败
/// （所有工作目录动作都已完成）。
async fn crashed_restore(
    label: &str,
    trigger: &str,
    also_adopted: &[&str],
    fixture: impl FnOnce(&std::path::Path),
) -> (Lab, String, Value, Value) {
    let lab = Lab::new(label, None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册");
    fixture(&lab.work());
    adopted(&running, &token, &root, "last.txt").await;
    for name in also_adopted {
        adopted(&running, &token, &root, name).await;
    }
    let epoch = epoch_of(&running, &token, &root).await;
    let entries = snapshot(&running, &token, &root).await;
    let top = entry_id(&entries, "top");
    let deleted = delete_directory(
        &running,
        &token,
        &root,
        &epoch,
        0xc1,
        &top,
        &revision(&entries, "top"),
    )
    .await;
    let mut conn = db(&lab).await;
    sqlx::query(trigger)
        .execute(&mut conn)
        .await
        .expect("设置故障");
    let restore = json!({
        "op_id": identity(0xc2), "device_id": identity(0x71), "server_epoch": epoch,
        "kind": "restore", "entry_id": top, "base_revision": deleted["revision"],
    });
    let (status, failed) = submit(&running, &token, &root, restore.clone()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{failed}");
    drop(conn);
    running.stop().await;
    (lab, root, restore, entries[0].clone())
}

async fn finish_after_restart(lab: &Lab, root: &str, restore: &Value) -> Value {
    let mut conn = db(lab).await;
    sqlx::query("DROP TRIGGER s052_fail")
        .execute(&mut conn)
        .await
        .expect("移除故障");
    let service = vaultsyncd_server::bootstrap::prepare(lab.config())
        .await
        .expect("重启评估");
    assert!(
        service.assessment().is_complete(),
        "{:?}",
        service.assessment()
    );
    drop(service);
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE phase != 'finalized'"
        )
        .await,
        0
    );
    let running = Running::start(lab).await.expect("启动");
    let token = login(&running).await.expect("登录");
    let (status, replay) = submit(&running, &token, root, restore.clone()).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["replayed"], true);
    running.stop().await;
    replay
}

#[tokio::test]
async fn restore_resumes_when_a_file_intent_was_never_created() {
    let trigger = "CREATE TRIGGER s052_fail BEFORE INSERT ON fs_intents WHEN NEW.kind = 'put_content' AND NEW.target_path LIKE '%last.txt' BEGIN SELECT RAISE(ABORT, 'S052 injected intent failure'); END";
    let (lab, root, restore, _) =
        crashed_restore("dir-restore-insert", trigger, &["first.txt"], |work| {
            std::fs::create_dir_all(work.join("top")).expect("top");
            std::fs::write(work.join("top/first.txt"), b"first\n").expect("first");
            std::fs::write(work.join("top/last.txt"), b"last\n").expect("last");
        })
        .await;
    assert_eq!(
        std::fs::read(lab.work().join("top/first.txt")).expect("读"),
        b"first\n"
    );
    assert!(
        !lab.work().join("top/last.txt").exists(),
        "最后一个文件没有意图就没有发布"
    );
    let replay = finish_after_restart(&lab, &root, &restore).await;
    assert_eq!(replay["state"], "committed");
    assert_eq!(
        std::fs::read(lab.work().join("top/last.txt")).expect("读"),
        b"last\n"
    );
    assert!(!lab.work().join("top（恢复）").exists(), "没有第二副本");
    let mut conn = db(&lab).await;
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE kind = 'put_content'"
        )
        .await,
        2
    );
}

#[tokio::test]
async fn restore_resumes_after_everything_was_published_but_the_commit_failed() {
    let trigger = "CREATE TRIGGER s052_fail BEFORE UPDATE OF state ON operations WHEN NEW.state = 'committed' AND NEW.root_seq IS NOT NULL BEGIN SELECT RAISE(ABORT, 'S052 injected commit failure'); END";
    let (lab, root, restore, _) =
        crashed_restore("dir-restore-commit", trigger, &["inner.txt"], |work| {
            std::fs::create_dir_all(work.join("top/mid")).expect("mid");
            std::fs::write(work.join("top/mid/inner.txt"), b"inner\n").expect("inner");
            std::fs::write(work.join("top/last.txt"), b"last\n").expect("last");
        })
        .await;
    assert!(
        lab.work().join("top/mid/inner.txt").is_file(),
        "子目录与文件都已建回"
    );
    let replay = finish_after_restart(&lab, &root, &restore).await;
    assert_eq!(replay["state"], "committed");
    assert!(lab.work().join("top/mid/inner.txt").is_file());
    assert_eq!(
        std::fs::read(lab.work().join("top/last.txt")).expect("读"),
        b"last\n"
    );
    let mut conn = db(&lab).await;
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM changes WHERE events_json LIKE '%\"kind\":\"restore\"%'"
        )
        .await,
        1
    );
}
