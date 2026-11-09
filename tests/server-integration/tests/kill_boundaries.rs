//! S052 续接验收：真实服务端**子进程**被 SIGKILL 后的重启补完。
//!
//! 与 `recovery_boundaries.rs` 的区别：那里用 SQLite 触发器让事务失败；这里让子进程在持久化边界上
//! **卡住**（触发器里跑一个很长的递归查询，写事务始终没有提交），父进程确认工作目录动作已发生后
//! 直接 `SIGKILL`。被杀进程的未提交事务由 SQLite 的 WAL 恢复丢弃，留下的就是真实的“已发布、
//! 元数据未提交”现场。随后移除触发器、重启并要求同一操作被补完，且只产生一份结果。

mod common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use common::{
    Lab, Running, db, entry_named, epoch_of, identity, login, register_vault, snapshot, submit,
    subtree_token, upload, wait_for,
};
use reqwest::StatusCode;
use serde_json::{Value, json};

const V1: &[u8] = b"version one\n";
const V2: &[u8] = b"version two uploaded by the client\n";
const HEAVY: &str = "SELECT count(*) FROM (WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 4000000000) SELECT x FROM c)";

fn stall_trigger(on: &str) -> String {
    format!("CREATE TRIGGER s052_stall {on} BEGIN {HEAVY}; END")
}

/// 在 `operations` 提交为 committed 时卡住：此刻工作目录动作都已完成。
fn stall_commit() -> String {
    stall_trigger("BEFORE UPDATE OF state ON operations WHEN NEW.state = 'committed'")
}

struct ServerChild {
    child: Child,
    running: Arc<Running>,
}

impl ServerChild {
    async fn spawn(lab: &Lab) -> Self {
        let _ = lab.config();
        let mut child = Command::new(env!("CARGO_BIN_EXE_server-child"))
            .arg(lab.config_path())
            .stdout(Stdio::piped())
            .spawn()
            .expect("启动服务端子进程");
        let stdout = child.stdout.take().expect("stdout");
        let line = tokio::task::spawn_blocking(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).map(|_| line)
        })
        .await
        .expect("读取任务")
        .expect("读取监听地址");
        let addr = line
            .trim()
            .strip_prefix("LISTEN ")
            .unwrap_or_else(|| panic!("子进程没有给出监听地址：{line:?}"))
            .to_owned();
        let running = Arc::new(Running::attach(format!("http://{addr}")));
        for _ in 0..100 {
            if let Ok((StatusCode::OK, _)) = running.json("GET", "/v1/health", None, None).await {
                return Self { child, running };
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("子进程健康探测超时");
    }

    /// SIGKILL 并回收；不给任何优雅停止的机会。
    fn kill(mut self) {
        self.child.kill().expect("SIGKILL");
        let status = self.child.wait().expect("回收");
        assert!(!status.success(), "子进程必须是被强杀的：{status:?}");
    }
}

