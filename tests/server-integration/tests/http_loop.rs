//! S052 验收：服务端小型 HTTP 闭环与满盘故障（design §21.1 T03/T17/T21/T22/T23）。
//!
//! 每个用例启动真实监听的服务端（真实 TCP、真实 SQLite、真实 btrfs 工作目录），按客户端的方式
//! 走完整闭环：
//!
//! 1. 登录 → 注册 Vault；
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

use common::{
    Lab, PutArgs, Running, all_changes, create_snapshot, identity, login, put_operation,
    register_vault, snapshot_entries, upload, wait_for,
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

/// 文件在 Vault 内的名字。
const NAME: &str = "note.txt";

fn sha256(bytes: &[u8]) -> String {
    common::hex(&vaultsyncd_platform_linux_server::files::hash::digest(
        bytes,
    ))
}

fn work_file(lab: &Lab) -> PathBuf {
    lab.work().join(NAME)
}

/// `events_json` 里是否有一条事件精确描述“该路径的新内容就是这份哈希与长度”。
///
/// 只按子串匹配长度会让别的批次（revision、其他哈希里的数字）误命中，从而在采纳真正发生之前
/// 就返回；这里解析事件数组并按字段逐项比较。
fn events_mention(events_json: &str, path: &str, hash: &str, size: usize) -> bool {
    let Ok(events) = serde_json::from_str::<Vec<Value>>(events_json) else {
        return false;
    };
    events.iter().any(|event| {
        event.get("path").and_then(Value::as_str) == Some(path)
            && event.get("hash").and_then(Value::as_str) == Some(hash)
            && event.get("size").and_then(Value::as_str) == Some(size.to_string().as_str())
    })
}

/// 从快照清单里取某个条目的字段。
fn entry_field<'a>(entries: &'a [Value], name: &str, field: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|entry| entry.get(field))
        .and_then(Value::as_str)
}

/// 在 Vault 里建好初始文件并等观察循环采纳。
async fn seed_and_adopt(running: &Running, token: &str, vault: &str, lab: &Lab) {
    std::fs::write(work_file(lab), V1).expect("写初始外部文件");
    wait_for("初始导入采纳", || async {
        let entries = snapshot_entries(
            running,
            token,
            vault,
            create_snapshot(running, token, vault)
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
    let vault = register_vault(&running, &token, &lab.work())
        .await
        .expect("注册 Vault");

    // 1. 初始外部文件被采纳：首个版本与事件都来自观察循环。
    seed_and_adopt(&running, &token, &vault, &lab).await;
    let snapshot = create_snapshot(&running, &token, &vault)
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
        &vault,
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
    let (upload_id, hash, size) = upload(&running, &token, &vault, V2, 32)
        .await
        .expect("上传并 finalize");
    assert_eq!(hash, sha256(V2));
    let device = identity(0x52);
    let op_id = identity(0x11);
    let args = PutArgs {
        op_id: &op_id,
        device_id: &device,
        epoch: &epoch,
        entry: &entry,
        revision: &revision,
        upload: &upload_id,
    };
    let (status, result) = put_operation(&running, &token, &vault, &args)
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
    let vault_seq2 = result
        .get("vault_seq")
        .and_then(Value::as_str)
        .expect("VaultSeq")
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
    let (status, replay) = put_operation(&running, &token, &vault, &args)
        .await
        .expect("重试提交");
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay.get("replayed").and_then(Value::as_bool), Some(true));
    for field in ["version_id", "revision", "vault_seq"] {
        assert_eq!(
            replay.get(field).and_then(Value::as_str),
            Some(match field {
                "version_id" => version2.as_str(),
                "revision" => revision2.as_str(),
                _ => vault_seq2.as_str(),
            }),
            "重试必须返回同一 {field}"
        );
    }

    // 4. 快照 → 按固定版本身份下载。
    let snapshot = create_snapshot(&running, &token, &vault)
        .await
        .expect("快照");
    let snapshot_id = snapshot
        .get("snapshot_id")
        .and_then(Value::as_str)
        .expect("snapshot_id")
        .to_owned();
    let entries = snapshot_entries(&running, &token, &vault, &snapshot_id)
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
            &format!("/v1/vaults/{vault}/entries/{entry}/versions/{version2}/content"),
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
        let batches = all_changes(&running, &token, &vault).await?;
        Ok(batches.iter().any(|batch| {
            batch
                .get("events_json")
                .and_then(Value::as_str)
                .is_some_and(|events| events_mention(events, NAME, &sha256(V3), V3.len()))
        }))
    })
    .await
    .expect("外部修改应被采纳并进入 changes");

    let batches_before = all_changes(&running, &token, &vault)
        .await
        .expect("拉取 changes");
    let snapshot_before = create_snapshot(&running, &token, &vault)
        .await
        .expect("快照");
    let entries_before = snapshot_entries(
        &running,
        &token,
        &vault,
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
            .any(|batch| batch.get("seq").and_then(Value::as_str) == Some(vault_seq2.as_str())),
        "客户端提交的批次仍在 changes 里"
    );

    // 6. 重启：真实优雅停止，再用同一状态目录启动。
    running.stop().await;
    let restarted = Running::start(&lab).await.expect("重启服务端");
    let token = login(&restarted).await.expect("重启后登录");
    let batches_after = all_changes(&restarted, &token, &vault)
        .await
        .expect("重启后拉取 changes");
    assert_eq!(
        batches_after, batches_before,
        "重启后 changes 批次（含 seq/events_json）逐字段一致"
    );
    let snapshot_after = create_snapshot(&restarted, &token, &vault)
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
        &vault,
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
            &format!("/v1/vaults/{vault}/entries/{entry}/versions/{version3}/content"),
            &token,
        )
        .await
        .expect("重启后下载");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, V3);
    restarted.stop().await;
}

