//! S103 容量故障集成验收（design §21.1 T21，§11.2、§11.5、§13.2）。
//!
//! 只在本 run 专属的小型 btrfs loop 卷上制造**真实** ENOSPC；未设置
//! `VAULTSYNCD_S103_WORK_VOLUME` / `VAULTSYNCD_S103_STATE_VOLUME` 时跳过（不以 fake 冒充）。
//! 驱动脚本 `tests/server-integration/run-volume-faults.sh` 建立两个互不相同的卷：
//!
//! - 工作盘（Root + 内部状态同一 btrfs，准入要求）：先填满**数据**块，再验证新内容上传失败，
//!   以及预先上传的内容提交时“要么真正成功、要么明确失败”，绝不提前返回 committed；
//! - 状态盘（同布局的另一块卷）：用大量小文件耗尽**元数据**，验证提交事务/发布失败时没有
//!   任何 committed 证据，工作文件与已确认版本保持可恢复；
//! - reflink 后的 CoW 分配失败：先 FICLONE 一个共享 extent 的克隆，再耗尽数据与元数据，
//!   验证对克隆的写入真实返回 ENOSPC、源与克隆字节不变、后续暂存失败且不留下未验证目标。

mod common;

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{
    Lab, Running, create_snapshot, identity, login, register_root, snapshot_entries, upload,
};
use reqwest::StatusCode;
use serde_json::Value;
use vaultsyncd_platform_linux_server::files::clone::{CloneMethod, StageRequest, stage_copy};
use vaultsyncd_platform_linux_server::files::{FileIdentity, ResolveMode, RestrictedDir};

const NAME: &str = "note.txt";
const BASE: &[u8] = b"confirmed content that must survive a full disk\n";

fn sha256(bytes: &[u8]) -> String {
    common::hex(&vaultsyncd_platform_linux_server::files::hash::digest(
        bytes,
    ))
}

fn volume(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
}

fn work_file(lab: &Lab) -> PathBuf {
    lab.work().join(NAME)
}

#[derive(Clone, Debug)]
struct Head {
    entry: String,
    revision: String,
    version: String,
    hash: String,
    epoch: String,
}

async fn head_of(running: &Running, token: &str, root: &str, name: &str) -> Option<Head> {
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
        epoch,
    })
}

async fn wait_head(running: &Running, token: &str, root: &str, name: &str, hash: &str) -> Head {
    for _ in 0..160 {
        if let Some(head) = head_of(running, token, root, name).await
            && head.hash == hash
            && !head.version.is_empty()
        {
            return head;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("等待 {name} 采纳为 {hash} 超时");
}

async fn version_count(lab: &Lab, entry_hex: &str) -> i64 {
    let mut conn = common::db(lab).await;
    sqlx::query_scalar("SELECT count(*) FROM versions WHERE lower(hex(entry_id)) = ?1")
        .bind(entry_hex)
        .fetch_one(&mut conn)
        .await
        .expect("查版本数")
}

async fn operation_state(lab: &Lab, op_hex: &str) -> Option<String> {
    let mut conn = common::db(lab).await;
    sqlx::query_scalar("SELECT state FROM operations WHERE lower(hex(op_id)) = ?1")
        .bind(op_hex)
        .fetch_optional(&mut conn)
        .await
        .expect("查操作状态")
}

/// 只填数据块：一直写大块，直到数据分配失败（不刷小文件，保留元数据余量）。
fn fill_data(dir: &Path, label: &str) -> Vec<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(90);
    let path = dir.join(format!(".data-filler-{label}"));
    let block = vec![0x5a_u8; 4 * 1024 * 1024];
    let mut file = std::fs::File::create(&path).expect("建数据填充文件");
    let mut written = 0_usize;
    while Instant::now() < deadline {
        match file.write_all(&block) {
            Ok(()) => written += block.len(),
            Err(error) => {
                assert_eq!(
                    error.raw_os_error(),
                    Some(28),
                    "填满数据必须是 ENOSPC，实际：{error}"
                );
                break;
            }
        }
    }
    let _ = file.sync_all();
    assert!(written >= 8 * 1024 * 1024, "数据填充太少：{written} 字节");
    vec![path]
}

/// 只压元数据：大量 ~1.5 KiB 文件（btrfs 内联 extent 落在元数据），直到创建失败。
///
/// 返回已创建的填充路径；`full` 表示确实遇到了 ENOSPC（不是达到数量/时间上限）。
fn fill_small_files(meta: &Path, limit: u32) -> (u32, bool) {
    const INLINE: usize = 1536;
    let body = vec![0x61_u8; INLINE];
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut created = 0_u32;
    for counter in 0..limit {
        match std::fs::write(meta.join(format!("{counter:06}")), &body) {
            Ok(()) => created += 1,
            Err(error) => {
                assert_eq!(
                    error.raw_os_error(),
                    Some(28),
                    "压元数据必须遇到 ENOSPC，实际：{error}"
                );
                return (created, true);
            }
        }
        if Instant::now() > deadline {
            break;
        }
    }
    (created, false)
}

/// 把元数据压到分配失败：先写内联小文件；若还能分配新元数据块，就用数据填满卷，
/// 让新元数据块没有可用的数据块，再继续创建小文件直到 ENOSPC。
fn fill_metadata(dir: &Path, label: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let first = dir.join(format!(".meta-filler-{label}"));
    std::fs::create_dir_all(&first).expect("建元数据填充目录");
    let (created, mut full) = fill_small_files(&first, 300_000);
    eprintln!("元数据填充第一轮：{created} 个内联小文件，ENOSPC={full}");
    paths.push(first);
    if !full {
        assert!(created > 0, "第一轮没有创建任何小文件");
        // 数据填满后，btrfs 不能再申请新的元数据块：这就是真实的元数据耗尽手段。
        paths.extend(fill_data(dir, &format!("{label}-meta")));
        let second = dir.join(format!(".meta-filler-{label}-2"));
        std::fs::create_dir_all(&second).expect("建第二轮元数据填充目录");
        let (created, reached) = fill_small_files(&second, 300_000);
        eprintln!("元数据填充第二轮：{created} 个小文件，ENOSPC={reached}");
        paths.push(second);
        full = reached;
    }
    assert!(full, "未能把元数据压到 ENOSPC（填充不足，不能冒充满盘）");
    paths
}

fn release(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_dir_all(path);
        let _ = std::fs::remove_file(path);
    }
}