impl Drop for ServerChild {
    fn drop(&mut self) {
        // 用例中途 panic 时也不留下子进程。
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn scalar(conn: &mut sqlx::SqliteConnection, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&mut *conn)
        .await
        .expect("查询")
}

async fn wait_file(path: &Path, expected: &[u8]) {
    for _ in 0..200 {
        if std::fs::read(path).is_ok_and(|bytes| bytes == expected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("等待 {path:?} 出现预期内容超时");
}

async fn wait_exists(path: &Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("等待 {path:?} 出现超时");
}

async fn wait_gone(path: &Path) {
    for _ in 0..200 {
        if !path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("等待 {path:?} 消失超时");
}

/// 真实重启评估：恢复必须让所有 Vault 都可以开放写入。
async fn recovery_complete(lab: &Lab) -> bool {
    let service = vaultsyncd_server::bootstrap::prepare(lab.config())
        .await
        .expect("重启评估");
    let complete = service.assessment().is_complete();
    drop(service);
    complete
}

async fn replay(lab: &Lab, vault: &str, body: &Value) -> Value {
    let running = Running::start(lab).await.expect("补完后开放写入");
    let token = login(&running).await.expect("登录");
    let (status, replay) = submit(&running, &token, vault, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["state"], "committed");
    assert_eq!(replay["replayed"], true);
    running.stop().await;
    replay
}

struct Prelude {
    lab: Lab,
    vault: String,
    epoch: String,
    vault_entry: String,
}

/// 进程内准备：注册 Vault、外部文件被采纳；调用方继续用 `running` 做前置命令，最后自行 stop。
async fn prelude(
    label: &str,
    seed: impl FnOnce(&Path),
    adopted: &[&str],
) -> (Prelude, Running, String) {
    let lab = Lab::new(label, None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动");
    let token = login(&running).await.expect("登录");
    let vault = register_vault(&running, &token, &lab.work())
        .await
        .expect("注册");
    seed(&lab.work());
    for name in adopted {
        wait_for(&format!("采纳 {name}"), || async {
            Ok(snapshot(&running, &token, &vault)
                .await
                .iter()
                .any(|entry| entry["name"] == *name))
        })
        .await
        .expect("采纳");
    }
    let epoch = epoch_of(&running, &token, &vault).await;
    let entries = snapshot(&running, &token, &vault).await;
    let vault_entry = entries
        .iter()
        .find(|entry| entry["parent_id"].is_null())
        .expect("根")["entry_id"]
        .as_str()
        .expect("id")
        .to_owned();
    (
        Prelude {
            lab,
            vault,
            epoch,
            vault_entry,
        },
        running,
        token,
    )
}

/// 在子进程里发请求（不等响应：它会被卡住），返回后台任务句柄。
fn fire(
    child: &ServerChild,
    token: String,
    vault: String,
    body: Value,
) -> tokio::task::JoinHandle<()> {
    let running = Arc::clone(&child.running);
    tokio::spawn(async move {
        let _ = running
            .json(
                "POST",
                &format!("/v1/vaults/{vault}/operations"),
                Some(&token),
                Some(&body),
            )
            .await;
    })
}

#[tokio::test]
async fn killed_put_between_publish_and_phase_advance_is_completed_once() {
    let (pre, running, token) = prelude(
        "kill-put",
        |work| std::fs::write(work.join("note.txt"), V1).expect("V1"),
        &["note.txt"],
    )
    .await;
    let entries = snapshot(&running, &token, &pre.vault).await;
    let note = entry_named(&entries, "note.txt");
    let (entry, revision) = (
        note["entry_id"].as_str().expect("id").to_owned(),
        note["revision"].as_str().expect("rev").to_owned(),
    );
    let (upload_id, _, _) = upload(&running, &token, &pre.vault, V2, 32)
        .await
        .expect("上传");
    running.stop().await;
    let body = json!({
        "op_id": identity(0x31), "device_id": identity(0x72), "server_epoch": pre.epoch,
        "kind": "put_content", "entry_id": entry, "base_revision": revision,
        "upload_id": upload_id, "local_generation": "7", "conflict_policy": "preserve_both",
    });

    // 卡在“意图推进到 published”：工作文件已经是 V2，意图还停在 prepared。
    let mut conn = db(&pre.lab).await;
    sqlx::query(&stall_trigger(
        "BEFORE UPDATE OF phase ON fs_intents WHEN NEW.phase = 'published' AND NEW.kind = 'put_content'",
    ))
    .execute(&mut conn)
    .await
    .expect("卡点");
    drop(conn);
    let child = ServerChild::spawn(&pre.lab).await;
    let token = login(&child.running).await.expect("登录");
    let _request = fire(&child, token, pre.vault.clone(), body.clone());
    wait_file(&pre.lab.work().join("note.txt"), V2).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    child.kill();

    let mut conn = db(&pre.lab).await;
    sqlx::query("DROP TRIGGER s052_stall")
        .execute(&mut conn)
        .await
        .expect("移除卡点");
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE phase = 'prepared'"
        )
        .await,
        1,
        "被杀时意图停在 prepared，而工作文件已是后像"
    );
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM operations WHERE state = 'pending'"
        )
        .await,
        1
    );
    drop(conn);
    assert!(recovery_complete(&pre.lab).await, "后像 + prepared 应补完");
    let result = replay(&pre.lab, &pre.vault, &body).await;
    assert_eq!(result["echo_local_generation"], "7");
    let mut conn = db(&pre.lab).await;
    assert_eq!(scalar(&mut conn, "SELECT count(*) FROM versions").await, 2);
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM changes WHERE events_json LIKE '%\"origin\":\"client\"%'"
        )
        .await,
        1
    );
    assert_eq!(
        std::fs::read(pre.lab.work().join("note.txt")).expect("读"),
        V2
    );
}

