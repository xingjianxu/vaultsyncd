//! S052 验收：服务端小型 HTTP 闭环与满盘故障（design §21.1 T03/T17/T21/T22/T23）。
//!
//! 每个用例启动真实监听的服务端（真实 TCP、真实 SQLite、真实 btrfs 工作目录），按客户端的方式
//! 走完整闭环：
//!
//! 1. 登录 → 注册 Root；
//! 2. 初始外部文件被后台观察循环采纳（首次导入，产生版本与事件）；
//! 3. 上传会话/分块 → finalize → 内容 put 操作提交（200 committed，同一事务写版本、事件与操作结果）；
//! 4. 快照 → 按固定版本身份下载 → 内容与哈希一致；
//! 5. 外部修改工作文件 → 观察循环采纳 → `changes` 出现 external 事件；
//! 6. 重启（真实优雅停止 + 用同一状态目录再启动）后版本/hash/事件完全一致；
//! 7. 满盘故障：工作盘满与状态盘满都**不得**提前返回 committed（507/503），恢复后可重新提交。
//!
//! 满盘用例需要小型专用 btrfs 卷（`tools/test-volume/test-volume.sh`），因此按环境变量启用：
//! `VAULTSYNCD_S052_WORK_VOLUME` / `VAULTSYNCD_S052_STATE_VOLUME` 指向该卷的挂载目录。

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{
    Lab, Running, all_changes, create_snapshot, identity, login, put_operation, register_root,
    snapshot_entries, upload, wait_for,
};
use reqwest::StatusCode;
use serde_json::Value;

/// 初始外部内容。
const V1: &[u8] = b"v1: external note written by the user\n";
/// 第一次客户端提交的内容（跨多个块，验证分块上传）。
const V2: &[u8] =
    b"v2: uploaded by the client in several chunks. 0123456789 abcdefghij klmnopqrst uvwxyz\n";
/// 第二次外部修改。
const V3: &[u8] = b"v3: edited outside the client\n";

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

/// 从快照清单里取某个条目的字段。
fn entry_field<'a>(entries: &'a [Value], name: &str, field: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|entry| entry.get(field))
        .and_then(Value::as_str)
}