/// 请求一次上传的第一个块（不再 finalize），返回状态码与响应体。
async fn upload_first_part(
    running: &Running,
    token: &str,
    root: &str,
    content: &[u8],
) -> (StatusCode, Value) {
    let hash = sha256(content);
    let body = serde_json::json!({
        "chunk_size": content.len().to_string(),
        "hash": hash,
        "total_size": content.len().to_string(),
    });
    let (status, value) = running
        .json(
            "POST",
            &format!("/v1/roots/{root}/uploads"),
            Some(token),
            Some(&body),
        )
        .await
        .expect("创建上传会话");
    assert_eq!(status, StatusCode::CREATED, "创建上传会话：{value}");
    let upload_id = value
        .get("upload_id")
        .and_then(Value::as_str)
        .expect("upload_id")
        .to_owned();
    running
        .put_part(
            &format!("/v1/roots/{root}/uploads/{upload_id}/parts/0"),
            token,
            content.to_vec(),
        )
        .await
        .expect("上传第一块")
}

/// T21：工作盘数据空间耗尽时，新内容上传必须明确失败，已确认内容与版本不变。
#[tokio::test]
async fn data_exhaustion_on_work_volume_never_claims_committed() {
    let Some(work_volume) = volume("VAULTSYNCD_S103_WORK_VOLUME") else {
        eprintln!("跳过：未设置 VAULTSYNCD_S103_WORK_VOLUME（需要小型 btrfs 测试卷）");
        return;
    };
    let lab = Lab::new("s103-data-full", Some(&work_volume), Some(&work_volume));
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册 Root");
    std::fs::write(work_file(&lab), BASE).expect("写已确认内容");
    let before = wait_head(&running, &token, &root, NAME, &sha256(BASE)).await;

    // 先在盘还有空间时上传一份内容：之后填满数据，提交只能“真正成功（reflink 共享）或明确失败”。
    let prepared: Vec<u8> = (0..8 * 1024 * 1024_usize)
        .map(|index| u8::try_from(index % 251).expect("字节"))
        .collect();
    let (upload_id, prepared_hash, _) = upload(&running, &token, &root, &prepared, 4 * 1024 * 1024)
        .await
        .expect("预上传内容");

    let fillers = fill_data(&lab.work(), "data");
    // 新内容上传：对象写入需要新的数据块，必须明确 507，而不是静默成功。
    let fresh = vec![0x7b_u8; 4 * 1024 * 1024];
    let (status, value) = upload_first_part(&running, &token, &root, &fresh).await;
    assert_eq!(
        status,
        StatusCode::INSUFFICIENT_STORAGE,
        "数据满时上传块必须明确失败：{value}"
    );
    assert_eq!(
        value.pointer("/error/code").and_then(Value::as_str),
        Some("INSUFFICIENT_STORAGE"),
        "{value}"
    );

    // 预上传内容的提交：reflink 只需要元数据，可能成功；成功就必须有完整证据，失败就必须
    // 保持旧内容与旧版本，绝不出现“ committed 但工作文件没变”或“失败却写了新版本”。
    use common::PutArgs;
    let args = PutArgs {
        op_id: &identity(0xa1),
        device_id: &identity(0xa2),
        epoch: &before.epoch,
        entry: &before.entry,
        revision: &before.revision,
        upload: &upload_id,
    };
    let (status, result) = common::put_operation(&running, &token, &root, &args)
        .await
        .expect("提交预上传内容");
    let committed = result.get("state").and_then(Value::as_str) == Some("committed");
    if status == StatusCode::OK && committed {
        assert_eq!(
            std::fs::read(work_file(&lab)).expect("读工作文件"),
            prepared,
            "声称 committed 就必须是完整的新内容"
        );
        assert_eq!(
            head_of(&running, &token, &root, NAME)
                .await
                .expect("清单")
                .hash,
            prepared_hash,
            "声称 committed 就必须推进到新版本"
        );
    } else {
        assert!(
            status == StatusCode::INSUFFICIENT_STORAGE
                || status == StatusCode::SERVICE_UNAVAILABLE,
            "数据满时提交只能明确失败：{status} {result}"
        );
        assert_eq!(
            std::fs::read(work_file(&lab)).expect("读工作文件"),
            BASE,
            "失败时工作文件仍是旧内容"
        );
        assert_eq!(
            version_count(&lab, &before.entry).await,
            1,
            "失败时不得写新版本"
        );
        assert_ne!(
            operation_state(&lab, &identity(0xa1)).await.as_deref(),
            Some("committed"),
            "失败的操作不能记录成 committed"
        );
    }

    release(&fillers);
    running.stop().await;
}