#[tokio::test]
async fn killed_directory_restore_at_commit_is_completed_without_a_second_copy() {
    let (pre, running, token) = prelude(
        "kill-dir-commit",
        |work| {
            std::fs::create_dir_all(work.join("top/mid")).expect("mid");
            std::fs::write(work.join("top/first.txt"), b"first\n").expect("first");
            std::fs::write(work.join("top/mid/inner.txt"), b"inner\n").expect("inner");
        },
        &["inner.txt", "first.txt"],
    )
    .await;
    let entries = snapshot(&running, &token, &pre.vault).await;
    let top = entry_named(&entries, "top");
    let top_id = top["entry_id"].as_str().expect("id").to_owned();
    let (_, subtree) = subtree_token(&running, &token, &pre.vault, &top_id).await;
    let (status, deleted) = submit(
        &running,
        &token,
        &pre.vault,
        json!({
            "op_id": identity(0x32), "device_id": identity(0x72), "server_epoch": pre.epoch,
            "kind": "delete", "entry_id": top_id, "base_revision": top["revision"],
            "subtree_token": subtree["token"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    running.stop().await;
    let restore = json!({
        "op_id": identity(0x33), "device_id": identity(0x72), "server_epoch": pre.epoch,
        "kind": "restore", "entry_id": top_id, "base_revision": deleted["revision"],
        "local_generation": "4",
    });

    let mut conn = db(&pre.lab).await;
    sqlx::query(&stall_commit())
        .execute(&mut conn)
        .await
        .expect("卡点");
    drop(conn);
    let child = ServerChild::spawn(&pre.lab).await;
    let token = login(&child.running).await.expect("登录");
    let _request = fire(&child, token, pre.vault.clone(), restore.clone());
    wait_file(&pre.lab.work().join("top/mid/inner.txt"), b"inner\n").await;
    wait_file(&pre.lab.work().join("top/first.txt"), b"first\n").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    child.kill();

    let mut conn = db(&pre.lab).await;
    sqlx::query("DROP TRIGGER s052_stall")
        .execute(&mut conn)
        .await
        .expect("移除卡点");
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM operations WHERE state = 'pending'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM trash_entries").await,
        4,
        "元数据未提交，回收站清单仍在"
    );
    drop(conn);
    assert!(recovery_complete(&pre.lab).await);
    let result = replay(&pre.lab, &pre.vault, &restore).await;
    assert_eq!(result["echo_local_generation"], "4");
    let mut conn = db(&pre.lab).await;
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM trash_entries").await,
        0
    );
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM changes WHERE events_json LIKE '%\"kind\":\"restore\"%'"
        )
        .await,
        1
    );
    assert!(!pre.lab.work().join("top（恢复）").exists(), "没有第二副本");
    assert_eq!(
        std::fs::read(pre.lab.work().join("top/first.txt")).expect("读"),
        b"first\n"
    );
}

