//! S103 挂载故障集成验收（design §21.1 T19/T35，§11.5、§12.3）。
//!
//! 这些用例操作**真实挂载**（`sudo -n umount/mount --bind`），因此只在本 run 的 btrfs loop 卷上
//! 运行，并由 `tests/server-integration/run-mount-faults.sh` 创建/清理卷与挂载；未设置
//! `VAULTSYNCD_S103_MOUNT` 时跳过（不伪造挂载故障）。
//!
//! 布局（脚本准备）：卷挂在 `<mnt>`；内部状态与 Vault 子卷都在同一 btrfs 文件系统（准入要求）；
//! Vault 是 `<mnt>/vaultmnt`，它本身是子卷 `<mnt>/vault` 的一次真实挂载，卸载后露出空目录；
//! `<mnt>/decoy` 是替换用子卷。
//!
//! 判据：
//! - 卸载后露出空目录 = `VAULT_UNAVAILABLE`，零删除事件、零新版本、不读取空目录里的诱饵；
//!   重新挂载后（挂载 ID 改变，仍按保守语义暂停）仍无删除、无新版本；
//! - 子卷被替换（同一路径挂上另一个子卷）时暂停 Vault，绝不读取替换内容，也不生成删除；
//! - 已注册 Vault 的 bind 别名不能再次注册；Vault 内出现子挂载时暂停 Vault，外部内容不被读取。

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::{
    Lab, PutArgs, Running, all_changes, create_snapshot, identity, login, put_operation,
    register_vault, snapshot_entries, upload,
};
use reqwest::StatusCode;
use serde_json::Value;

const NAME: &str = "note.txt";

fn sha256(bytes: &[u8]) -> String {
    common::hex(&vaultsyncd_platform_linux_server::files::hash::digest(
        bytes,
    ))
}

/// 本 run 的真实挂载布局；未配置时返回 `None`（用例跳过）。
struct MountLab {
    /// 卷的挂载点（脚本创建/清理）。
    mount: PathBuf,
    /// 注册为 Vault 的路径：`<mnt>/vaultmnt` 是子卷 `<mnt>/vault` 的真实挂载。
    vault: PathBuf,
    device: PathBuf,
    /// 原 Vault 子卷名（用于恢复挂载）。
    vault_subvol: String,
    /// 替换用子卷名（脚本在其中放了同名文件）。
    decoy_subvol: String,
    alias_base: PathBuf,
}

fn mount_lab() -> Option<MountLab> {
    let mount = std::env::var_os("VAULTSYNCD_S103_MOUNT").map(PathBuf::from)?;
    let vault = std::env::var_os("VAULTSYNCD_S103_VAULT")
        .map(PathBuf::from)
        .unwrap_or_else(|| mount.join("vaultmnt"));
    let device = std::env::var_os("VAULTSYNCD_S103_LOOP").map(PathBuf::from)?;
    let vault_subvol =
        std::env::var("VAULTSYNCD_S103_VAULT_SUBVOL").unwrap_or_else(|_| "vault".to_owned());
    let decoy_subvol =
        std::env::var("VAULTSYNCD_S103_DECOY_SUBVOL").unwrap_or_else(|_| "decoy".to_owned());
    let alias_base = std::env::var_os("VAULTSYNCD_S103_ALIAS_BASE")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("vaultsyncd-s103-alias"));
    Some(MountLab {
        mount,
        vault,
        device,
        vault_subvol,
        decoy_subvol,
        alias_base,
    })
}