/// T21：状态盘元数据耗尽时提交必须有明确失败证据，不提前返回 committed。
#[tokio::test]
async fn metadata_exhaustion_on_state_volume_never_reports_committed() {
    let Some(state_volume) = volume("VAULTSYNCD_S103_STATE_VOLUME") else {
        eprintln!("跳过：未设置 VAULTSYNCD_S103_STATE_VOLUME（需要小型 btrfs 测试卷）");
        return;
    };
    let lab = Lab::new("s103-state-full", Some(&state_volume), Some(&state_volume));
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let root = register_root(&running, &token, &lab.work())
        .await
        .expect("注册 Root");
    std::fs::write(work_file(&lab), BASE).expect("写已确认内容");
    let before = wait_head(&running, &token, &root, NAME, &sha256(BASE)).await;

    let content: Vec<u8> = (0..2 * 1024 * 1024_usize)
        .map(|index| u8::try_from(index % 241).expect("字节"))
        .collect();
    let (upload_id, _, _) = upload(&running, &token, &root, &content, 1024 * 1024)
        .await
        .expect("预上传内容");

    // 元数据耗尽：大量 1 字节文件 + SQLite 事务/发布的元数据分配。
    let fillers = fill_metadata(&lab.state(), "meta");

    use common::PutArgs;
    let args = PutArgs {
        op_id: &identity(0xb1),
        device_id: &identity(0xb2),
        epoch: &before.epoch,
        entry: &before.entry,
        revision: &before.revision,
        upload: &upload_id,
    };
    let (status, result) = common::put_operation(&running, &token, &root, &args)
        .await
        .expect("元数据满时提交");
    assert!(
        status == StatusCode::INSUFFICIENT_STORAGE || status == StatusCode::SERVICE_UNAVAILABLE,
        "元数据满必须明确失败：{status} {result}"
    );
    assert_ne!(
        result.get("state").and_then(Value::as_str),
        Some("committed"),
        "元数据满不得提前返回 committed：{result}"
    );
    let state = operation_state(&lab, &identity(0xb1)).await;
    assert!(
        state.as_deref() != Some("committed"),
        "失败的操作不能留下 committed 终态：{state:?}"
    );
    // 事务没提交就只能是 pending（下次启动按持久意图恢复）或 rejected；两者都不是成功。
    assert!(
        state.is_none() || matches!(state.as_deref(), Some("pending" | "rejected")),
        "失败操作的状态必须可恢复/可解释：{state:?}"
    );
    // 工作文件要么仍是旧内容，要么已经发布了新内容但没有任何 committed 证据；两者都不是“已提交”。
    let on_disk = std::fs::read(work_file(&lab)).expect("读工作文件");
    assert!(
        on_disk == BASE || on_disk == content,
        "工作文件只能是旧内容或已发布的新内容"
    );
    assert_eq!(
        version_count(&lab, &before.entry).await,
        1,
        "没有 committed 就不应有新版本"
    );
    release(&fillers);
    running.stop().await;
}