#[tokio::test]
async fn killed_directory_restore_midway_resumes_the_same_plan() {
    let (pre, running, token) = prelude(
        "kill-dir-midway",
        |work| {
            std::fs::create_dir_all(work.join("top")).expect("top");
            std::fs::write(work.join("top/first.txt"), b"first\n").expect("first");
            std::fs::write(work.join("top/last.txt"), b"last\n").expect("last");
        },
        &["last.txt", "first.txt"],
    )
    .await;
    let entries = snapshot(&running, &token, &pre.vault).await;
    let top = entry_named(&entries, "top");
    let top_id = top["entry_id"].as_str().expect("id").to_owned();
    let (_, subtree) = subtree_token(&running, &token, &pre.vault, &top_id).await;
    let (status, deleted) = submit(
        &running,
        &token,
        &pre.vault,
        json!({
            "op_id": identity(0x34), "device_id": identity(0x72), "server_epoch": pre.epoch,
            "kind": "delete", "entry_id": top_id, "base_revision": top["revision"],
            "subtree_token": subtree["token"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    running.stop().await;
    let restore = json!({
        "op_id": identity(0x35), "device_id": identity(0x72), "server_epoch": pre.epoch,
        "kind": "restore", "entry_id": top_id, "base_revision": deleted["revision"],
    });

    // 卡在第二个文件的意图创建：目录与第一个文件已在，第二个文件没有意图也没有发布。
    let mut conn = db(&pre.lab).await;
    sqlx::query(&stall_trigger(
        "BEFORE INSERT ON fs_intents WHEN NEW.kind = 'put_content' AND NEW.target_path LIKE '%last.txt'",
    ))
    .execute(&mut conn)
    .await
    .expect("卡点");
    drop(conn);
    let child = ServerChild::spawn(&pre.lab).await;
    let token = login(&child.running).await.expect("登录");
    let _request = fire(&child, token, pre.vault.clone(), restore.clone());
    wait_file(&pre.lab.work().join("top/first.txt"), b"first\n").await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(!pre.lab.work().join("top/last.txt").exists());
    child.kill();

    let mut conn = db(&pre.lab).await;
    sqlx::query("DROP TRIGGER s052_stall")
        .execute(&mut conn)
        .await
        .expect("移除卡点");
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE kind = 'put_content'"
        )
        .await,
        1,
        "只有第一个文件有意图"
    );
    drop(conn);
    assert!(
        recovery_complete(&pre.lab).await,
        "按持久计划补建第二个文件"
    );
    replay(&pre.lab, &pre.vault, &restore).await;
    assert_eq!(
        std::fs::read(pre.lab.work().join("top/last.txt")).expect("读"),
        b"last\n"
    );
    let mut conn = db(&pre.lab).await;
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE kind = 'put_content'"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE phase != 'finalized'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn killed_create_and_delete_at_commit_are_completed_by_restart() {
    let (pre, running, token) = prelude(
        "kill-ns",
        |work| std::fs::write(work.join("doomed.txt"), V1).expect("doomed"),
        &["doomed.txt"],
    )
    .await;
    let entries = snapshot(&running, &token, &pre.vault).await;
    let doomed = entry_named(&entries, "doomed.txt");
    let (doomed_id, doomed_rev) = (
        doomed["entry_id"].as_str().expect("id").to_owned(),
        doomed["revision"].as_str().expect("rev").to_owned(),
    );
    running.stop().await;
    let create = json!({
        "op_id": identity(0x36), "device_id": identity(0x72), "server_epoch": pre.epoch,
        "kind": "create", "parent_id": pre.vault_entry, "name": "made", "expected_absent": true,
        "local_generation": "2",
    });
    let delete = json!({
        "op_id": identity(0x37), "device_id": identity(0x72), "server_epoch": pre.epoch,
        "kind": "delete", "entry_id": doomed_id, "base_revision": doomed_rev,
        "local_generation": "3",
    });

    for (body, probe_exists, path) in [
        (create.clone(), true, pre.lab.work().join("made")),
        (delete.clone(), false, pre.lab.work().join("doomed.txt")),
    ] {
        let mut conn = db(&pre.lab).await;
        sqlx::query(&stall_commit())
            .execute(&mut conn)
            .await
            .expect("卡点");
        drop(conn);
        let child = ServerChild::spawn(&pre.lab).await;
        let token = login(&child.running).await.expect("登录");
        let _request = fire(&child, token, pre.vault.clone(), body.clone());
        if probe_exists {
            wait_exists(&path).await;
        } else {
            wait_gone(&path).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        child.kill();
        let mut conn = db(&pre.lab).await;
        sqlx::query("DROP TRIGGER s052_stall")
            .execute(&mut conn)
            .await
            .expect("移除卡点");
        assert_eq!(
            scalar(
                &mut conn,
                "SELECT count(*) FROM operations WHERE state = 'pending'"
            )
            .await,
            1
        );
        drop(conn);
        assert!(recovery_complete(&pre.lab).await);
        let result = replay(&pre.lab, &pre.vault, &body).await;
        assert_eq!(result["state"], "committed");
    }
    assert!(pre.lab.work().join("made").is_dir());
    assert!(!pre.lab.work().join("doomed.txt").exists());
    let mut conn = db(&pre.lab).await;
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM trash_entries").await,
        1
    );
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE phase != 'finalized'"
        )
        .await,
        0
    );
}
