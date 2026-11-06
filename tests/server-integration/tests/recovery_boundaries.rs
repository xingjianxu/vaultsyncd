//! S052 续接验收：启动补完在各发布边界上的真实重启行为。
//!
//! 每个用例都用真实服务端 + 真实 SQLite + btrfs 工作目录：用 SQLite 触发器（只作用于本用例的数据库）
//! 或目录只读权限制造“工作目录动作与元数据事务之间”的失败，再真实停止、重启：
//!
//! - **before 前像**（意图已落盘、尚未发布）→ 重启后重做同一操作；
//! - **after 后像**（已发布、元数据事务失败）→ 重启后补完，不产生第二个版本/副本/事件；
//! - **第三种内容 / 前像被外部改动**→ 不覆盖、不删除、不清证据，保持停写；现场还原后才补完；
//! - **意图损坏 / 缺少完整操作键关联**→ 保持停写；证据还原后同一操作可继续。

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::{
    Lab, Running, db, entry_named, epoch_of, identity, login, register_root, snapshot, submit,
    upload, wait_for,
};
use reqwest::StatusCode;
use serde_json::{Value, json};

const V1: &[u8] = b"version one\n";
const V2: &[u8] = b"version two uploaded by the client\n";
const THIRD: &[u8] = b"third content written by someone else\n";
const FAIL_COMMIT: &str = "CREATE TRIGGER s052_fail BEFORE UPDATE OF state ON operations WHEN NEW.state = 'committed' BEGIN SELECT RAISE(ABORT, 'S052 injected commit failure'); END";

struct Setup {
    lab: Lab,
    running: Running,
    token: String,
    root: String,
    epoch: String,
    root_entry: String,
}

async fn boot(label: &str, seed: impl FnOnce(&Path), adopted: &str) -> Setup {
    let lab = Lab::new(label, None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册");
    seed(&lab.work());
    wait_for(&format!("采纳 {adopted}"), || async {
        Ok(snapshot(&running, &token, &root)
            .await
            .iter()
            .any(|entry| entry["name"] == adopted))
    })
    .await
    .expect("采纳");
    let epoch = epoch_of(&running, &token, &root).await;
    let entries = snapshot(&running, &token, &root).await;
    let root_entry = entries
        .iter()
        .find(|entry| entry["parent_id"].is_null())
        .expect("根")["entry_id"]
        .as_str()
        .expect("id")
        .to_owned();
    Setup {
        lab,
        running,
        token,
        root,
        epoch,
        root_entry,
    }
}

impl Setup {
    async fn entry(&self, name: &str) -> (String, String) {
        let entries = snapshot(&self.running, &self.token, &self.root).await;
        let entry = entry_named(&entries, name);
        (
            entry["entry_id"].as_str().expect("id").to_owned(),
            entry["revision"].as_str().expect("revision").to_owned(),
        )
    }

    async fn submit(&self, body: Value) -> (StatusCode, Value) {
        submit(&self.running, &self.token, &self.root, body).await
    }

    /// 停止实例并释放所有权，返回实验目录等恢复阶段要用的东西。
    async fn stop(self) -> (Lab, String) {
        self.running.stop().await;
        (self.lab, self.root)
    }
}

fn lock_dir(dir: &Path) {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).expect("只读");
}

fn unlock_dir(dir: &Path) {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).expect("可写");
}

async fn assert_pending_intent(lab: &Lab) {
    // Root 准入仍完整可写；只锁发布目标的父目录，不让 upfront rwx 代替真实发布故障。
    assert_eq!(
        std::fs::metadata(lab.work()).unwrap().permissions().mode() & 0o700,
        0o700
    );
    let mut conn = db(lab).await;
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE phase != 'finalized'"
        )
        .await,
        1,
        "必须真实经过 Prepared 持久边界"
    );
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM operations WHERE state = 'pending'"
        )
        .await,
        1,
        "真实操作保留 pending"
    );
}