/// T21：reflink 后 CoW 分配失败必须真实报 ENOSPC，源与克隆字节不变，且不留未验证目标。
#[test]
fn reflink_cow_allocation_failure_preserves_source_and_clone() {
    let Some(work_volume) = volume("VAULTSYNCD_S103_WORK_VOLUME") else {
        eprintln!("跳过：未设置 VAULTSYNCD_S103_WORK_VOLUME（需要小型 btrfs 测试卷）");
        return;
    };
    let dir = work_volume.join(format!("s103-cow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建 CoW 工作目录");

    let source_path = dir.join("source.bin");
    let source_bytes: Vec<u8> = (0..8 * 1024 * 1024_usize)
        .map(|index| u8::try_from(index % 249).expect("字节"))
        .collect();
    std::fs::write(&source_path, &source_bytes).expect("写源文件");
    let source = std::fs::File::open(&source_path).expect("打开源文件");
    let source_identity = FileIdentity::of_fd(source.as_raw_fd()).expect("源身份");
    let target = RestrictedDir::open(&dir, ResolveMode::Auto).expect("打开目标目录");

    // 正常 reflink：共享 extent 的克隆（不是完整复制）。
    let clone = stage_copy(StageRequest {
        source: &source,
        source_identity: &source_identity,
        target_dir: &target,
        staging_name: "clone-one",
    })
    .expect("正常 reflink 克隆");
    assert_eq!(
        clone.method,
        CloneMethod::Ficlone,
        "正常同卷克隆必须走 FICLONE（fallback={:?}）",
        clone.fallback
    );
    assert_eq!(clone.sha256, vaultsyncd_platform_linux_server::files::hash::digest(&source_bytes));

    // 耗尽数据与元数据，制造“克隆后 CoW 新分配失败”。
    let mut fillers = fill_data(&dir, "cow-data");
    fillers.extend(fill_metadata(&dir, "cow-meta"));

    // 对克隆的写入需要新的 CoW extent：必须真实 ENOSPC，而不是悄悄改坏共享内容。
    let write = clone.file.write_at(&[0xab_u8; 4096], 0);
    assert_eq!(
        write.as_ref().err().and_then(std::io::Error::raw_os_error),
        Some(28),
        "CoW 分配失败必须是 ENOSPC：{write:?}"
    );
    let mut clone_bytes = Vec::new();
    (&clone.file)
        .seek(SeekFrom::Start(0))
        .expect("回到克隆开头");
    (&clone.file)
        .read_to_end(&mut clone_bytes)
        .expect("读克隆内容");
    assert_eq!(clone_bytes, source_bytes, "失败的 CoW 写入不能改动克隆字节");
    assert_eq!(
        std::fs::read(&source_path).expect("读源文件"),
        source_bytes,
        "源文件字节不变（共享 extent 未被破坏）"
    );

    // 再克隆一次：优先要求容量失败；若源与已有克隆共享 extent，新增引用可能仍能放进
    // 元数据余量（FICLONE 不需要数据块），那也必须产生**完整、可校验**的克隆，而不是半个文件。
    let before: Vec<String> = std::fs::read_dir(&dir)
        .expect("列目录")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    match stage_copy(StageRequest {
        source: &source,
        source_identity: &source_identity,
        target_dir: &target,
        staging_name: "clone-two",
    }) {
        Err(error) => {
            assert!(
                ["reflink_failed", "staging_io_failed", "staging_create_failed"]
                    .contains(&error.code()),
                "失败原因必须是容量/IO：{} / {error}",
                error.code()
            );
            assert!(
                error.staging_removed || !dir.join(&error.staging_name).exists(),
                "失败后不得留下未验证的暂存文件"
            );
            let after: Vec<String> = std::fs::read_dir(&dir)
                .expect("列目录")
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(before, after, "失败不能留下新目标文件：{after:?}");
        }
        Ok(second) => {
            assert_eq!(
                second.sha256,
                vaultsyncd_platform_linux_server::files::hash::digest(&source_bytes),
                "满盘时仍成功的克隆必须是完整的源内容"
            );
            assert_eq!(
                second.method,
                CloneMethod::Ficlone,
                "同卷克隆不得静默退回完整复制：fallback={:?}",
                second.fallback
            );
        }
    }

    release(&fillers);
    let _ = std::fs::remove_dir_all(&dir);
}
