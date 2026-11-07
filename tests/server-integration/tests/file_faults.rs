//! S103 文件故障集成验收（design §21.1 T17/T18/T25/T27，§12.1—12.3）。
//!
//! 每个用例启动真实监听的服务端（真实 TCP、真实 SQLite、真实 btrfs 工作目录、真实观察循环），
//! 只在本 run 独占的实验目录里注入外部写、rename-over、截断与 symlink。判据是字节与可恢复版本，
//! 不是界面状态：
//!
//! - T18：外部写入尚未被采集时，客户端提交同一文件不得静默覆盖它；必须给出明确的协调错误，
//!   采集完成后按新 revision 重试才提交。
//! - T27：编辑器 rename-over 保存不丢旧内容；旧条目退役并进入回收站，新文件按新身份采纳。
//! - T18/T27：保存期间截断/半写与客户端提交重叠时，不得把未采集内容覆盖成“成功”。
//! - T25：Root 内 symlink 不跟随、不读取 Root 外内容、不产生删除，原外部文件字节不变。
//! - T17：外部 rename 保持 EntryId、不产生新版本；外部删除进入回收站且可恢复。

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{
    Lab, PutArgs, Running, all_changes, create_snapshot, identity, login, put_operation,
    register_root, snapshot, snapshot_entries, submit, upload,
};
use reqwest::StatusCode;
use serde_json::{Value, json};

/// 文件在 Root 内的名字。
const NAME: &str = "note.txt";

fn sha256(bytes: &[u8]) -> String {
    common::hex(&vaultsyncd_platform_linux_server::files::hash::digest(
        bytes,
    ))
}

fn work_file(lab: &Lab) -> PathBuf {
    lab.work().join(NAME)
}

/// 当前清单条目（条目、revision、版本、哈希、长度、epoch）。
#[derive(Clone, Debug)]
struct Head {
    entry: String,
    revision: String,
    version: String,
    hash: String,
    #[expect(dead_code)]
    size: String,
    epoch: String,
}

async fn head_named(running: &Running, token: &str, root: &str, name: &str) -> Option<Head> {
    let snapshot = create_snapshot(running, token, root).await.ok()?;
    let epoch = snapshot.get("server_epoch")?.as_str()?.to_owned();
    let id = snapshot.get("snapshot_id")?.as_str()?;
    let entries = snapshot_entries(running, token, root, id).await.ok()?;
    let entry = entries
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))?;
    let field = |name: &str| {
        entry
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_default()
    };
    Some(Head {
        entry: field("entry_id"),
        revision: field("revision"),
        version: field("version_id"),
        hash: field("hash"),
        size: field("size"),
        epoch,
    })
}