async fn scalar(conn: &mut sqlx::SqliteConnection, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&mut *conn)
        .await
        .expect("查询")
}

/// 只评估启动恢复（不开放监听）：返回恢复是否让所有 Root 都能开放写入。
async fn recovery_complete(lab: &Lab) -> bool {
    let service = vaultsyncd_server::bootstrap::prepare(lab.config())
        .await
        .expect("重启评估");
    let complete = service.assessment().is_complete();
    drop(service);
    complete
}

async fn external_events(conn: &mut sqlx::SqliteConnection) -> i64 {
    scalar(
        conn,
        "SELECT count(*) FROM changes WHERE events_json LIKE '%\"origin\":\"external\"%'",
    )
    .await
}

/// 补完后真实启动，同一请求必须重放为 committed，并返回该结果。
async fn replay_committed(lab: &Lab, root: &str, body: &Value) -> Value {
    let running = Running::start(lab).await.expect("补完后开放写入");
    let token = login(&running).await.expect("登录");
    let (status, replay) = submit(&running, &token, root, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["state"], "committed");
    assert_eq!(replay["replayed"], true);
    running.stop().await;
    replay
}

struct PutCase {
    setup: Setup,
    body: Value,
    entry: String,
    work_file: std::path::PathBuf,
}

/// 准备一次 put：note.txt 已被采纳为 V1，V2 已上传，随后构造请求体（尚未提交）。
async fn put_case(label: &str) -> PutCase {
    put_case_at(label, None).await
}

async fn put_case_at(label: &str, parent: Option<&str>) -> PutCase {
    let setup = boot(
        label,
        |work| {
            let directory = match parent {
                Some(name) => {
                    let directory = work.join(name);
                    std::fs::create_dir(&directory).expect("建发布父目录");
                    directory
                }
                None => work.to_owned(),
            };
            std::fs::write(directory.join("note.txt"), V1).expect("写 V1");
        },
        "note.txt",
    )
    .await;
    let (entry, revision) = setup.entry("note.txt").await;
    let (upload_id, _, _) = upload(&setup.running, &setup.token, &setup.root, V2, 32)
        .await
        .expect("上传 V2");
    let body = json!({
        "op_id": identity(0xd1), "device_id": identity(0x72), "server_epoch": setup.epoch,
        "kind": "put_content", "entry_id": entry, "base_revision": revision,
        "upload_id": upload_id, "local_generation": "7", "conflict_policy": "preserve_both",
    });
    let directory = parent.map_or_else(|| setup.lab.work(), |name| setup.lab.work().join(name));
    let work_file = directory.join("note.txt");
    PutCase {
        setup,
        body,
        entry,
        work_file,
    }
}

async fn assert_put_finished_once(lab: &Lab, work_file: &Path, replay: &Value) {
    assert_eq!(std::fs::read(work_file).expect("读"), V2);
    assert_eq!(
        replay["echo_local_generation"], "7",
        "补完后的重放仍回显本地代际"
    );
    let mut conn = db(lab).await;
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM changes WHERE events_json LIKE '%\"origin\":\"client\"%'"
        )
        .await,
        1,
        "只产生一个客户端事件批次"
    );
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM versions").await,
        2,
        "V1 + 补完的 V2，没有第三个版本"
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
async fn put_before_image_is_redone_after_restart() {
    let case = put_case_at("rb-put-before", Some("scope")).await;
    // Root 保持合规，仅发布父目录只读：意图落盘后实际发布失败，仍是 V1。
    lock_dir(case.work_file.parent().unwrap());
    let (status, failed) = case.setup.submit(case.body.clone()).await;
    unlock_dir(case.work_file.parent().unwrap());
    assert_ne!(status, StatusCode::OK, "{failed}");
    assert_pending_intent(&case.setup.lab).await;
    assert_eq!(std::fs::read(&case.work_file).expect("读"), V1);
    let mut conn = db(&case.setup.lab).await;
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE phase = 'prepared'"
        )
        .await,
        1
    );
    drop(conn);
    let (lab, root) = case.setup.stop().await;
    assert!(
        recovery_complete(&lab).await,
        "前像 + prepared 应重做并补完"
    );
    let replay = replay_committed(&lab, &root, &case.body).await;
    assert_put_finished_once(&lab, &case.work_file, &replay).await;
}