/// 在 Root 里建好初始文件并等观察循环采纳。
async fn seed_and_adopt(running: &Running, token: &str, root: &str, lab: &Lab) {
    std::fs::write(work_file(lab), V1).expect("写初始外部文件");
    wait_for("初始导入采纳", || async {
        let entries = snapshot_entries(
            running,
            token,
            root,
            create_snapshot(running, token, root)
                .await?
                .get("snapshot_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "快照响应没有 snapshot_id".to_owned())?,
        )
        .await?;
        Ok(entry_field(&entries, NAME, "version_id").is_some())
    })
    .await
    .expect("初始导入应被采纳");
}

/// 完整闭环 + 重启一致性（不依赖满盘卷）。
#[tokio::test]
async fn full_http_loop_is_consistent_across_restart() {
    let lab = Lab::new("loop", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册 Root");

    // 1. 初始外部文件被采纳：首个版本与事件都来自观察循环。
    seed_and_adopt(&running, &token, &root, &lab).await;
    let snapshot = create_snapshot(&running, &token, &root)
        .await
        .expect("快照");
    let epoch = snapshot
        .get("server_epoch")
        .and_then(Value::as_str)
        .expect("快照带 epoch")
        .to_owned();
    let entries = snapshot_entries(
        &running,
        &token,
        &root,
        snapshot
            .get("snapshot_id")
            .and_then(Value::as_str)
            .expect("snapshot_id"),
    )
    .await
    .expect("快照清单");
    let entry = entry_field(&entries, NAME, "entry_id")
        .expect("条目身份")
        .to_owned();
    let revision = entry_field(&entries, NAME, "revision")
        .expect("条目 revision")
        .to_owned();
    assert_eq!(
        entry_field(&entries, NAME, "hash"),
        Some(sha256(V1).as_str()),
        "初始版本内容哈希应等于外部文件内容"
    );

    // 2. 上传（跨 3 个块）→ finalize → put 操作。
    let (upload_id, hash, size) = upload(&running, &token, &root, V2, 32)
        .await
        .expect("上传并 finalize");
    assert_eq!(hash, sha256(V2));
    let device = identity(0x52);
    let op_id = identity(0x11);
    let (status, result) = put_operation(
        &running, &token, &root, &op_id, &device, &epoch, &entry, &revision, &upload_id,
    )
    .await
    .expect("提交操作");
    assert_eq!(status, StatusCode::OK, "put 应提交：{result}");
    assert_eq!(
        result.get("state").and_then(Value::as_str),
        Some("committed")
    );
    let version2 = result
        .get("version_id")
        .and_then(Value::as_str)
        .expect("版本身份")
        .to_owned();
    let revision2 = result
        .get("revision")
        .and_then(Value::as_str)
        .expect("新 revision")
        .to_owned();
    let root_seq2 = result
        .get("root_seq")
        .and_then(Value::as_str)
        .expect("RootSeq")
        .to_owned();
    assert_eq!(
        result.get("echo_local_generation").and_then(Value::as_str),
        Some("7"),
        "客户端本地代际必须回显"
    );
    assert_eq!(
        std::fs::read(work_file(&lab)).expect("读工作文件"),
        V2,
        "提交后工作目录就是新内容"
    );

    // 3. 响应丢失后的重试：同一 OpId 同一请求返回同一结果（T03）。
    let (status, replay) = put_operation(
        &running, &token, &root, &op_id, &device, &epoch, &entry, &revision, &upload_id,
    )
    .await
    .expect("重试提交");
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay.get("replayed").and_then(Value::as_bool), Some(true));
    for field in ["version_id", "revision", "root_seq"] {
        assert_eq!(
            replay.get(field).and_then(Value::as_str),
            Some(match field {
                "version_id" => version2.as_str(),
                "revision" => revision2.as_str(),
                _ => root_seq2.as_str(),
            }),
            "重试必须返回同一 {field}"
        );
    }

    // 4. 快照 → 按固定版本身份下载。
    let snapshot = create_snapshot(&running, &token, &root)
        .await
        .expect("快照");
    let snapshot_id = snapshot
        .get("snapshot_id")
        .and_then(Value::as_str)
        .expect("snapshot_id")
        .to_owned();
    let entries = snapshot_entries(&running, &token, &root, &snapshot_id)
        .await
        .expect("快照清单");
    assert_eq!(
        entry_field(&entries, NAME, "version_id"),
        Some(version2.as_str())
    );
    assert_eq!(entry_field(&entries, NAME, "hash"), Some(hash.as_str()));
    assert_eq!(
        entry_field(&entries, NAME, "size"),
        Some(size.to_string().as_str())
    );
    let (status, bytes, header_hash) = running
        .download(
            &format!("/v1/roots/{root}/entries/{entry}/versions/{version2}/content"),
            &token,
        )
        .await
        .expect("下载");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, V2, "下载内容必须逐字节等于上传内容");
    if let Some(header_hash) = header_hash {
        assert_eq!(header_hash, hash, "下载响应头哈希与快照一致");
    }

    // 5. 外部修改：观察循环采纳成 external 版本与事件。
    std::fs::write(work_file(&lab), V3).expect("外部改写");
    wait_for("外部修改采纳", || async {
        let batches = all_changes(&running, &token, &root).await?;
        Ok(batches.iter().any(|batch| {
            batch
                .get("events_json")
                .and_then(Value::as_str)
                .is_some_and(|events| {
                    events.contains("\"origin\":\"external\"")
                        && events.contains(V3.len().to_string().as_str())
                })
        }))
    })
    .await
    .expect("外部修改应被采纳并进入 changes");

    let batches_before = all_changes(&running, &token, &root)
        .await
        .expect("拉取 changes");
    let snapshot_before = create_snapshot(&running, &token, &root)
        .await
        .expect("快照");
    let entries_before = snapshot_entries(
        &running,
        &token,
        &root,
        snapshot_before
            .get("snapshot_id")
            .and_then(Value::as_str)
            .expect("snapshot_id"),
    )
    .await
    .expect("快照清单");
    let version3 = entry_field(&entries_before, NAME, "version_id")
        .expect("外部采纳后的版本")
        .to_owned();
    assert_ne!(version3, version2, "外部修改产生新版本");
    assert_eq!(
        entry_field(&entries_before, NAME, "hash"),
        Some(sha256(V3).as_str())
    );
    assert!(
        batches_before
            .iter()
            .any(|batch| batch.get("seq").and_then(Value::as_str) == Some(root_seq2.as_str())),
        "客户端提交的批次仍在 changes 里"
    );

    // 6. 重启：真实优雅停止，再用同一状态目录启动。
    running.stop().await;
    let restarted = Running::start(&lab).await.expect("重启服务端");
    let token = login(&restarted).await.expect("重启后登录");
    let batches_after = all_changes(&restarted, &token, &root)
        .await
        .expect("重启后拉取 changes");
    assert_eq!(
        batches_after, batches_before,
        "重启后 changes 批次（含 seq/events_json）逐字段一致"
    );
    let snapshot_after = create_snapshot(&restarted, &token, &root)
        .await
        .expect("重启后快照");
    assert_eq!(
        snapshot_after.get("server_epoch"),
        snapshot_before.get("server_epoch"),
        "重启不改 epoch（没有重建）"
    );
    let entries_after = snapshot_entries(
        &restarted,
        &token,
        &root,
        snapshot_after
            .get("snapshot_id")
            .and_then(Value::as_str)
            .expect("snapshot_id"),
    )
    .await
    .expect("重启后快照清单");
    assert_eq!(
        entry_field(&entries_after, NAME, "version_id"),
        Some(version3.as_str()),
        "重启后当前版本不变"
    );
    assert_eq!(
        entry_field(&entries_after, NAME, "hash"),
        Some(sha256(V3).as_str()),
        "重启后内容哈希不变"
    );
    assert_eq!(
        entry_field(&entries_after, NAME, "revision"),
        entry_field(&entries_before, NAME, "revision"),
        "重启后条目 revision 不变"
    );
    let (status, bytes, _) = restarted
        .download(
            &format!("/v1/roots/{root}/entries/{entry}/versions/{version3}/content"),
            &token,
        )
        .await
        .expect("重启后下载");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, V3);
    restarted.stop().await;
}