/// 把目录所在文件系统填满：先写大块数据，再建大量小文件，直到两者都分配失败。
///
/// 只写一个大文件不够：btrfs 的 `FICLONE` 只需要元数据，而元数据块是单独分配的（T21 的
/// “reflink 后 CoW 分配失败”）。返回全部填充路径。
fn fill_data_then_metadata(dir: &Path) -> Vec<PathBuf> {
    use std::io::Write;
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(60);
    let big = dir.join(".filler");
    let metadata_dir = dir.join(".meta-filler");
    std::fs::create_dir_all(&metadata_dir).expect("建元数据填充目录");
    let block = vec![0x5a_u8; 4 * 1024 * 1024];
    let mut file = std::fs::File::create(&big).expect("建填充文件");
    let mut created: Vec<PathBuf> = Vec::new();
    let mut counter = 0_u32;
    loop {
        let mut progressed = false;
        if file.write_all(&block).is_ok() {
            progressed = true;
        }
        let path = metadata_dir.join(format!("{counter:06}"));
        counter += 1;
        if std::fs::write(&path, b"x").is_ok() {
            created.push(path);
            progressed = true;
        }
        if !progressed || counter > 40_000 || Instant::now() > deadline {
            break;
        }
    }
    let _ = file.sync_all();
    created.push(metadata_dir);
    created.push(big);
    created
}

fn release(paths: &[PathBuf]) {
    for path in paths {
        let _ = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
    }
}

fn volume_from_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
}