#[tokio::test]
async fn put_prepared_with_changed_before_image_stays_blocked_until_restored() {
    let case = put_case_at("rb-put-changed", Some("scope")).await;
    lock_dir(case.work_file.parent().unwrap());
    let (status, _) = case.setup.submit(case.body.clone()).await;
    unlock_dir(case.work_file.parent().unwrap());
    assert_ne!(status, StatusCode::OK);
    assert_pending_intent(&case.setup.lab).await;
    let (lab, root) = case.setup.stop().await;
    // 前像被外部改成第三种内容：不能用 V2 覆盖它。
    std::fs::write(&case.work_file, THIRD).expect("外部改写");
    assert!(!recovery_complete(&lab).await, "前像已变，必须停写");
    assert_eq!(
        std::fs::read(&case.work_file).expect("读"),
        THIRD,
        "不覆盖第三种内容"
    );
    let mut conn = db(&lab).await;
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE phase = 'prepared'"
        )
        .await,
        1,
        "证据保留"
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
    // 现场回到前像后，同一操作继续。
    std::fs::write(&case.work_file, V1).expect("还原");
    assert!(recovery_complete(&lab).await);
    let replay = replay_committed(&lab, &root, &case.body).await;
    assert_put_finished_once(&lab, &case.work_file, &replay).await;
}

#[tokio::test]
async fn put_after_image_completes_and_third_content_blocks() {
    let case = put_case("rb-put-after").await;
    let mut conn = db(&case.setup.lab).await;
    sqlx::query(FAIL_COMMIT)
        .execute(&mut conn)
        .await
        .expect("故障");
    let (status, failed) = case.setup.submit(case.body.clone()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{failed}");
    assert_eq!(
        std::fs::read(&case.work_file).expect("读"),
        V2,
        "已发布后像"
    );
    sqlx::query("DROP TRIGGER s052_fail")
        .execute(&mut conn)
        .await
        .expect("移除");
    drop(conn);
    let (lab, root) = case.setup.stop().await;
    // 后像之后被改成第三种内容：不能把它记成 V2 已提交。
    std::fs::write(&case.work_file, THIRD).expect("外部改写");
    assert!(!recovery_complete(&lab).await);
    assert_eq!(std::fs::read(&case.work_file).expect("读"), THIRD);
    std::fs::write(&case.work_file, V2).expect("还原");
    assert!(recovery_complete(&lab).await);
    let replay = replay_committed(&lab, &root, &case.body).await;
    assert_put_finished_once(&lab, &case.work_file, &replay).await;
}

#[tokio::test]
async fn corrupted_or_unlinked_intent_keeps_writes_closed_without_losing_evidence() {
    let case = put_case("rb-put-corrupt").await;
    let mut conn = db(&case.setup.lab).await;
    sqlx::query(FAIL_COMMIT)
        .execute(&mut conn)
        .await
        .expect("故障");
    let (status, _) = case.setup.submit(case.body.clone()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    sqlx::query("DROP TRIGGER s052_fail")
        .execute(&mut conn)
        .await
        .expect("移除");
    let original: String = sqlx::query_scalar("SELECT desired_after FROM fs_intents")
        .fetch_one(&mut conn)
        .await
        .expect("原后像");
    drop(conn);
    let (lab, root) = case.setup.stop().await;

    // 损坏：后像 JSON 合法但不是意图证据形状。
    let mut conn = db(&lab).await;
    sqlx::query("UPDATE fs_intents SET desired_after = '{\"x\":1}'")
        .execute(&mut conn)
        .await
        .expect("损坏");
    assert!(!recovery_complete(&lab).await, "损坏的意图不能被猜测或清除");
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE desired_after = '{\"x\":1}'"
        )
        .await,
        1
    );
    assert_eq!(std::fs::read(&case.work_file).expect("读"), V2);
    sqlx::query("UPDATE fs_intents SET desired_after = ?1")
        .bind(&original)
        .execute(&mut conn)
        .await
        .expect("还原");

    // 缺少完整操作键关联：不靠最新操作行推断。
    let link: (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) =
        sqlx::query_as("SELECT intent_id, root_id, device_id, op_id FROM intent_operations")
            .fetch_one(&mut conn)
            .await
            .expect("关联");
    sqlx::query("DELETE FROM intent_operations")
        .execute(&mut conn)
        .await
        .expect("删关联");
    assert!(!recovery_complete(&lab).await, "旧的未关联意图不能猜关联");
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM operations WHERE state = 'pending'"
        )
        .await,
        1
    );
    sqlx::query(
        "INSERT INTO intent_operations(intent_id,root_id,device_id,op_id) VALUES (?1,?2,?3,?4)",
    )
    .bind(link.0)
    .bind(link.1)
    .bind(link.2)
    .bind(link.3)
    .execute(&mut conn)
    .await
    .expect("还原关联");
    drop(conn);
    assert!(recovery_complete(&lab).await);
    let replay = replay_committed(&lab, &root, &case.body).await;
    assert_put_finished_once(&lab, &case.work_file, &replay).await;
    let _ = case.entry;
}