fn sudo(args: &[&str]) -> Result<(), String> {
    let output = Command::new("sudo")
        .arg("-n")
        .args(args)
        .output()
        .map_err(|error| format!("执行 sudo -n {args:?} 失败：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "sudo -n {args:?} 退出 {:?}：{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn mount_point(path: &Path) -> bool {
    Command::new("mountpoint")
        .arg("-q")
        .arg(path)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn umount(path: &Path) -> Result<(), String> {
    sudo(&["umount", &path.display().to_string()])
}

/// 卸载 Vault 子卷：服务进程按设计持有 Vault 目录句柄，普通 umount 会 EBUSY；管理员对
/// “有人占用”的挂载使用 lazy 卸载（`umount -l`），挂载立即从命名空间分离，原路径露出
/// 父文件系统上的空目录——这正是 T19 要验收的现场。
fn umount_vault(path: &Path) -> Result<(), String> {
    sudo(&["umount", "-l", &path.display().to_string()])
}

/// 把指定子卷挂到 Vault 路径。
fn mount_subvol(lab: &MountLab, subvol: &str) -> Result<(), String> {
    sudo(&[
        "mount",
        "-o",
        &format!("subvol={subvol}"),
        &lab.device.display().to_string(),
        &lab.vault.display().to_string(),
    ])
}

#[derive(Clone, Debug)]
struct Head {
    entry: String,
    revision: String,
    version: String,
    hash: String,
    epoch: String,
}

async fn head_of(running: &Running, token: &str, vault: &str, name: &str) -> Option<Head> {
    let snapshot = create_snapshot(running, token, vault).await.ok()?;
    let epoch = snapshot.get("server_epoch")?.as_str()?.to_owned();
    let id = snapshot.get("snapshot_id")?.as_str()?;
    let entries = snapshot_entries(running, token, vault, id).await.ok()?;
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

async fn wait_head(running: &Running, token: &str, vault: &str, name: &str, hash: &str) -> Head {
    for _ in 0..200 {
        if let Some(head) = head_of(running, token, vault, name).await
            && head.hash == hash
            && !head.version.is_empty()
        {
            return head;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("等待 {name} 采纳为 {hash} 超时");
}

async fn vault_view(running: &Running, token: &str, vault: &str) -> Value {
    let (status, value) = running
        .json("GET", "/v1/admin/vaults", Some(token), None)
        .await
        .expect("读 Vault 清单");
    assert_eq!(status, StatusCode::OK, "{value}");
    value
        .get("vaults")
        .and_then(Value::as_array)
        .and_then(|vaults| {
            vaults
                .iter()
                .find(|item| item.get("vault_id").and_then(Value::as_str) == Some(vault))
        })
        .cloned()
        .unwrap_or_else(|| panic!("Vault 清单里没有 {vault}：{value}"))
}

/// 轮询 Vault 管理视图直到健康与采集状态满足条件。
async fn wait_vault(
    running: &Running,
    token: &str,
    vault: &str,
    health: &str,
    collection: Option<&str>,
) -> Value {
    for _ in 0..200 {
        let view = vault_view(running, token, vault).await;
        let actual_health = view
            .get("health")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let actual_collection = view
            .get("collection")
            .and_then(|collection| collection.get("state"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if actual_health == health && collection.is_none_or(|wanted| actual_collection == wanted) {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("等待 Vault 健康状态为 {health}（采集 {collection:?}）超时");
}

async fn events(running: &Running, token: &str, vault: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for batch in all_changes(running, token, vault).await.expect("changes") {
        let text = batch
            .get("events_json")
            .and_then(Value::as_str)
            .expect("批次带 events_json");
        let parsed: Vec<Value> = serde_json::from_str(text).expect("事件 JSON");
        out.extend(parsed);
    }
    out
}

fn has_delete_event(events: &[Value], path: &str) -> bool {
    events.iter().any(|event| {
        event.get("path").and_then(Value::as_str) == Some(path)
            && event.get("kind").and_then(Value::as_str) == Some("delete")
    })
}

fn has_hash_event(events: &[Value], hash: &str) -> bool {
    events
        .iter()
        .any(|event| event.get("hash").and_then(Value::as_str) == Some(hash))
}

async fn version_count(lab: &Lab, entry_hex: &str) -> i64 {
    let mut conn = common::db(lab).await;
    sqlx::query_scalar("SELECT count(*) FROM versions WHERE lower(hex(entry_id)) = ?1")
        .bind(entry_hex)
        .fetch_one(&mut conn)
        .await
        .expect("查版本数")
}

async fn put(
    running: &Running,
    token: &str,
    vault: &str,
    head: &Head,
    content: &[u8],
) -> (StatusCode, Value) {
    let (upload_id, _, _) = upload(running, token, vault, content, 32)
        .await
        .expect("上传");
    let args = PutArgs {
        op_id: &identity(0x91),
        device_id: &identity(0x92),
        epoch: &head.epoch,
        entry: &head.entry,
        revision: &head.revision,
        upload: &upload_id,
    };
    put_operation(running, token, vault, &args)
        .await
        .expect("提交操作")
}

/// 在 Vault 不可用期间等待若干观察轮，确认不产生删除/新版本。
async fn settle_unavailable(running: &Running, token: &str, lab: &Lab, head: &Head, vault: &str) {
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    let view = wait_vault(running, token, vault, "unavailable", Some("blocked")).await;
    assert!(
        !view
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .is_empty(),
        "Vault 不可用必须给出原因：{view}"
    );
    let current = head_of(running, token, vault, NAME).await.expect("清单");
    assert_eq!(current.version, head.version, "不可用期间固定版本不变");
    assert_eq!(current.revision, head.revision, "不可用期间 revision 不变");
    assert_eq!(version_count(lab, &head.entry).await, 1);
    let changes = events(running, token, vault).await;
    assert!(
        !has_delete_event(&changes, NAME),
        "Vault 不可用不能生成删除：{changes:?}"
    );
}

/// T19：真实卸载后露出空目录 → Vault 不可用；零删除；重新挂载后仍无删除。
#[tokio::test]
async fn real_unmount_pauses_vault_and_never_deletes() {
    const NOTE: &[u8] = b"content that must survive an unmount\n";
    const DECOY: &[u8] = b"decoy written under the empty mount point\n";
    const CLIENT: &[u8] = b"client content attempted while unmounted\n";

    let Some(mounts) = mount_lab() else {
        eprintln!("跳过：未设置 VAULTSYNCD_S103_MOUNT（需要 run-mount-faults.sh 建立的真实卷）");
        return;
    };
    assert!(
        mount_point(&mounts.vault),
        "{} 必须已挂载 Vault 子卷",
        mounts.vault.display()
    );
    std::fs::write(mounts.vault.join(NAME), NOTE).expect("写初始文件");
    std::fs::create_dir_all(&mounts.alias_base).expect("建别名基目录");

    let lab = Lab::allowing(
        "s103-unmount",
        Some(&mounts.mount),
        Some(&mounts.mount),
        vec![mounts.mount.clone(), mounts.alias_base.clone()],
    );
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let vault = register_vault(&running, &token, &mounts.vault)
        .await
        .expect("注册卷根目录");
    let before = wait_head(&running, &token, &vault, NAME, &sha256(NOTE)).await;

    // 真实卸载 Vault 子卷的挂载：原路径露出主文件系统上的空目录。
    umount_vault(&mounts.vault).expect("lazy 卸载 Vault 子卷挂载");
    assert!(!mount_point(&mounts.vault));
    assert!(mounts.vault.is_dir(), "卸载后 Vault 路径应留下空目录");
    std::fs::write(mounts.vault.join(NAME), DECOY).expect("写诱饵");
    settle_unavailable(&running, &token, &lab, &before, &vault).await;
    assert!(
        !has_hash_event(&events(&running, &token, &vault).await, &sha256(DECOY)),
        "露出的空目录/诱饵内容不能被采纳"
    );

    // 卸载期间客户端提交必须被拒绝，且不产生版本/事件。
    let (status, body) = put(&running, &token, &vault, &before, CLIENT).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        body.pointer("/error/code").and_then(Value::as_str),
        Some("VAULT_UNAVAILABLE"),
        "错误语义必须是 Vault 不可用：{body}"
    );
    assert_eq!(version_count(&lab, &before.entry).await, 1);

    // 恢复挂载：内容回到原子卷；挂载 ID 已改变，产品保持保守暂停（不重绑、不推导删除）。
    std::fs::remove_file(mounts.vault.join(NAME)).expect("删除诱饵");
    mount_subvol(&mounts, &mounts.vault_subvol).expect("重新挂载原 Vault 子卷");
    assert!(mount_point(&mounts.vault));
    assert_eq!(
        std::fs::read(mounts.vault.join(NAME)).expect("读工作文件"),
        NOTE,
        "恢复后工作文件字节不变"
    );
    settle_unavailable(&running, &token, &lab, &before, &vault).await;
    let changes = events(&running, &token, &vault).await;
    assert!(
        !has_hash_event(&changes, &sha256(DECOY)) && !has_hash_event(&changes, &sha256(CLIENT)),
        "诱饵与卸载期间的提交都不能进入 changes：{changes:?}"
    );
    running.stop().await;
}

/// T35：bind 别名拒绝注册；Vault 内子挂载暂停；子卷替换暂停且不读取替换内容。
#[tokio::test]
async fn subvolume_replacement_and_bind_aliases_pause_vault() {
    const NOTE: &[u8] = b"the original subvolume content\n";
    const DECOY: &[u8] = b"another subvolume mounted at the same path\n";
    const OUTSIDE: &[u8] = b"content outside the vault via a bind mount\n";

    let Some(mounts) = mount_lab() else {
        eprintln!("跳过：未设置 VAULTSYNCD_S103_MOUNT（需要 run-mount-faults.sh 建立的真实卷）");
        return;
    };
    assert!(
        mount_point(&mounts.vault),
        "{} 必须已挂载 Vault 子卷",
        mounts.vault.display()
    );
    let decoy_file = mounts.mount.join(&mounts.decoy_subvol).join(NAME);
    assert!(
        decoy_file.is_file(),
        "缺少替换用子卷文件 {}",
        decoy_file.display()
    );
    std::fs::write(mounts.vault.join(NAME), NOTE).expect("写初始文件");
    std::fs::create_dir_all(&mounts.alias_base).expect("建别名基目录");

    let lab = Lab::allowing(
        "s103-volume-swap",
        Some(&mounts.mount),
        Some(&mounts.mount),
        vec![mounts.mount.clone(), mounts.alias_base.clone()],
    );
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let vault = register_vault(&running, &token, &mounts.vault)
        .await
        .expect("注册卷根目录");
    let before = wait_head(&running, &token, &vault, NAME, &sha256(NOTE)).await;

    // 1) bind 别名：同一目录经另一个路径注册必须被拒绝（不是只比路径字符串）。
    let alias = mounts.alias_base.join("alias");
    std::fs::create_dir_all(&alias).expect("建别名挂载点");
    sudo(&[
        "mount",
        "--bind",
        &mounts.vault.display().to_string(),
        &alias.display().to_string(),
    ])
    .expect("建立 bind 别名");
    let (status, body) = running
        .json(
            "POST",
            "/v1/admin/vaults",
            Some(&token),
            Some(&serde_json::json!({ "path": alias.display().to_string() })),
        )
        .await
        .expect("注册别名");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body.pointer("/error/code").and_then(Value::as_str),
        Some("UNSUPPORTED_STORAGE_BACKEND"),
        "{body}"
    );
    let reason = body
        .pointer("/error/details/reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        reason.contains("duplicate") || reason.contains("alias") || reason.contains("overlap"),
        "别名必须按身份拒绝：{reason} / {body}"
    );
    let (_, listed) = running
        .json("GET", "/v1/admin/vaults", Some(&token), None)
        .await
        .expect("Vault 清单");
    assert_eq!(
        listed.get("vaults").and_then(Value::as_array).map(Vec::len),
        Some(1),
        "被拒绝的别名不能变成第二个 Vault：{listed}"
    );
    umount(&alias).expect("卸载 bind 别名");
    std::fs::remove_dir(&alias).expect("删别名挂载点");

    // 2) Vault 内出现子挂载：暂停 Vault，不读取子挂载里的外部内容，也不删除原条目。
    let outside = mounts.alias_base.join("outside");
    std::fs::create_dir_all(&outside).expect("建外部目录");
    std::fs::write(outside.join("secret.txt"), OUTSIDE).expect("写外部内容");
    let nested = mounts.vault.join("nested");
    std::fs::create_dir_all(&nested).expect("建嵌套挂载点");
    sudo(&[
        "mount",
        "--bind",
        &outside.display().to_string(),
        &nested.display().to_string(),
    ])
    .expect("把外部目录 bind 到 Vault 内");
    let view = wait_vault(&running, &token, &vault, "unavailable", Some("blocked")).await;
    let reason = view
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        reason.contains("submount") || reason.contains("挂载") || reason.contains("storage"),
        "子挂载必须给出原因：{view}"
    );
    let changes = events(&running, &token, &vault).await;
    assert!(
        !has_hash_event(&changes, &sha256(OUTSIDE)),
        "子挂载里的外部内容不能被读取：{changes:?}"
    );
    assert!(
        !changes.iter().any(|event| {
            event
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.starts_with("nested/"))
        }),
        "子挂载不能生成外部路径事件：{changes:?}"
    );
    assert!(
        !has_delete_event(&changes, NAME),
        "子挂载不能删除原条目：{changes:?}"
    );
    assert_eq!(
        std::fs::read(outside.join("secret.txt")).expect("读外部内容"),
        OUTSIDE,
        "外部文件未被改动"
    );
    umount(&nested).expect("卸载嵌套 bind");
    wait_vault(&running, &token, &vault, "healthy", None).await;
    assert_eq!(
        head_of(&running, &token, &vault, NAME)
            .await
            .expect("清单")
            .version,
        before.version,
        "解除子挂载后固定版本不变"
    );

    // 3) 子卷替换：同一路径挂上 decoy 子卷；暂停 Vault，绝不采纳替换内容或生成删除。
    umount_vault(&mounts.vault).expect("lazy 卸载原 Vault 子卷");
    mount_subvol(&mounts, &mounts.decoy_subvol).expect("把 decoy 子卷挂到 Vault 路径");
    assert_eq!(
        std::fs::read(mounts.vault.join(NAME)).expect("读替换后的文件"),
        DECOY
    );
    settle_unavailable(&running, &token, &lab, &before, &vault).await;
    let changes = events(&running, &token, &vault).await;
    assert!(
        !has_hash_event(&changes, &sha256(DECOY)),
        "替换子卷的内容不能被采纳：{changes:?}"
    );
    assert!(
        !has_delete_event(&changes, NAME),
        "替换不能生成删除：{changes:?}"
    );

    // 4) 挂回原 Vault 子卷：可见字节回到原内容；挂载 ID 已变，产品保持暂停但零删除。
    umount_vault(&mounts.vault).expect("lazy 卸载 decoy 子卷");
    mount_subvol(&mounts, &mounts.vault_subvol).expect("挂回原 Vault 子卷");
    assert_eq!(
        std::fs::read(mounts.vault.join(NAME)).expect("读原文件"),
        NOTE
    );
    settle_unavailable(&running, &token, &lab, &before, &vault).await;
    assert!(
        !has_hash_event(&events(&running, &token, &vault).await, &sha256(DECOY)),
        "替换内容始终不得进入 changes"
    );
    std::fs::remove_dir_all(&outside).expect("清理外部目录");
    running.stop().await;
}