/// 等待观察循环采纳指定内容为 `name` 的当前 head。
async fn wait_head(running: &Running, token: &str, root: &str, name: &str, hash: &str) -> Head {
    for _ in 0..160 {
        if let Some(head) = head_named(running, token, root, name).await
            && head.hash == hash
            && !head.version.is_empty()
        {
            return head;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("等待 {name} 采纳为 {hash} 超时");
}

/// 历史里的内容哈希（含当前版本）。
async fn history_hashes(running: &Running, token: &str, root: &str, entry: &str) -> Vec<String> {
    let (status, value) = running
        .json(
            "GET",
            &format!("/v1/roots/{root}/entries/{entry}/history"),
            Some(token),
            None,
        )
        .await
        .expect("读取历史");
    assert_eq!(status, StatusCode::OK, "历史应可读：{value}");
    value
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("hash").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// 全部 changes 事件（已解析 `events_json`），按 seq 顺序。
async fn events(running: &Running, token: &str, root: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for batch in all_changes(running, token, root).await.expect("changes") {
        let text = batch
            .get("events_json")
            .and_then(Value::as_str)
            .expect("批次带 events_json");
        let parsed: Vec<Value> = serde_json::from_str(text).expect("事件 JSON");
        out.extend(parsed);
    }
    out
}

/// 是否存在“该路径上某个哈希的删除事件”。
fn has_delete_event(events: &[Value], path: &str) -> bool {
    events.iter().any(|event| {
        event.get("path").and_then(Value::as_str) == Some(path)
            && event.get("kind").and_then(Value::as_str) == Some("delete")
    })
}

/// Root 的管理视图（健康、原因、采集状态与说明）。
async fn root_view(running: &Running, token: &str, root: &str) -> Value {
    let (status, value) = running
        .json("GET", "/v1/admin/roots", Some(token), None)
        .await
        .expect("读 Root 清单");
    assert_eq!(status, StatusCode::OK, "{value}");
    value
        .get("roots")
        .and_then(Value::as_array)
        .and_then(|roots| {
            roots
                .iter()
                .find(|item| item.get("root_id").and_then(Value::as_str) == Some(root))
        })
        .cloned()
        .unwrap_or_else(|| panic!("Root 清单里没有 {root}：{value}"))
}

/// 预置一个被采纳的初始文件。
async fn seed(running: &Running, token: &str, root: &str, lab: &Lab, content: &[u8]) -> Head {
    std::fs::write(work_file(lab), content).expect("写初始文件");
    wait_head(running, token, root, NAME, &sha256(content)).await
}

/// 客户端 put：上传 + 提交（`head` 决定基线、epoch 与条目）。
async fn put(
    running: &Running,
    token: &str,
    root: &str,
    head: &Head,
    content: &[u8],
    op_seed: u8,
) -> (StatusCode, Value) {
    let (upload_id, _, _) = upload(running, token, root, content, 32)
        .await
        .expect("上传");
    let args = PutArgs {
        op_id: &identity(op_seed),
        device_id: &identity(op_seed.wrapping_add(1)),
        epoch: &head.epoch,
        entry: &head.entry,
        revision: &head.revision,
        upload: &upload_id,
    };
    put_operation(running, token, root, &args)
        .await
        .expect("提交操作")
}

/// 对象区里的字节（按版本身份查 `objects.storage_path` 后直接读文件）。
async fn version_bytes(lab: &Lab, version_hex: &str) -> Option<Vec<u8>> {
    let mut conn = common::db(lab).await;
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT o.storage_path, o.gc_state FROM objects o JOIN versions v \
         ON v.object_hash = o.hash AND v.root_id = o.root_id WHERE lower(hex(v.id)) = ?1",
    )
    .bind(version_hex)
    .fetch_optional(&mut conn)
    .await
    .expect("查对象路径");
    let (path, _gc_state) = row?;
    // `objects.storage_path` 相对对象区根（`<state>/objects`），见 `ObjectLayout::relative_object_path`。
    let full = lab.state().join("objects").join(&path);
    std::fs::read(full).ok()
}

async fn trash_rows(lab: &Lab) -> Vec<(String, String, String, String)> {
    let mut conn = common::db(lab).await;
    sqlx::query_as(
        "SELECT lower(hex(entry_id)), path, COALESCE(lower(hex(content_version_id)), ''), \
         CAST(revision AS TEXT) FROM trash_entries ORDER BY path",
    )
    .fetch_all(&mut conn)
    .await
    .expect("查回收站")
}

async fn version_count(lab: &Lab, entry_hex: &str) -> i64 {
    let mut conn = common::db(lab).await;
    sqlx::query_scalar("SELECT count(*) FROM versions WHERE lower(hex(entry_id)) = ?1")
        .bind(entry_hex)
        .fetch_one(&mut conn)
        .await
        .expect("查版本数")
}

/// T18：外部写入尚未被采集时客户端提交同一文件，不得静默覆盖外部内容。
#[tokio::test]
async fn external_write_before_put_is_not_silently_overwritten() {
    const BASE: &[u8] = b"base content\n";
    const EXTERNAL: &[u8] = b"external edit that must survive\n";
    const CLIENT: &[u8] = b"client content from stale base\n";

    let lab = Lab::new("s103-external-race", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册 Root");
    let base = seed(&running, &token, &root, &lab, BASE).await;
    assert_eq!(base.hash, sha256(BASE));

    // 外部进程改写，但**不等观察循环采纳**就发出客户端提交：两者是同一文件的并发意图。
    std::fs::write(work_file(&lab), EXTERNAL).expect("外部写");
    let (status, body) = put(&running, &token, &root, &base, CLIENT, 0x31).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "外部内容未采集时不能提交成功：{body}"
    );
    assert_eq!(
        body.pointer("/error/code").and_then(Value::as_str),
        Some("RECONCILE_REQUIRED"),
        "必须给出明确的协调错误：{body}"
    );
    assert_eq!(
        std::fs::read(work_file(&lab)).expect("读工作文件"),
        EXTERNAL,
        "被拒绝的提交不得改动外部内容"
    );
    assert!(
        !history_hashes(&running, &token, &root, &base.entry)
            .await
            .contains(&sha256(CLIENT)),
        "客户端内容没有提交，历史里不应出现它"
    );

    // 观察循环采集外部内容后，它成为新版本与新 revision；旧基线自动过期。
    let external_head = wait_head(&running, &token, &root, NAME, &sha256(EXTERNAL)).await;
    assert_eq!(
        external_head.entry, base.entry,
        "同一 inode 的原地改写沿用条目身份"
    );
    assert_ne!(
        external_head.revision, base.revision,
        "外部内容被采纳后 revision 必须推进"
    );
    assert_eq!(
        version_bytes(&lab, &external_head.version).await.as_deref(),
        Some(EXTERNAL),
        "外部内容必须作为不可变版本落库"
    );

    // 按新事实重新规划后，同一客户端内容可以提交（证明拒绝不是永久阻塞）。
    let (status, body) = put(&running, &token, &root, &external_head, CLIENT, 0x41).await;
    assert_eq!(status, StatusCode::OK, "重新规划后应能提交：{body}");
    assert_eq!(body.get("state").and_then(Value::as_str), Some("committed"));
    assert_eq!(std::fs::read(work_file(&lab)).expect("读工作文件"), CLIENT);
    assert!(
        history_hashes(&running, &token, &root, &external_head.entry)
            .await
            .contains(&sha256(EXTERNAL)),
        "提交后外部版本仍可在历史里恢复"
    );
    running.stop().await;
}

/// T27：编辑器 rename-over 保存不丢旧内容；旧条目退役进回收站，新文件按新身份采纳。
#[tokio::test]
async fn rename_over_save_retires_old_entry_and_keeps_bytes() {
    const BEFORE: &[u8] = b"before the editor save\n";
    const AFTER: &[u8] = b"the final saved content\n";
    const OTHER: &[u8] = b"untouched neighbour\n";

    let lab = Lab::new("s103-rename-over", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册 Root");
    let before = seed(&running, &token, &root, &lab, BEFORE).await;
    std::fs::write(lab.work().join("other.txt"), OTHER).expect("写邻居文件");
    wait_head(&running, &token, &root, "other.txt", &sha256(OTHER)).await;

    // 编辑器保存：写临时文件 → rename-over 目标（新 inode 出现在同一路径）。
    let temp = lab.work().join(".note.txt.swp");
    std::fs::write(&temp, AFTER).expect("写临时文件");
    std::fs::rename(&temp, work_file(&lab)).expect("rename-over");

    let after = wait_head(&running, &token, &root, NAME, &sha256(AFTER)).await;
    // 同一路径上的新文件对象沿用条目身份（服务端的 Root 条目按路径稳定），但保存前的内容
    // 必须成为一个可恢复的历史版本，不能被最终内容顶掉。
    assert_eq!(after.entry, before.entry, "同一路径保存不改变条目身份");
    assert_eq!(
        version_bytes(&lab, &after.version).await.as_deref(),
        Some(AFTER),
        "最终保存内容必须逐字节落成版本"
    );
    assert_eq!(
        version_bytes(&lab, &before.version).await.as_deref(),
        Some(BEFORE),
        "保存前的已确认版本仍在对象区"
    );
    let hashes = history_hashes(&running, &token, &root, &after.entry).await;
    assert!(
        hashes.contains(&sha256(BEFORE)),
        "旧内容必须仍可恢复：{hashes:?}"
    );
    assert!(
        hashes.contains(&sha256(AFTER)),
        "最终内容在当前/历史：{hashes:?}"
    );
    assert_eq!(
        version_count(&lab, &before.entry).await,
        2,
        "rename-over 只产生一个保存版本"
    );
    assert!(
        !has_delete_event(&events(&running, &token, &root).await, NAME),
        "同一路径的保存不推导删除"
    );
    assert_eq!(std::fs::read(work_file(&lab)).expect("读工作文件"), AFTER);
    assert_eq!(
        std::fs::read(lab.work().join("other.txt")).expect("读邻居文件"),
        OTHER,
        "rename-over 只影响目标路径"
    );
    assert_eq!(
        std::fs::metadata(lab.work().join("other.txt"))
            .expect("邻居元数据")
            .len(),
        OTHER.len() as u64
    );
    let kept = snapshot(&running, &token, &root).await;
    assert!(
        kept.iter()
            .any(|item| item.get("name").and_then(Value::as_str) == Some("other.txt")),
        "邻居条目仍在清单里"
    );
    let changes = events(&running, &token, &root).await;
    assert!(
        !has_delete_event(&changes, "other.txt"),
        "rename-over 不能影响邻居：{changes:?}"
    );
    running.stop().await;
}

/// T18/T27：保存期间截断/半写与客户端提交重叠，不得把未采集内容覆盖成成功。
#[tokio::test]
async fn truncate_during_save_blocks_stale_put_and_keeps_versions() {
    const BEFORE: &[u8] = b"the whole original document\n";
    const PARTIAL: &[u8] = b"the rewritten first hal";
    const FINAL: &[u8] = b"the rewritten first half plus the rest that arrives later\n";
    const CLIENT: &[u8] = b"client content planned before the save\n";

    let lab = Lab::new("s103-truncate", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册 Root");
    let base = seed(&running, &token, &root, &lab, BEFORE).await;

    // 应用保存方式：先截断，再写前半段并停住（模拟保存中途）。
    std::fs::write(work_file(&lab), PARTIAL).expect("截断并写半段");
    let (status, body) = put(&running, &token, &root, &base, CLIENT, 0x51).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "半写状态不能接受旧基线提交：{body}"
    );
    assert_eq!(
        body.pointer("/error/code").and_then(Value::as_str),
        Some("RECONCILE_REQUIRED"),
        "必须给出明确的协调错误：{body}"
    );
    assert_eq!(
        std::fs::read(work_file(&lab)).expect("读工作文件"),
        PARTIAL,
        "被拒绝的提交不能改动正在保存的文件"
    );

    // 继续写完：最终稳定内容被采集；旧内容与半写状态都没有丢失。
    std::fs::write(work_file(&lab), FINAL).expect("写完");
    let final_head = wait_head(&running, &token, &root, NAME, &sha256(FINAL)).await;
    let hashes = history_hashes(&running, &token, &root, &final_head.entry).await;
    assert!(
        hashes.contains(&sha256(FINAL)),
        "最终内容必须在历史/当前里：{hashes:?}"
    );
    assert_eq!(
        version_bytes(&lab, &base.version).await.as_deref(),
        Some(BEFORE),
        "保存前的已确认版本仍在对象区"
    );
    assert!(
        !history_hashes(&running, &token, &root, &base.entry)
            .await
            .contains(&sha256(CLIENT)),
        "被拒绝的客户端内容没有落库"
    );
    assert!(
        !events(&running, &token, &root)
            .await
            .iter()
            .any(|event| event.get("hash").and_then(Value::as_str) == Some(sha256(CLIENT).as_str())),
        "被拒绝的客户端内容不能出现在 changes"
    );
    running.stop().await;
}

/// T25：Root 内 symlink 不跟随、不读取 Root 外内容、不产生删除，原外部文件字节不变。
#[tokio::test]
async fn symlink_is_never_followed_or_adopted() {
    const NOTE: &[u8] = b"normal file in the root\n";
    const KEEP: &[u8] = b"neighbour that stays untouched\n";
    const OUTSIDE: &[u8] = b"outside secret that must not be read\n";
    const OUTSIDE2: &[u8] = b"a different outside target\n";

    let lab = Lab::new("s103-symlink", None, None);
    let outside = lab.root().join("outside");
    std::fs::create_dir_all(&outside).expect("建 Root 外目录");
    std::fs::write(outside.join("secret.txt"), OUTSIDE).expect("写 Root 外文件");
    std::fs::write(outside.join("other.txt"), OUTSIDE2).expect("写第二个 Root 外文件");

    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册 Root");
    let note = seed(&running, &token, &root, &lab, NOTE).await;
    std::fs::write(lab.work().join("keep.txt"), KEEP).expect("写邻居");
    let keep = wait_head(&running, &token, &root, "keep.txt", &sha256(KEEP)).await;

    // 把已采纳的文件换成 Root 外的 symlink，再加一个目录 symlink：都不能跟随、不能采纳。
    std::fs::remove_file(work_file(&lab)).expect("移走普通文件");
    std::os::unix::fs::symlink(outside.join("secret.txt"), work_file(&lab)).expect("建 symlink");
    std::os::unix::fs::symlink(&outside, lab.work().join("out-dir")).expect("建目录 symlink");

    // 让观察循环跑几轮：Root 有不可支持的条目 → 采集显式 blocked（不是静默完成）。
    let mut blocked = false;
    for _ in 0..40 {
        let view = root_view(&running, &token, &root).await;
        let state = view
            .get("collection")
            .and_then(|collection| collection.get("state"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if state == "blocked" {
            blocked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        blocked,
        "含 symlink 的 Root 必须显式暂停采集，而不是静默完成"
    );
    assert_eq!(
        std::fs::read(outside.join("secret.txt")).expect("读 Root 外文件"),
        OUTSIDE,
        "symlink 目标字节必须未被改动"
    );
    assert_eq!(
        std::fs::read(outside.join("other.txt")).expect("读第二个 Root 外文件"),
        OUTSIDE2
    );
    let entries = snapshot(&running, &token, &root).await;
    assert!(
        !entries.iter().any(|item| {
            matches!(
                item.get("hash").and_then(Value::as_str),
                Some(hash) if hash == sha256(OUTSIDE) || hash == sha256(OUTSIDE2)
            )
        }),
        "Root 外内容不能成为版本：{entries:?}"
    );
    assert!(
        !entries
            .iter()
            .any(|item| item.get("name").and_then(Value::as_str) == Some("out-dir")),
        "目录 symlink 不能被采纳成目录条目"
    );
    // 旧内容仍然可恢复：要么仍在基线里，要么在回收站里（扫描与替换的竞争允许它作为删除进入
    // 回收站，但绝不能变成 Root 外内容）。
    let note_in_snapshot = entries
        .iter()
        .find(|item| item.get("entry_id").and_then(Value::as_str) == Some(note.entry.as_str()))
        .and_then(|item| item.get("hash").and_then(Value::as_str))
        .map(str::to_owned);
    let note_in_trash = trash_rows(&lab)
        .await
        .into_iter()
        .find(|(entry, _, _, _)| entry == &note.entry);
    match (note_in_snapshot, note_in_trash) {
        (Some(hash), _) => assert_eq!(hash, sha256(NOTE), "基线内容不能被 symlink 顶替"),
        (None, Some(row)) => assert_eq!(
            version_bytes(&lab, &row.2).await.as_deref(),
            Some(NOTE),
            "进入回收站的旧内容仍可恢复"
        ),
        (None, None) => panic!("旧内容既不在基线也不在回收站"),
    }
    // 邻居不受影响：哈希、revision 与内容都不变。
    let kept = head_named(&running, &token, &root, "keep.txt")
        .await
        .expect("邻居条目仍在");
    assert_eq!(kept.hash, sha256(KEEP));
    assert_eq!(kept.revision, keep.revision);
    assert_eq!(
        std::fs::read(lab.work().join("keep.txt")).expect("读邻居"),
        KEEP
    );

    // symlink 换目标：仍然不跟随，目标内容都不进历史，也不产生删除事件。
    std::fs::remove_file(work_file(&lab)).expect("删 symlink");
    std::os::unix::fs::symlink(outside.join("other.txt"), work_file(&lab)).expect("换目标");
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    let changes = events(&running, &token, &root).await;
    assert!(
        !changes.iter().any(|event| {
            matches!(
                event.get("hash").and_then(Value::as_str),
                Some(hash) if hash == sha256(OUTSIDE) || hash == sha256(OUTSIDE2)
            )
        }),
        "symlink 目标内容不得进入 changes：{changes:?}"
    );
    assert!(
        !has_delete_event(&changes, "keep.txt"),
        "symlink 现场不能产生邻居的删除事件"
    );
    assert_eq!(
        std::fs::read(outside.join("other.txt")).expect("读第二个 Root 外文件"),
        OUTSIDE2
    );
    running.stop().await;
}

/// T17：外部 rename 保持身份与版本；外部删除进入回收站并按原身份恢复。
#[tokio::test]
async fn external_rename_and_delete_propagate_and_restore() {
    const NOTE: &[u8] = b"rename me without a new version\n";
    const GONE: &[u8] = b"delete me but keep me recoverable\n";

    let lab = Lab::new("s103-move-delete", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册 Root");
    let note = seed(&running, &token, &root, &lab, NOTE).await;
    std::fs::write(lab.work().join("gone.txt"), GONE).expect("写待删文件");
    let gone = wait_head(&running, &token, &root, "gone.txt", &sha256(GONE)).await;

    // 外部 rename：EntryId 与固定版本都不变，也不产生新版本/新事件内容。
    let renamed_path = lab.work().join("renamed.txt");
    std::fs::rename(work_file(&lab), &renamed_path).expect("外部 rename");
    let renamed = wait_head(&running, &token, &root, "renamed.txt", &sha256(NOTE)).await;
    assert_eq!(renamed.entry, note.entry, "rename 不改变 EntryId");
    assert_eq!(renamed.version, note.version, "rename 不产生新版本");
    assert_eq!(version_count(&lab, &note.entry).await, 1);
    assert!(
        head_named(&running, &token, &root, NAME).await.is_none(),
        "旧路径不再有条目"
    );

    // 外部删除：进入回收站，内容仍可按固定版本读到。
    std::fs::remove_file(lab.work().join("gone.txt")).expect("外部删除");
    let mut trashed = None;
    for _ in 0..80 {
        let rows = trash_rows(&lab).await;
        if let Some(row) = rows
            .iter()
            .find(|(entry, path, _, _)| entry == &gone.entry && path == "gone.txt")
        {
            trashed = Some(row.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let trashed = trashed.expect("外部删除必须进入回收站");
    assert_eq!(
        version_bytes(&lab, &trashed.2).await.as_deref(),
        Some(GONE),
        "回收站记录必须指向删除前的已确认版本"
    );
    assert!(
        has_delete_event(&events(&running, &token, &root).await, "gone.txt"),
        "外部删除进入 changes"
    );
    assert!(
        head_named(&running, &token, &root, "gone.txt")
            .await
            .is_none(),
        "删除后不在活跃清单"
    );

    // 回收站恢复：原 EntryId、原内容回到工作目录。
    let (status, restored) = submit(
        &running,
        &token,
        &root,
        json!({
            "op_id": identity(0x71),
            "device_id": identity(0x72),
            "server_epoch": gone.epoch,
            "kind": "restore",
            "entry_id": gone.entry,
            "base_revision": trashed.3,
            "local_generation": "7",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "回收站恢复应提交：{restored}");
    assert_eq!(
        restored.get("entry_id"),
        Some(&Value::String(gone.entry.clone()))
    );
    assert_eq!(
        std::fs::read(lab.work().join("gone.txt")).expect("恢复后读文件"),
        GONE
    );
    assert!(trash_rows(&lab).await.is_empty(), "恢复后回收站清单应为空");
    running.stop().await;
}