#[tokio::test]
async fn namespace_before_and_third_state_across_restart() {
    let setup = boot(
        "rb-create",
        |work| {
            std::fs::create_dir(work.join("scope")).expect("scope");
            std::fs::write(work.join("scope/seed.txt"), V1).expect("seed");
        },
        "seed.txt",
    )
    .await;
    let (parent, _) = setup.entry("scope").await;
    let directory = setup.lab.work().join("scope");
    let made = directory.join("made");
    let body = json!({
        "op_id": identity(0xd2), "device_id": identity(0x72), "server_epoch": setup.epoch,
        "kind": "create", "parent_id": parent, "name": "made",
        "expected_absent": true, "local_generation": "5",
    });
    // 前像：意图已落盘，mkdir 因只读失败，目录不存在。
    lock_dir(&directory);
    let (status, failed) = setup.submit(body.clone()).await;
    unlock_dir(&directory);
    assert_ne!(status, StatusCode::CREATED, "{failed}");
    assert_ne!(status, StatusCode::OK, "{failed}");
    assert!(!made.exists());
    assert_pending_intent(&setup.lab).await;
    let (lab, root) = setup.stop().await;
    // 第三种现场：同名位置现在是一个普通文件（外部创建）：不能当成目录已建好。
    std::fs::write(&made, THIRD).expect("外部同名文件");
    assert!(!recovery_complete(&lab).await);
    assert_eq!(std::fs::read(&made).expect("读"), THIRD);
    std::fs::remove_file(&made).expect("还原空缺");
    assert!(recovery_complete(&lab).await);
    let replay = replay_committed(&lab, &root, &body).await;
    assert!(made.is_dir());
    assert_eq!(replay["echo_local_generation"], "5", "补完后仍回显本地代际");
    let mut conn = db(&lab).await;
    assert_eq!(scalar(&mut conn, "SELECT count(*) FROM changes WHERE events_json LIKE '%\"made\"%' OR events_json LIKE '%made%'").await, 1);
}