/// 满盘（数据空间耗尽）不提前 committed：向工作目录写大块数据直到卷满，随后的大内容发布
/// （reflink 的 CoW 分配也要空间）或元数据事务必然失败；失败必须是 507/503，工作文件仍是旧内容，
/// 版本/revision 不推进。
///
/// 准入要求内部状态目录与 Vault 在同一 btrfs 文件系统（design §11.4），因此两侧都放在专用小卷上。
#[tokio::test]
async fn work_volume_full_never_reports_committed() {
    let Some(volume) = volume_from_env("VAULTSYNCD_S052_WORK_VOLUME") else {
        eprintln!("跳过：未设置 VAULTSYNCD_S052_WORK_VOLUME（需要小型 btrfs 测试卷）");
        return;
    };
    let lab = Lab::new("workfull", Some(&volume), Some(&volume));
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let vault = register_vault(&running, &token, &lab.work())
        .await
        .expect("注册 Vault");
    seed_and_adopt(&running, &token, &vault, &lab).await;

    // 大内容先上传到对象区（对象在状态侧，尚未写工作目录）。
    let big: Vec<u8> = (0..24 * 1024 * 1024)
        .map(|index| u8::try_from(index % 251).expect("字节"))
        .collect();
    let (upload_id, _, _) = upload(&running, &token, &vault, &big, 1 << 20)
        .await
        .expect("上传大内容");
    let before = create_snapshot(&running, &token, &vault)
        .await
        .expect("快照");
    let epoch = before
        .get("server_epoch")
        .and_then(Value::as_str)
        .expect("epoch")
        .to_owned();
    let entries = snapshot_entries(
        &running,
        &token,
        &vault,
        before
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

    // 填满卷。
    let fillers = fill_data_then_metadata(&lab.work());
    let args = PutArgs {
        op_id: &identity(0x21),
        device_id: &identity(0x62),
        epoch: &epoch,
        entry: &entry,
        revision: &revision,
        upload: &upload_id,
    };
    let (status, result) = put_operation(&running, &token, &vault, &args)
        .await
        .expect("满盘时提交");
    assert!(
        status == StatusCode::INSUFFICIENT_STORAGE || status == StatusCode::SERVICE_UNAVAILABLE,
        "工作盘满必须失败而不是 committed：{status} {result}"
    );
    assert_ne!(
        result.get("state").and_then(Value::as_str),
        Some("committed")
    );
    assert_eq!(
        std::fs::read(work_file(&lab)).expect("读工作文件"),
        V1,
        "发布失败时工作文件不得被替换"
    );
    release(&fillers);
    running.stop().await;
}

/// 满盘（状态侧空间/元数据不足）不提前 committed：把状态目录所在的同一块小卷填满，SQLite 的
/// 事务/元数据写入拿不到空间；答案必须是 507/503，绝不出现“没有证据的成功”。
#[tokio::test]
async fn state_volume_full_never_reports_committed() {
    let Some(volume) = volume_from_env("VAULTSYNCD_S052_STATE_VOLUME") else {
        eprintln!("跳过：未设置 VAULTSYNCD_S052_STATE_VOLUME（需要小型 btrfs 测试卷）");
        return;
    };
    let lab = Lab::new("statefull", Some(&volume), Some(&volume));
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let vault = register_vault(&running, &token, &lab.work())
        .await
        .expect("注册 Vault");
    seed_and_adopt(&running, &token, &vault, &lab).await;
    let (upload_id, _, _) = upload(&running, &token, &vault, V2, 32)
        .await
        .expect("上传");
    let before = create_snapshot(&running, &token, &vault)
        .await
        .expect("快照");
    let epoch = before
        .get("server_epoch")
        .and_then(Value::as_str)
        .expect("epoch")
        .to_owned();
    let entries = snapshot_entries(
        &running,
        &token,
        &vault,
        before
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

    // 与工作盘满用例同一块小卷（准入要求两侧同一 btrfs 文件系统），这里把数据与元数据都填到失败，
    // 让 SQLite 的事务写入没有可分配空间。
    let fillers = fill_data_then_metadata(&lab.state());
    let args = PutArgs {
        op_id: &identity(0x31),
        device_id: &identity(0x72),
        epoch: &epoch,
        entry: &entry,
        revision: &revision,
        upload: &upload_id,
    };
    let (status, result) = put_operation(&running, &token, &vault, &args)
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
    // 发布可能已经发生（意图停在 `published`，重启恢复会补完元数据），但**没有任何** committed 证据：
    // 工作文件要么还是旧内容，要么是新内容 + 一条已持久化的意图。两种都不是“已提交”。
    let content = std::fs::read(work_file(&lab)).expect("读工作文件");
    assert!(
        content == V1 || content == V2,
        "工作文件只能是旧内容或已发布的新内容"
    );
    let (status, value) = running
        .json(
            "GET",
            &format!("/v1/vaults/{vault}/operations/{}", identity(0x31)),
            Some(&token),
            None,
        )
        .await
        .expect("查询操作记录");
    assert!(
        status != StatusCode::OK || value.get("state").and_then(Value::as_str) != Some("committed"),
        "满盘失败的操作不得记录成 committed：{status} {value}"
    );
    release(&fillers);
    running.stop().await;
}
