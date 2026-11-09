//! S089显式原生验收：实际备份CLI、离线restore、真实服务和旧epoch请求。
mod common;
use common::{Lab, Running, login, register_vault};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use vaultsyncd_platform_linux_server::files::hash::digest;
use vaultsyncd_server_core::{
    backup::export::Index,
    objects::{ContentDigest, ObjectLayout},
};
struct Server {
    child: Child,
    http: Running,
}
impl Server {
    async fn config(path: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_server-child"))
            .arg(path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let line = tokio::task::spawn_blocking(move || {
            let mut s = String::new();
            BufReader::new(stdout).read_line(&mut s).unwrap();
            s
        })
        .await
        .unwrap();
        assert!(line.starts_with("LISTEN "), "{line}");
        let http = Running::attach(format!(
            "http://{}",
            line.trim().strip_prefix("LISTEN ").unwrap()
        ));
        for _ in 0..100 {
            if http.json("GET", "/v1/health", None, None).await.unwrap().0 == 200 {
                return Self { child, http };
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("server did not become ready");
    }
    async fn lab(lab: &Lab) -> Self {
        let _ = lab.config();
        Self::config(&lab.config_path()).await
    }
    fn kill(mut self) {
        self.child.kill().unwrap();
        assert!(!self.child.wait().unwrap().success());
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn admin() -> PathBuf {
    std::env::var_os("VAULTSYNCD_ADMIN_BIN")
        .expect("actual admin binary")
        .into()
}
async fn command(args: Vec<OsString>, code: i32) -> Value {
    let output = tokio::task::spawn_blocking(move || {
        Command::new(admin())
            .args(args)
            .arg("--json")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(code),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
use std::ffi::OsString;
async fn backup(lab: &Lab, args: &[&str]) -> Value {
    let mut a: Vec<OsString> = std::iter::once("backup")
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect();
    a.extend([
        OsString::from("--socket"),
        lab.state().join("admin/socket").into_os_string(),
    ]);
    command(a, 0).await
}
fn restore_args(op: &str, archive: &Path, state: &Path, vault: &str, work: &Path) -> Vec<OsString> {
    vec![
        "restore".into(),
        op.into(),
        "--archive".into(),
        archive.as_os_str().into(),
        "--state-dir".into(),
        state.as_os_str().into(),
        "--vault".into(),
        format!("{vault}={}", work.display()).into(),
    ]
}
fn empty(volume: &Path, label: &str) -> (PathBuf, PathBuf) {
    let state = volume.join(format!("{label}-state"));
    let work = volume.join(format!("{label}-work"));
    for p in [&state, &work] {
        std::fs::create_dir(p).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    (state, work)
}
fn hex(bytes: &[u8]) -> String {
    ContentDigest::from_bytes(digest(bytes)).to_hex()
}
async fn adopt(s: &Server, token: &str, vault: &str, name: &str, hash: &str) {
    for _ in 0..200 {
        let entries = common::snapshot(&s.http, token, vault).await;
        if entries
            .iter()
            .any(|e| e["name"] == name && e["hash"] == hash)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("not adopted {name}");
}
async fn unchanged_db(state: &Path, index: &Index) {
    vaultsyncd_server_store::backup::verify_database(&state.join("server.sqlite"), index).unwrap();
}

#[tokio::test]
#[ignore = "requires dedicated VAULTSYNCD_S089_VOLUME btrfs and actual VAULTSYNCD_ADMIN_BIN"]
async fn real_empty_restore_preserves_bytes_history_and_isolates_old_epoch() {
    let volume = PathBuf::from(
        std::env::var_os("VAULTSYNCD_S089_VOLUME").expect("dedicated restore volume"),
    );
    assert!(volume.starts_with("/data/vaultsyncd-lab/runs/"));
    let volume = volume.join(format!("run-{}", std::process::id()));
    std::fs::create_dir(&volume).unwrap();
    let lab = Lab::new("r", Some(&volume), Some(&volume));
    lab.init_owner().await;
    std::fs::create_dir(lab.work().join("nested")).unwrap();
    std::fs::write(lab.work().join("nested/ordinary"), b"nested ordinary bytes").unwrap();
    std::fs::write(lab.work().join("note"), b"historical bytes").unwrap();
    std::fs::write(lab.work().join("trash"), b"trash bytes").unwrap();
    std::fs::write(lab.work().join("large"), vec![0x89; 16 * 1024 * 1024]).unwrap();
    let source = Server::lab(&lab).await;
    let token = login(&source.http).await.unwrap();
    let vault = register_vault(&source.http, &token, &lab.work())
        .await
        .unwrap();
    adopt(&source, &token, &vault, "note", &hex(b"historical bytes")).await;
    adopt(&source, &token, &vault, "trash", &hex(b"trash bytes")).await;
    adopt(
        &source,
        &token,
        &vault,
        "large",
        &hex(&vec![0x89; 16 * 1024 * 1024]),
    )
    .await;
    let entries = common::snapshot(&source.http, &token, &vault).await;
    let note = common::entry_named(&entries, "note");
    let entry = note["entry_id"].as_str().unwrap().to_owned();
    let history = note["version_id"].as_str().unwrap().to_owned();
    let old_epoch = common::epoch_of(&source.http, &token, &vault).await;
    let (upload, _, _) =
        common::upload(&source.http, &token, &vault, b"current committed bytes", 8)
            .await
            .unwrap();
    let op = "89010000000000000000000000000001";
    let device = "89020000000000000000000000000002";
    let args = common::PutArgs {
        op_id: op,
        device_id: device,
        epoch: &old_epoch,
        entry: &entry,
        revision: note["revision"].as_str().unwrap(),
        upload: &upload,
    };
    let (code, result) = common::put_operation(&source.http, &token, &vault, &args)
        .await
        .unwrap();
    assert_eq!(code, 200, "{result}");
    std::fs::remove_file(lab.work().join("trash")).unwrap();
    let mut trash = Value::Null;
    for _ in 0..200 {
        let (code, value) = source
            .http
            .json(
                "GET",
                &format!("/v1/vaults/{vault}/trash"),
                Some(&token),
                None,
            )
            .await
            .unwrap();
        assert_eq!(code, 200);
        if value["items"].as_array().is_some_and(|a| !a.is_empty()) {
            trash = value;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!trash.is_null(), "real observer must have collected trash");
    let id = "89030000000000000000000000000003";
    let prepared = backup(&lab, &["prepare", "--id", id]).await;
    let archive = volume.join("restore.vsbk");
    backup(&lab, &["export", id, "--output", archive.to_str().unwrap()]).await;
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(
            lab.state()
                .join("backups")
                .join(id)
                .join("snapshot/manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let manifest_bytes = std::fs::read(
        lab.state()
            .join("backups")
            .join(id)
            .join("snapshot/manifest.json"),
    )
    .unwrap();
    let index = Index::parse(
        &manifest_bytes,
        *ContentDigest::from_hex(prepared["manifest_hash"].as_str().unwrap())
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    source.kill();
    let (state, work) = empty(&volume, "first");
    let validated = command(restore_args("validate", &archive, &state, &vault, &work), 0).await;
    assert_eq!(validated["state"], "validated");
    assert!(!state.join("server.sqlite").exists());
    assert_eq!(std::fs::read_dir(&work).unwrap().count(), 0);
    let restored = command(restore_args("apply", &archive, &state, &vault, &work), 0).await;
    assert_eq!(restored["state"], "restored");
    assert_eq!(restored["server_id"], manifest["server_id"]);
    assert_ne!(restored["epoch"], old_epoch);
    assert!(!state.join("restore.incomplete").exists());
    assert_eq!(
        std::fs::read(work.join("note")).unwrap(),
        b"current committed bytes"
    );
    assert_eq!(
        std::fs::read(work.join("nested/ordinary")).unwrap(),
        b"nested ordinary bytes"
    );
    assert!(!work.join("trash").exists());
    let object = state
        .join("objects")
        .join(ObjectLayout::scoped_relative_object_path(
            vaultsyncd_domain::ids::VaultId::from_hex(&vault).unwrap(),
            &ContentDigest::from_bytes(digest(b"current committed bytes")),
        ));
    assert_ne!(
        std::fs::metadata(&object).unwrap().ino(),
        std::fs::metadata(work.join("note")).unwrap().ino()
    );
    assert_eq!(std::fs::metadata(work.join("note")).unwrap().nlink(), 1);
    unchanged_db(&state, &index).await;
    let settings: Value =
        serde_json::from_slice(&std::fs::read(state.join("backup-config.json")).unwrap()).unwrap();
    assert_eq!(settings["log"]["level"], "WARN");
    let running = Server::config(&state.join("server.toml")).await;
    assert_eq!(
        running
            .http
            .json("GET", "/v1/admin/vaults", Some(&token), None)
            .await
            .unwrap()
            .0,
        401
    );
    let fresh = login(&running.http).await.unwrap();
    let epoch = common::epoch_of(&running.http, &fresh, &vault).await;
    assert_eq!(restored["epoch"], epoch);
    let (code, body) = common::put_operation(&running.http, &fresh, &vault, &args)
        .await
        .unwrap();
    assert_eq!(code, 410, "{body}");
    assert_eq!(
        std::fs::read(work.join("note")).unwrap(),
        b"current committed bytes"
    );
    let (code, bytes, _) = running
        .http
        .download(
            &format!("/v1/vaults/{vault}/entries/{entry}/versions/{history}/content"),
            &fresh,
        )
        .await
        .unwrap();
    assert_eq!(code, 200);
    assert_eq!(bytes, b"historical bytes");
    let (code, value) = running
        .http
        .json(
            "GET",
            &format!("/v1/vaults/{vault}/trash"),
            Some(&fresh),
            None,
        )
        .await
        .unwrap();
    assert_eq!(code, 200);
    assert_eq!(value["items"], trash["items"]);
    let online = command(restore_args("apply", &archive, &state, &vault, &work), 3).await;
    assert_eq!(online["state"], "incomplete"); // 同一状态锁先拒绝在线实例，绝不写在线DB。
    running.kill();
    eprintln!(
        "native restore: real empty instance/current/nested bytes, history/trash/config/ServerId preserved; new epoch rejects old committed operation before replay; source sessions invalidated"
    );

    // 第二次从同一旧备份恢复，不得再次分配相同epoch。
    let (second, second_work) = empty(&volume, "second");
    let second_result = command(
        restore_args("apply", &archive, &second, &vault, &second_work),
        0,
    )
    .await;
    assert_ne!(second_result["epoch"], restored["epoch"]);
    let running = Server::config(&second.join("server.toml")).await;
    let fresh = login(&running.http).await.unwrap();
    let previous = common::PutArgs {
        epoch: restored["epoch"].as_str().unwrap(),
        ..args
    };
    let (code, body) = common::put_operation(&running.http, &fresh, &vault, &previous)
        .await
        .unwrap();
    assert_eq!(code, 410, "{body}");
    running.kill();
    let (nonempty, nonempty_work) = empty(&volume, "nonempty");
    std::fs::write(nonempty_work.join("user-data"), b"preserve").unwrap();
    command(
        restore_args("apply", &archive, &nonempty, &vault, &nonempty_work),
        1,
    )
    .await;
    assert_eq!(
        std::fs::read(nonempty_work.join("user-data")).unwrap(),
        b"preserve"
    );
    assert!(!nonempty.join("server.sqlite").exists());
    let (alias, alias_work) = empty(&volume, "alias");
    let link = volume.join("aliased-work");
    std::os::unix::fs::symlink(&alias_work, &link).unwrap();
    command(restore_args("validate", &archive, &alias, &vault, &link), 1).await;
    command(
        restore_args("validate", &archive, &alias, &vault, &lab.work()),
        1,
    )
    .await;
    let mut missing = restore_args("validate", &archive, &alias, &vault, &alias_work);
    missing.truncate(missing.len() - 2);
    command(missing, 1).await;
    // 真正的在线空实例拥有相同flock；错误码3且普通文件、数据库均不被restore写入。
    let live = Lab::new("live", Some(&volume), Some(&volume));
    live.init_owner().await;
    let live_server = Server::lab(&live).await;
    let live_work = volume.join("live-new-work");
    std::fs::create_dir(&live_work).unwrap();
    command(
        restore_args("apply", &archive, &live.state(), &vault, &live_work),
        3,
    )
    .await;
    let empty_id = "89040000000000000000000000000004";
    backup(&live, &["prepare", "--id", empty_id]).await;
    let empty_archive = volume.join("empty.vsbk");
    backup(
        &live,
        &[
            "export",
            empty_id,
            "--output",
            empty_archive.to_str().unwrap(),
        ],
    )
    .await;
    live_server.kill();
    let (empty_state, _) = empty(&volume, "empty");
    let empty_result = command(
        vec![
            "restore".into(),
            "apply".into(),
            "--archive".into(),
            empty_archive.as_os_str().into(),
            "--state-dir".into(),
            empty_state.as_os_str().into(),
        ],
        0,
    )
    .await;
    assert_eq!(empty_result["vault_count"], "0");
    let empty_server = Server::config(&empty_state.join("server.toml")).await;
    login(&empty_server.http).await.unwrap();
    empty_server.kill();
    eprintln!(
        "native restore: repeated old backup gets distinct epoch; nonempty, missing/original/symlink mappings and live state lock rejected without overwriting data"
    );

    let (corrupt_state, corrupt_work) = empty(&volume, "corrupt");
    let corrupt_archive = volume.join("corrupt.vsbk");
    let mut corrupt = std::fs::read(&archive).unwrap();
    let n = corrupt.len();
    corrupt[n - 5] ^= 1;
    std::fs::write(&corrupt_archive, corrupt).unwrap();
    std::fs::copy(
        format!("{}.verification.json", archive.display()),
        format!("{}.verification.json", corrupt_archive.display()),
    )
    .unwrap();
    command(
        restore_args(
            "validate",
            &corrupt_archive,
            &corrupt_state,
            &vault,
            &corrupt_work,
        ),
        1,
    )
    .await;
    assert!(!corrupt_state.join("server.sqlite").exists());
    assert!(!corrupt_state.join("restore.incomplete").exists());
    // 仅填本run独立1GiB卷，降到预算一半但不填满宿主；真实内核空间检查应拒绝。
    let (space_state, space_work) = empty(&volume, "space");
    let capacity = vaultsyncd_platform_linux_server::restore::Target::open(&space_state).unwrap();
    let threshold = validated["required_bytes"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap()
        / 2;
    let filler_path = volume.join("capacity-filler");
    let mut filler = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&filler_path)
        .unwrap();
    let bytes = vec![0x44; 1024 * 1024];
    for _ in 0..1024 {
        if capacity.available().unwrap() < threshold {
            break;
        }
        if filler.write_all(&bytes).is_err() {
            break;
        }
    }
    filler.sync_all().unwrap();
    assert!(
        capacity.available().unwrap()
            < validated["required_bytes"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
    );
    command(
        restore_args("validate", &archive, &space_state, &vault, &space_work),
        1,
    )
    .await;
    assert!(!space_state.join("server.sqlite").exists());
    drop(filler);
    std::fs::remove_file(filler_path).unwrap();
    assert!(
        Command::new("sync")
            .arg("-f")
            .arg(&volume)
            .status()
            .unwrap()
            .success()
    );
    let required = validated["required_bytes"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    for _ in 0..200 {
        if capacity.available().unwrap() > required + 32 * 1024 * 1024 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        capacity.available().unwrap() > required + 32 * 1024 * 1024,
        "test volume must recover space before SIGKILL case"
    );
    eprintln!(
        "native restore: corrupt archive and real insufficient-space preflight rejected before publishing database"
    );
    // 实际apply进程在未完成标记持久后强杀，随后服务/init必须拒绝，文件现场保留。
    let (interrupted, interrupted_work) = empty(&volume, "interrupted");
    let mut child = Command::new(admin())
        .args(restore_args(
            "apply",
            &archive,
            &interrupted,
            &vault,
            &interrupted_work,
        ))
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..5000 {
        if interrupted.join("restore.incomplete").exists()
            && interrupted.join("restore.candidate.sqlite").exists()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    if !interrupted.join("restore.incomplete").exists() {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!(
            "apply did not reach durable marker: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(interrupted.join("restore.incomplete").exists());
    assert!(!interrupted.join("restore.complete.json").exists());
    assert!(
        matches!(
            vaultsyncd_server_store::db::Database::open(interrupted.join("server.sqlite")).await,
            Err(vaultsyncd_server_store::db::DbError::ForeignDatabase {
                application_id: vaultsyncd_server_store::restore::RESTORE_GUARD_APPLICATION_ID
            })
        ),
        "unchanged pre-S089 database admission must reject the recovery guard too"
    );
    let mut cfg = lab.config();
    cfg.state_dir = interrupted.clone();
    cfg.database = interrupted.join("server.sqlite");
    match vaultsyncd_server::bootstrap::prepare(cfg).await {
        Err(vaultsyncd_server::bootstrap::StartupError::Replay(e)) => {
            assert!(e.contains("incomplete"))
        }
        Err(e) => panic!("wrong startup gate: {e}"),
        Ok(_) => panic!("half restored service was opened"),
    }
    let mut init = Command::new(admin())
        .arg("init")
        .arg("--state-dir")
        .arg(&interrupted)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    init.stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n", common::PASSWORD).as_bytes())
        .unwrap();
    let output = init.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("incomplete"));
    command(
        restore_args("apply", &archive, &interrupted, &vault, &interrupted_work),
        1,
    )
    .await;
    assert!(interrupted.join("restore.incomplete").exists());
    eprintln!(
        "native restore: SIGKILL leaves persistent incomplete evidence; real bootstrap and init refuse before opening half-restored DB; nonempty retry preserved"
    );
}