#[tokio::test]
async fn delete_before_image_is_redone_but_changed_content_is_never_deleted() {
    let setup = boot(
        "rb-delete",
        |work| {
            std::fs::create_dir(work.join("scope")).expect("scope");
            std::fs::write(work.join("scope/doomed.txt"), V1).expect("doomed");
        },
        "doomed.txt",
    )
    .await;
    let (entry, revision) = setup.entry("doomed.txt").await;
    let directory = setup.lab.work().join("scope");
    let doomed = directory.join("doomed.txt");
    let body = json!({
        "op_id": identity(0xd3), "device_id": identity(0x72), "server_epoch": setup.epoch,
        "kind": "delete", "entry_id": entry, "base_revision": revision, "local_generation": "6",
    });
    lock_dir(&directory);
    let (status, failed) = setup.submit(body.clone()).await;
    unlock_dir(&directory);
    assert_ne!(status, StatusCode::OK, "{failed}");
    assert!(doomed.exists());
    assert_pending_intent(&setup.lab).await;
    let (lab, root) = setup.stop().await;
    // 删除前像之后文件被外部改写：删除会丢掉新内容，必须停写保留。
    std::fs::write(&doomed, THIRD).expect("外部改写");
    assert!(!recovery_complete(&lab).await);
    assert_eq!(
        std::fs::read(&doomed).expect("读"),
        THIRD,
        "改写后的内容不得被删除"
    );
    std::fs::write(&doomed, V1).expect("还原");
    assert!(recovery_complete(&lab).await);
    let replay = replay_committed(&lab, &root, &body).await;
    assert!(!doomed.exists());
    assert_eq!(replay["echo_local_generation"], "6");
    let mut conn = db(&lab).await;
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM trash_entries").await,
        1
    );
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM versions").await,
        1,
        "删除不动版本与内容"
    );
}

#[tokio::test]
async fn version_restore_and_conflict_complete_once_after_commit_failure() {
    let case = put_case("rb-restore-conflict").await;
    let setup = &case.setup;
    // 先提交 V2，得到 v1/v2 两个版本与新的 revision。
    let (status, put) = setup.submit(case.body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let entries = snapshot(&setup.running, &setup.token, &setup.root).await;
    let note = entry_named(&entries, "note.txt").clone();
    let mut conn = db(&setup.lab).await;
    let v1: String =
        sqlx::query_scalar("SELECT lower(hex(id)) FROM versions ORDER BY rowid LIMIT 1")
            .fetch_one(&mut conn)
            .await
            .expect("v1");

    // 历史恢复在提交失败后重启补完。
    sqlx::query(FAIL_COMMIT)
        .execute(&mut conn)
        .await
        .expect("故障");
    let restore = json!({
        "op_id": identity(0xd4), "device_id": identity(0x72), "server_epoch": setup.epoch,
        "kind": "restore", "entry_id": case.entry, "base_revision": put["revision"],
        "version_id": v1, "local_generation": "8",
    });
    let (status, failed) = setup.submit(restore.clone()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{failed}");
    assert_eq!(std::fs::read(&case.work_file).expect("读"), V1);
    sqlx::query("DROP TRIGGER s052_fail")
        .execute(&mut conn)
        .await
        .expect("移除");
    drop(conn);
    let work_file = case.work_file.clone();
    let body = case.body.clone();
    let entry = case.entry.clone();
    let (lab, root) = case.setup.stop().await;
    assert!(recovery_complete(&lab).await);
    let replay = replay_committed(&lab, &root, &restore).await;
    assert_eq!(replay["entry_id"], entry);
    assert_eq!(replay["echo_local_generation"], "8");
    assert_eq!(std::fs::read(&work_file).expect("读"), V1);
    let mut conn = db(&lab).await;
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM versions").await,
        3,
        "V1、V2 与一个恢复版本"
    );
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM changes WHERE events_json LIKE '%\"kind\":\"restore\"%'"
        )
        .await,
        1
    );
    let _ = (body, note);
    drop(conn);
    // 补完后的历史版本逐个下载：字节与内容哈希都对应各自版本，不混版。
    let mut conn = db(&lab).await;
    let versions: Vec<String> =
        sqlx::query_scalar("SELECT lower(hex(id)) FROM versions ORDER BY rowid")
            .fetch_all(&mut conn)
            .await
            .expect("版本");
    drop(conn);
    let running = Running::start(&lab).await.expect("启动");
    let token = login(&running).await.expect("登录");
    for (version, expected) in versions.iter().zip([V1, V2, V1]) {
        let (status, bytes, _) = running
            .download(
                &format!("/v1/roots/{root}/entries/{entry}/versions/{version}/content"),
                &token,
            )
            .await
            .expect("下载");
        assert_eq!(status, StatusCode::OK, "版本 {version}");
        assert_eq!(bytes, expected, "版本 {version} 的字节");
    }
    running.stop().await;
}