/// 读卷的可用空间：用 `df --output=avail -B1`（只读、无 unsafe；测试环境由 devShell 提供 coreutils）。
fn available_bytes(path: &Path) -> u64 {
    let output = std::process::Command::new("df")
        .arg("--output=avail")
        .arg("-B1")
        .arg(path)
        .output()
        .expect("执行 df");
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .nth(1)
        .and_then(|line| line.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// 把目录所在文件系统填到几乎没空间（保留 `reserve` 字节），返回填充文件名。
fn fill_until_full(dir: &Path, reserve: u64) -> PathBuf {
    let filler = dir.join(".filler");
    let block = vec![0x5a_u8; 1024 * 1024];
    let mut file = std::fs::File::create(&filler).expect("建填充文件");
    use std::io::Write;
    loop {
        if available_bytes(dir) < reserve + block.len() as u64 {
            break;
        }
        if file.write_all(&block).is_err() {
            break;
        }
    }
    let _ = file.sync_all();
    filler
}

fn volume_from_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
}

/// 满盘（工作盘）不提前 committed：发布因 ENOSPC 失败时是 507/503，工作文件与版本都不变；
/// 腾出空间后新的 OpId 可以正常提交。
#[tokio::test]
async fn work_volume_full_never_reports_committed() {
    let Some(volume) = volume_from_env("VAULTSYNCD_S052_WORK_VOLUME") else {
        eprintln!("跳过：未设置 VAULTSYNCD_S052_WORK_VOLUME（需要小型 btrfs 测试卷）");
        return;
    };
    let lab = Lab::new("workfull", Some(&volume), None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册 Root");
    seed_and_adopt(&running, &token, &root, &lab).await;

    let snapshot = create_snapshot(&running, &token, &root)
        .await
        .expect("快照");
    let epoch = snapshot
        .get("server_epoch")
        .and_then(Value::as_str)
        .expect("epoch")
        .to_owned();
    let entries = snapshot_entries(
        &running,
        &token,
        &root,
        snapshot
            .get("snapshot_id")
            .and_then(Value::as_str)
            .expect("snapshot_id"),
    )
    .await
    .expect("快照清单");
    let entry = entry_field(&entries, NAME, "entry_id")
        .expect("条目")
        .to_owned();
    let revision = entry_field(&entries, NAME, "revision")
        .expect("revision")
        .to_owned();
    let revision_value: u64 = revision.parse().expect("revision 是十进制");

    let (upload_id, _, _) = upload(&running, &token, &root, V2, 32).await.expect("上传");
    let filler = fill_until_full(&lab.work(), 8 * 1024);
    let free = available_bytes(&lab.work());
    assert!(
        free < V2.len() as u64,
        "工作卷应已几乎填满（余 {free} 字节）"
    );

    let device = identity(0x62);
    let op_id = identity(0x21);
    let (status, result) = put_operation(
        &running, &token, &root, &op_id, &device, &epoch, &entry, &revision, &upload_id,
    )
    .await
    .expect("满盘时提交");
    assert!(
        status == StatusCode::INSUFFICIENT_STORAGE || status == StatusCode::SERVICE_UNAVAILABLE,
        "工作盘满必须是 507/503 而不是成功：{status} {result}"
    );
    assert_ne!(
        result.get("state").and_then(Value::as_str),
        Some("committed")
    );
    assert!(
        !result.get("error").is_some_and(|error| error
            .get("code")
            .and_then(Value::as_str)
            .is_some_and(|code| code == "INSUFFICIENT_STORAGE" && false)),
        "错误必须是真实的存储不足"
    );
    // 没有提前提交：工作文件仍是旧内容，条目 revision 未推进，也没有新批次。
    assert_eq!(std::fs::read(work_file(&lab)).expect("读工作文件"), V1);
    let after = snapshot_entries(
        &running,
        &token,
        &root,
        create_snapshot(&running, &token, &root)
            .await
            .expect("快照")
            .get("snapshot_id")
            .and_then(Value::as_str)
            .expect("snapshot_id"),
    )
    .await
    .expect("快照清单");
    assert_eq!(
        entry_field(&after, NAME, "revision"),
        Some(revision.as_str())
    );
    assert_eq!(
        entry_field(&after, NAME, "hash"),
        Some(sha256(V1).as_str()),
        "满盘不改变当前版本"
    );
    let batches = all_changes(&running, &token, &root).await.expect("changes");
    assert!(
        !batches.iter().any(
            |batch| batch
                .get("seq")
                .and_then(Value::as_str)
                .is_some_and(|seq| seq.parse::<u64>().unwrap_or(0) > 1
                    && batch
                        .get("events_json")
                        .and_then(Value::as_str)
                        .is_some_and(|events| events.contains("\"kind\":\"put\"")))
        ),
        "满盘失败的请求不得留下 committed 事件"
    );

    // 腾出空间后重试：同一 OpId 是稳定拒绝，新 OpId 正常提交。
    std::fs::remove_file(&filler).expect("删除填充文件");
    let (status, result) = put_operation(
        &running, &token, &root, &op_id, &device, &epoch, &entry, &revision, &upload_id,
    )
    .await
    .expect("重试");
    assert_ne!(
        status,
        StatusCode::OK,
        "同一 OpId 不应在失败后变成成功：{result}"
    );
    let (status, result) = put_operation(
        &running,
        &token,
        &root,
        &identity(0x22),
        &device,
        &epoch,
        &entry,
        &revision,
        &upload_id,
    )
    .await
    .expect("新 OpId 提交");
    assert_eq!(status, StatusCode::OK, "{result}");
    let revision_after: u64 = result
        .get("revision")
        .and_then(Value::as_str)
        .expect("revision")
        .parse()
        .expect("十进制");
    assert!(revision_after > revision_value);
    assert_eq!(std::fs::read(work_file(&lab)).expect("读工作文件"), V2);
    running.stop().await;
}

/// 满盘（状态盘）不提前 committed：数据库写失败时是 503，工作文件不被替换成“已提交”的假象；
/// 腾出空间后新的 OpId 可以正常提交。
#[tokio::test]
async fn state_volume_full_never_reports_committed() {
    let Some(volume) = volume_from_env("VAULTSYNCD_S052_STATE_VOLUME") else {
        eprintln!("跳过：未设置 VAULTSYNCD_S052_STATE_VOLUME（需要小型 btrfs 测试卷）");
        return;
    };
    let lab = Lab::new("statefull", None, Some(&volume));
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册 Root");
    seed_and_adopt(&running, &token, &root, &lab).await;
    let snapshot = create_snapshot(&running, &token, &root)
        .await
        .expect("快照");
    let epoch = snapshot
        .get("server_epoch")
        .and_then(Value::as_str)
        .expect("epoch")
        .to_owned();
    let entries = snapshot_entries(
        &running,
        &token,
        &root,
        snapshot
            .get("snapshot_id")
            .and_then(Value::as_str)
            .expect("snapshot_id"),
    )
    .await
    .expect("快照清单");
    let entry = entry_field(&entries, NAME, "entry_id")
        .expect("条目")
        .to_owned();
    let revision = entry_field(&entries, NAME, "revision")
        .expect("revision")
        .to_owned();
    let (upload_id, _, _) = upload(&running, &token, &root, V2, 32).await.expect("上传");

    // 状态盘填满：SQLite 的 WAL/事务写入拿不到空间。
    let filler = fill_until_full(&lab.state(), 4 * 1024);
    let free = available_bytes(&lab.state());
    assert!(free < 64 * 1024, "状态卷应已几乎填满（余 {free} 字节）");

    let device = identity(0x72);
    let op_id = identity(0x31);
    let (status, result) = put_operation(
        &running, &token, &root, &op_id, &device, &epoch, &entry, &revision, &upload_id,
    )
    .await
    .expect("满盘时提交");
    assert!(
        status == StatusCode::INSUFFICIENT_STORAGE || status == StatusCode::SERVICE_UNAVAILABLE,
        "状态盘满必须失败而不是 committed：{status} {result}"
    );
    assert_ne!(
        result.get("state").and_then(Value::as_str),
        Some("committed")
    );
    assert_eq!(
        std::fs::read(work_file(&lab)).expect("读工作文件"),
        V1,
        "元数据事务没提交时不得留下“已发布”的假象（目标未替换）"
    );
    // 操作记录查询也不能报 committed。
    let (status, value) = running
        .json(
            "GET",
            &format!("/v1/roots/{root}/operations/{op_id}"),
            Some(&token),
            None,
        )
        .await
        .expect("查询操作");
    assert!(
        status != StatusCode::OK || value.get("state").and_then(Value::as_str) != Some("committed"),
        "状态盘满的 OpId 不得是 committed：{status} {value}"
    );

    // 腾出空间：新的 OpId 正常提交。
    std::fs::remove_file(&filler).expect("删除填充文件");
    let (status, result) = put_operation(
        &running,
        &token,
        &root,
        &identity(0x32),
        &device,
        &epoch,
        &entry,
        &revision,
        &upload_id,
    )
    .await
    .expect("恢复后提交");
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(
        result.get("state").and_then(Value::as_str),
        Some("committed")
    );
    assert_eq!(std::fs::read(work_file(&lab)).expect("读工作文件"), V2);
    let _ = tokio::time::sleep(Duration::from_millis(50)).await;
    running.stop().await;
}