#[tokio::test]
async fn conflict_sibling_survives_commit_failure_and_is_completed_once() {
    let case = put_case("rb-conflict").await;
    let setup = &case.setup;
    // head 前进：外部改写被采纳，客户端基线过期。
    std::fs::write(&case.work_file, b"server head advanced\n").expect("改写");
    let stale = case.body["base_revision"].clone();
    wait_for("head 前进", || async {
        let entries = snapshot(&setup.running, &setup.token, &setup.root).await;
        Ok(entries
            .iter()
            .any(|e| e["entry_id"] == case.entry.as_str() && e["revision"] != stale))
    })
    .await
    .expect("前进");
    let mut conn = db(&setup.lab).await;
    sqlx::query(FAIL_COMMIT)
        .execute(&mut conn)
        .await
        .expect("故障");
    let (status, failed) = setup.submit(case.body.clone()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{failed}");
    let siblings: Vec<_> = std::fs::read_dir(setup.lab.work())
        .expect("列目录")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("note") && name != "note.txt")
        .collect();
    assert_eq!(siblings.len(), 1, "冲突副本已按持久身份发布：{siblings:?}");
    sqlx::query("DROP TRIGGER s052_fail")
        .execute(&mut conn)
        .await
        .expect("移除");
    drop(conn);
    let body = case.body.clone();
    let work = case.setup.lab.work();
    let (lab, root) = case.setup.stop().await;
    assert!(recovery_complete(&lab).await);
    let replay = replay_committed(&lab, &root, &body).await;
    assert_eq!(replay["conflict"]["reason"], "stale_base");
    let after: Vec<_> = std::fs::read_dir(&work)
        .expect("列目录")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("note") && name != "note.txt")
        .collect();
    assert_eq!(after, siblings, "补完与重试都不产生第二个副本");
    assert_eq!(
        std::fs::read(work.join("note.txt")).expect("读"),
        b"server head advanced\n",
        "head 不被覆盖"
    );
    let mut conn = db(&lab).await;
    assert_eq!(scalar(&mut conn, "SELECT count(*) FROM conflicts").await, 1);
    drop(conn);
    // 冲突副本的内容可按其当前版本下载，且就是迟到的 V2。
    let sibling = replay["conflict"]["conflict_entry_id"]
        .as_str()
        .expect("副本")
        .to_owned();
    let running = Running::start(&lab).await.expect("启动");
    let token = login(&running).await.expect("登录");
    let entries = snapshot(&running, &token, &root).await;
    let version = entries
        .iter()
        .find(|entry| entry["entry_id"] == sibling.as_str())
        .expect("副本在清单里")["version_id"]
        .as_str()
        .expect("版本")
        .to_owned();
    let (status, bytes, _) = running
        .download(
            &format!("/v1/roots/{root}/entries/{sibling}/versions/{version}/content"),
            &token,
        )
        .await
        .expect("下载");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, V2, "副本保存的是迟到内容");
    running.stop().await;
}

#[tokio::test]
async fn delete_refuses_to_drop_content_the_observer_has_not_adopted() {
    let setup = boot(
        "rb-delete-guard",
        |work| {
            std::fs::write(work.join("lone.txt"), V1).expect("lone");
            std::fs::create_dir_all(work.join("box")).expect("box");
            std::fs::write(work.join("box/in.txt"), V1).expect("in");
        },
        "in.txt",
    )
    .await;
    let (lone, lone_rev) = setup.entry("lone.txt").await;
    let (boxed, box_rev) = setup.entry("box").await;
    // 文件被原地改写（尚未采纳）：删除不得丢掉新内容。
    std::fs::write(setup.lab.work().join("lone.txt"), THIRD).expect("改写");
    let (status, response) = setup
        .submit(json!({
            "op_id": identity(0xd5), "device_id": identity(0x72), "server_epoch": setup.epoch,
            "kind": "delete", "entry_id": lone, "base_revision": lone_rev,
        }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{response}");
    assert_eq!(
        std::fs::read(setup.lab.work().join("lone.txt")).expect("读"),
        THIRD
    );

    // 目录里出现尚未采纳的新文件：目录删除同样拒绝。
    std::fs::write(setup.lab.work().join("box/extra.txt"), b"x").expect("新文件");
    let (_, subtree) =
        common::subtree_token(&setup.running, &setup.token, &setup.root, &boxed).await;
    let (status, response) = setup
        .submit(json!({
            "op_id": identity(0xd6), "device_id": identity(0x72), "server_epoch": setup.epoch,
            "kind": "delete", "entry_id": boxed, "base_revision": box_rev,
            "subtree_token": subtree["token"],
        }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{response}");
    assert!(setup.lab.work().join("box/extra.txt").exists());
    assert!(setup.lab.work().join("box/in.txt").exists());
    let mut conn = db(&setup.lab).await;
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM trash_entries").await,
        0
    );
    assert_eq!(
        scalar(&mut conn, "SELECT count(*) FROM fs_intents").await,
        0,
        "拒绝发生在意图之前"
    );
    setup.stop().await;
}

#[tokio::test]
async fn own_commands_never_come_back_as_external_changes() {
    let setup = boot(
        "rb-own-write",
        |work| {
            std::fs::create_dir_all(work.join("dir1/sub")).expect("dir1");
            std::fs::write(work.join("dir1/file.txt"), V1).expect("file");
            std::fs::write(work.join("dir1/sub/deep.txt"), V2).expect("deep");
        },
        "deep.txt",
    )
    .await;
    let mut conn = db(&setup.lab).await;
    let baseline = external_events(&mut conn).await;
    assert!(baseline >= 1, "初始外部采纳已发生");
    let device = identity(0x72);
    let (dir1, _) = setup.entry("dir1").await;
    let (file, file_rev) = setup.entry("file.txt").await;

    // create / move / delete(file) / restore(file) / delete(dir) / restore(dir 整组)。
    let (status, made) = setup
        .submit(json!({
            "op_id": identity(0xe1), "device_id": device, "server_epoch": setup.epoch,
            "kind": "create", "parent_id": dir1, "name": "made", "expected_absent": true,
        }))
        .await;
    assert_eq!(status, StatusCode::OK, "{made}");
    let (status, moved) = setup
        .submit(json!({
            "op_id": identity(0xe2), "device_id": device, "server_epoch": setup.epoch,
            "kind": "move", "entry_id": file, "base_revision": file_rev,
            "parent_id": dir1, "name": "renamed.txt",
        }))
        .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    let (status, deleted) = setup
        .submit(json!({
            "op_id": identity(0xe3), "device_id": device, "server_epoch": setup.epoch,
            "kind": "delete", "entry_id": file, "base_revision": moved["revision"],
        }))
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let (status, restored) = setup
        .submit(json!({
            "op_id": identity(0xe4), "device_id": device, "server_epoch": setup.epoch,
            "kind": "restore", "entry_id": file, "base_revision": deleted["revision"],
        }))
        .await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    let entries = snapshot(&setup.running, &setup.token, &setup.root).await;
    let current = entry_named(&entries, "dir1")["revision"].clone();
    let (_, subtree) =
        common::subtree_token(&setup.running, &setup.token, &setup.root, &dir1).await;
    let (status, deleted) = setup
        .submit(json!({
            "op_id": identity(0xe5), "device_id": device, "server_epoch": setup.epoch,
            "kind": "delete", "entry_id": dir1, "base_revision": current,
            "subtree_token": subtree["token"],
        }))
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert!(!setup.lab.work().join("dir1").exists());
    let (status, restored) = setup
        .submit(json!({
            "op_id": identity(0xe6), "device_id": device, "server_epoch": setup.epoch,
            "kind": "restore", "entry_id": dir1, "base_revision": deleted["revision"],
        }))
        .await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    assert!(setup.lab.work().join("dir1/sub/deep.txt").is_file());

    // 观察循环至少再跑几轮：自己的动作不能被当成外部变化重新采纳。
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert_eq!(
        external_events(&mut conn).await,
        baseline,
        "命令结果没有以 origin=external 重新出现"
    );
    let entries = snapshot(&setup.running, &setup.token, &setup.root).await;
    for name in ["dir1", "sub", "deep.txt", "renamed.txt", "made"] {
        assert_eq!(
            entries.iter().filter(|entry| entry["name"] == name).count(),
            1,
            "{name} 只有一个活跃条目"
        );
    }
    assert_eq!(
        entry_named(&entries, "renamed.txt")["entry_id"],
        file.as_str(),
        "身份没有被重新采纳成新条目"
    );
    setup.stop().await;
}

#[tokio::test]
async fn epoch_and_retention_failures_keep_their_wire_codes() {
    let setup = boot(
        "rb-typed-errors",
        |work| std::fs::write(work.join("note.txt"), V1).expect("note"),
        "note.txt",
    )
    .await;
    let (note, revision) = setup.entry("note.txt").await;
    // 请求的 epoch 不是当前 epoch：create / delete / restore 都是 410 EPOCH_CHANGED。
    let wrong = "999";
    for (op, body) in [
        (
            0xf1,
            json!({"kind": "create", "parent_id": setup.root_entry, "name": "x", "expected_absent": true}),
        ),
        (
            0xf2,
            json!({"kind": "delete", "entry_id": note, "base_revision": revision}),
        ),
        (
            0xf3,
            json!({"kind": "restore", "entry_id": note, "base_revision": revision}),
        ),
    ] {
        let mut body = body;
        body["op_id"] = json!(identity(op));
        body["device_id"] = json!(identity(0x72));
        body["server_epoch"] = json!(wrong);
        let (status, response) = setup.submit(body).await;
        assert_eq!(status, StatusCode::GONE, "{response}");
        assert_eq!(response["error"]["code"], "EPOCH_CHANGED", "{response}");
    }
    assert!(
        setup.lab.work().join("note.txt").exists(),
        "没有任何动作发生"
    );

    // 回收站保留期已过：410 VERSION_GONE，而不是笼统的基线不符。
    let (status, deleted) = setup
        .submit(json!({
            "op_id": identity(0xf4), "device_id": identity(0x72), "server_epoch": setup.epoch,
            "kind": "delete", "entry_id": note, "base_revision": revision,
        }))
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let mut conn = db(&setup.lab).await;
    sqlx::query("UPDATE trash_entries SET deleted_at = 0, expires_at = 1")
        .execute(&mut conn)
        .await
        .expect("过期");
    let (status, response) = setup
        .submit(json!({
            "op_id": identity(0xf5), "device_id": identity(0x72), "server_epoch": setup.epoch,
            "kind": "restore", "entry_id": note, "base_revision": deleted["revision"],
        }))
        .await;
    assert_eq!(status, StatusCode::GONE, "{response}");
    assert_eq!(response["error"]["code"], "VERSION_GONE", "{response}");
    assert!(
        !setup.lab.work().join("note.txt").exists(),
        "过期内容不被放回"
    );
    assert_eq!(
        scalar(
            &mut conn,
            "SELECT count(*) FROM fs_intents WHERE phase != 'finalized'"
        )
        .await,
        0
    );
    setup.stop().await;
}
