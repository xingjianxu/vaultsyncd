//! 显式运行的S088原生验收：真实btrfs、独立服务子进程、实际admin二进制与TCP。
//! 需要专属run_id且已挂载user_subvol_rm_allowed；本测试和服务都不挂载、不sudo。
mod common;
use common::{Lab, Running, login, register_root};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use vaultsyncd_platform_linux_server::{files::hash::digest, ipc};
use vaultsyncd_server_core::{
    backup::export::{ArchiveVerifier, Index, item_header},
    objects::ContentDigest,
};

struct Server {
    child: Child,
    http: Running,
}
impl Server {
    async fn start(lab: &Lab) -> Self {
        let _ = lab.config();
        let mut child = Command::new(env!("CARGO_BIN_EXE_server-child"))
            .arg(lab.config_path())
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
        panic!("child not ready");
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

async fn rpc(lab: &Lab, op: &str, id: &str) -> Value {
    let mut socket = ipc::connect(&lab.state().join("admin/socket"))
        .await
        .unwrap();
    ipc::write_frame(
        &mut socket,
        &serde_json::to_vec(&["v1", "backup", op, id]).unwrap(),
    )
    .await
    .unwrap();
    serde_json::from_slice(&ipc::read_frame(&mut socket).await.unwrap()).unwrap()
}
async fn cli(lab: &Lab, args: &[&str], online: bool, success: bool) -> Value {
    let executable = std::env::var_os("VAULTSYNCD_ADMIN_BIN").expect("actual admin binary path");
    let mut command = Command::new(executable);
    command.arg("backup").args(args).arg("--json");
    if online {
        command
            .arg("--socket")
            .arg(lab.state().join("admin/socket"));
    }
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert_eq!(
        output.status.success(),
        success,
        "CLI {:?}: {} {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
async fn state(lab: &Lab, id: &str) -> Value {
    let v = rpc(lab, "status", id).await;
    assert_eq!(v["status"], 200, "{v}");
    v["result"].clone()
}
async fn wait_state(lab: &Lab, id: &str, phase: &str) {
    for _ in 0..300 {
        let s = state(lab, id).await;
        if s["phase"] == phase && s["job_pending"] == false {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("state did not become {phase}: {}", state(lab, id).await);
}
async fn adopted(s: &Server, token: &str, root: &str, name: &str, hash: &str) {
    for _ in 0..200 {
        let entries = common::snapshot(&s.http, token, root).await;
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
fn hex(bytes: &[u8]) -> String {
    ContentDigest::from_bytes(digest(bytes)).to_hex()
}
fn snapshot_path(lab: &Lab, id: &str) -> PathBuf {
    lab.state().join("backups").join(id).join("snapshot")
}
fn ro(path: &Path, yes: bool) {
    assert!(
        Command::new("btrfs")
            .args(["property", "set", "-ts"])
            .arg(path)
            .args(["ro", if yes { "true" } else { "false" }])
            .status()
            .unwrap()
            .success()
    );
}
fn receipt(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(format!("{}.verification.json", path.display())).unwrap())
        .unwrap()
}
fn source_objects(lab: &Lab, manifest: &Value) -> Vec<(PathBuf, [u8; 32])> {
    manifest["objects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            let path = lab
                .state()
                .join("objects")
                .join(o["storage_path"].as_str().unwrap());
            let hash = digest(&std::fs::read(&path).unwrap());
            (path, hash)
        })
        .collect()
}
fn assert_sources(objects: &[(PathBuf, [u8; 32])]) {
    for (p, h) in objects {
        assert_eq!(
            digest(&std::fs::read(p).unwrap()),
            *h,
            "shared object changed: {}",
            p.display()
        );
    }
}

/// 验证后才展开测试用副本，逐项比对归档头与Index，不从未经验证的路径生成文件。
fn inspect_archive(path: &Path, manifest_hash: &str, output: &Path) -> Index {
    let bytes = std::fs::read(path).unwrap();
    let mut verifier =
        ArchiveVerifier::new(*ContentDigest::from_hex(manifest_hash).unwrap().as_bytes());
    for chunk in bytes.chunks(17_123) {
        verifier.push(chunk).unwrap();
    }
    let index = verifier.finish().unwrap();
    let n = u64::from_be_bytes(bytes[8..16].try_into().unwrap()) as usize;
    let mut at = 16 + n;
    for item in &index.items {
        let header = item_header(item);
        assert_eq!(&bytes[at..at + header.len()], header);
        at += header.len();
        let target = output.join(&item.path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&target)
            .unwrap();
        std::fs::write(&target, &bytes[at..at + item.size as usize]).unwrap();
        at += item.size as usize;
    }
    assert_eq!(&bytes[at..], &[0; 4]);
    vaultsyncd_server_store::backup::verify_database(&output.join("database.sqlite"), &index)
        .unwrap();
    index
}

#[tokio::test]
#[ignore = "requires dedicated btrfs VAULTSYNCD_S088_VOLUME and built VAULTSYNCD_ADMIN_BIN"]
async fn real_export_integrity_authorization_and_release_restart() {
    let volume =
        PathBuf::from(std::env::var_os("VAULTSYNCD_S088_VOLUME").expect("dedicated run_id volume"));
    assert!(volume.starts_with("/data/vaultsyncd-lab/runs/"));
    let lab = Lab::new("b", Some(&volume), Some(&volume));
    lab.init_owner().await;
    std::fs::write(lab.work().join("note"), b"history-content").unwrap();
    std::fs::write(lab.work().join("trash"), b"trash-content").unwrap();
    // 大于Unix发送缓冲，确保暂停读取会真正保持服务端每ID读门。
    std::fs::write(lab.work().join("large"), vec![0x5b; 16 * 1024 * 1024]).unwrap();
    let mut s = Server::start(&lab).await;
    let token = login(&s.http).await.unwrap();
    let root = register_root(&s.http, &token, &lab.work()).await.unwrap();
    adopted(&s, &token, &root, "note", &hex(b"history-content")).await;
    adopted(
        &s,
        &token,
        &root,
        "large",
        &hex(&vec![0x5b; 16 * 1024 * 1024]),
    )
    .await;
    std::fs::write(lab.work().join("note"), b"current-content").unwrap();
    adopted(&s, &token, &root, "note", &hex(b"current-content")).await;
    std::fs::remove_file(lab.work().join("trash")).unwrap();
    for _ in 0..200 {
        let (status, body) = s
            .http
            .json(
                "GET",
                &format!("/v1/roots/{root}/trash"),
                Some(&token),
                None,
            )
            .await
            .unwrap();
        assert_eq!(status, 200, "{body}");
        if body["items"].as_array().is_some_and(|v| !v.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let id = "88010000000000000000000000000001";
    let prepared = cli(&lab, &["prepare", "--id", id], true, true).await;
    assert_eq!(prepared["phase"], "ready");
    assert_eq!(prepared["integrity"], "unverified");
    assert_eq!(
        cli(&lab, &["prepare", "--id", id], true, true).await["manifest_hash"],
        prepared["manifest_hash"]
    );
    let url = format!("/v1/admin/backup-snapshots/{id}");
    assert_eq!(s.http.json("GET", &url, None, None).await.unwrap().0, 401);
    let client = reqwest::Client::new();
    let response = client
        .get(s.http.uri(&format!("{url}/manifest")))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let manifest_bytes = response.bytes().await.unwrap();
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(hex(&manifest_bytes), prepared["manifest_hash"]);
    assert!(
        manifest["database_contents"]["versions"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 4
    );
    assert!(
        manifest["database_contents"]["trash_entries"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 1
    );
    assert_eq!(manifest["sessions"], "invalidated");
    assert_eq!(manifest["roots"][0]["root_id"], root);
    let objects = source_objects(&lab, &manifest);
    assert!(objects.len() >= 4);
    for item in manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (format!("{url}/files/{}", f["path"].as_str().unwrap()), f))
        .chain(manifest["objects"].as_array().unwrap().iter().map(|o| {
            (
                format!(
                    "{url}/objects/{}/{}",
                    o["root_id"].as_str().unwrap(),
                    o["sha256"].as_str().unwrap()
                ),
                o,
            )
        }))
    {
        let response = client
            .get(s.http.uri(&item.0))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = response.bytes().await.unwrap();
        assert_eq!(hex(&bytes), item.1["sha256"]);
        assert_eq!(bytes.len().to_string(), item.1["size"]);
    }
    for path in [
        format!("{url}/files/server.sqlite"),
        format!("{url}/objects/{root}/{}", "00".repeat(32)),
    ] {
        assert_eq!(
            client
                .get(s.http.uri(&path))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    assert_eq!(
        client
            .get(s.http.uri(&format!("{url}/manifest")))
            .bearer_auth(&token)
            .header("origin", "https://outside.invalid")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    cli(&lab, &["verify-source", id], true, true).await;
    let archive = volume.join(format!("s088-{}-good.vsbk", std::process::id()));
    let exported = cli(
        &lab,
        &["export", id, "--output", archive.to_str().unwrap()],
        true,
        true,
    )
    .await;
    assert_eq!(exported["state"], "exported_unverified");
    assert_eq!(
        std::fs::metadata(&archive).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        cli(&lab, &["verify", archive.to_str().unwrap()], false, true).await["state"],
        "verified"
    );
    let unpack = volume.join(format!("s088-{}-unpack", std::process::id()));
    let index = inspect_archive(
        &archive,
        prepared["manifest_hash"].as_str().unwrap(),
        &unpack,
    );
    assert_eq!(index.items.len(), objects.len() + 2);
    // 真实CLI进程中断：重新verify先撤销旧verified，SIGKILL后可再verify。
    let mut verifier = Command::new(std::env::var_os("VAULTSYNCD_ADMIN_BIN").unwrap())
        .args(["backup", "verify", archive.to_str().unwrap(), "--json"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..500 {
        let current: Option<Value> =
            std::fs::read(format!("{}.verification.json", archive.display()))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
        if current.is_some_and(|v| v["reason"] == "verification in progress") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    verifier.kill().unwrap();
    assert!(!verifier.wait().unwrap().success());
    assert_eq!(receipt(&archive)["state"], "incomplete");
    cli(&lab, &["verify", archive.to_str().unwrap()], false, true).await;
    let partial = volume.join(format!("s088-{}-cancel.vsbk", std::process::id()));
    let mut exporter = Command::new(std::env::var_os("VAULTSYNCD_ADMIN_BIN").unwrap())
        .args([
            "backup",
            "export",
            id,
            "--output",
            partial.to_str().unwrap(),
            "--socket",
            lab.state().join("admin/socket").to_str().unwrap(),
            "--json",
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..500 {
        if std::fs::metadata(format!("{}.verification.json", partial.display())).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    exporter.kill().unwrap();
    assert!(!exporter.wait().unwrap().success());
    assert_eq!(receipt(&partial)["state"], "incomplete");
    cli(&lab, &["verify", partial.to_str().unwrap()], false, false).await;
    let complete = std::fs::read(&archive).unwrap();
    let mut corrupt = complete.clone();
    let n = corrupt.len();
    corrupt[n - 5] ^= 1;
    std::fs::write(&archive, &corrupt).unwrap();
    cli(&lab, &["verify", archive.to_str().unwrap()], false, false).await;
    assert_eq!(receipt(&archive)["state"], "incomplete");
    std::fs::write(&archive, &complete[..complete.len() - 4]).unwrap();
    cli(&lab, &["verify", archive.to_str().unwrap()], false, false).await;
    std::fs::write(&archive, &complete).unwrap();
    cli(&lab, &["verify", archive.to_str().unwrap()], false, true).await;
    eprintln!(
        "native: complete manifest/objects/history/trash/schema/config and all archive bytes verified; corruption/truncation rejected"
    );

    // 第二个真实源先成功verify，再同长度破坏对象，重复verify不得复用旧成功。
    let bad = "88020000000000000000000000000002";
    cli(&lab, &["prepare", "--id", bad], true, true).await;
    cli(&lab, &["verify-source", bad], true, true).await;
    let snapshot = snapshot_path(&lab, bad);
    let object = snapshot.join(manifest["objects"][0]["archive_path"].as_str().unwrap());
    ro(&snapshot, false);
    std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o600)).unwrap();
    let original = std::fs::read(&object).unwrap();
    let mut changed = original.clone();
    changed[0] ^= 1;
    std::fs::write(&object, &changed).unwrap();
    ro(&snapshot, true);
    cli(&lab, &["verify-source", bad], true, false).await;
    assert_eq!(state(&lab, bad).await["integrity"], "incomplete");
    let bad_archive = volume.join(format!("s088-{}-bad.vsbk", std::process::id()));
    cli(
        &lab,
        &["export", bad, "--output", bad_archive.to_str().unwrap()],
        true,
        false,
    )
    .await;
    assert_eq!(receipt(&bad_archive)["state"], "incomplete");
    ro(&snapshot, false);
    std::fs::remove_file(&object).unwrap();
    ro(&snapshot, true);
    cli(&lab, &["verify-source", bad], true, false).await;
    assert_eq!(state(&lab, bad).await["integrity"], "incomplete");
    cli(&lab, &["release", bad], true, true).await;
    assert!(!snapshot.exists());
    assert_sources(&objects);
    eprintln!(
        "native: corrupt and missing source objects remain incomplete; release preserved all shared source objects"
    );

    // 跨越普通socket 10秒截止的真实流：空闲11秒后仍能读到完整终止标记。
    let mut slow = ipc::connect(&lab.state().join("admin/socket"))
        .await
        .unwrap();
    ipc::write_frame(
        &mut slow,
        &serde_json::to_vec(&["v1", "backup", "export", id]).unwrap(),
    )
    .await
    .unwrap();
    let slow_head: Value =
        serde_json::from_slice(&ipc::read_frame(&mut slow).await.unwrap()).unwrap();
    assert_eq!(slow_head["status"], 200);
    tokio::time::sleep(Duration::from_secs(11)).await;
    let mut verifier = ArchiveVerifier::new(
        *ContentDigest::from_hex(prepared["manifest_hash"].as_str().unwrap())
            .unwrap()
            .as_bytes(),
    );
    let mut chunk = vec![0; 65536];
    loop {
        use tokio::io::AsyncReadExt;
        let n = slow.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        verifier.push(&chunk[..n]).unwrap();
    }
    verifier.finish().unwrap();
    drop(slow);
    eprintln!(
        "native: socket export paused beyond ordinary 10-second deadline still verified all bytes"
    );
    // 暂停的真实socket导出持有读门；release持久入队，强杀后由重启自动续接。
    let mut paused = ipc::connect(&lab.state().join("admin/socket"))
        .await
        .unwrap();
    ipc::write_frame(
        &mut paused,
        &serde_json::to_vec(&["v1", "backup", "export", id]).unwrap(),
    )
    .await
    .unwrap();
    let head: Value = serde_json::from_slice(&ipc::read_frame(&mut paused).await.unwrap()).unwrap();
    assert_eq!(head["status"], 200);
    let accepted = rpc(&lab, "release", id).await;
    assert_eq!(accepted["status"], 202);
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert_eq!(state(&lab, id).await["job_pending"], true);
    assert!(snapshot_path(&lab, id).exists());
    s.kill();
    drop(paused);
    s = Server::start(&lab).await;
    wait_state(&lab, id, "released").await;
    assert!(!snapshot_path(&lab, id).exists());
    assert_sources(&objects);
    assert_eq!(
        cli(&lab, &["release", id], true, true).await["phase"],
        "released"
    );
    // 未知工件必须保留且状态releasing，移除人工故障后再次重启才可以安全继续。
    let failure = "88030000000000000000000000000003";
    cli(&lab, &["prepare", "--id", failure], true, true).await;
    let unknown = lab
        .state()
        .join("backups")
        .join(failure)
        .join("operator-evidence");
    std::fs::write(&unknown, b"must preserve").unwrap();
    cli(&lab, &["release", failure], true, false).await;
    assert_eq!(state(&lab, failure).await["phase"], "releasing");
    assert!(unknown.exists());
    s.kill();
    std::fs::remove_file(unknown).unwrap();
    s = Server::start(&lab).await;
    wait_state(&lab, failure, "released").await;
    assert_sources(&objects);
    eprintln!(
        "native: interrupted export/release and failed releasing restarted safely; unknown evidence and shared objects preserved"
    );
    s.kill();
    let database = vaultsyncd_server_store::db::Database::open(lab.state().join("server.sqlite"))
        .await
        .unwrap();
    sqlx::query("INSERT INTO users(id,login,password_hash,status) SELECT ?, 'second-owner',password_hash,'active' FROM users LIMIT 1").bind([0x88u8;16].as_slice()).execute(database.pool()).await.unwrap();
    database.close().await;
    s = Server::start(&lab).await;
    let other = serde_json::json!({"device_label":"S088-non-owner","login":"second-owner","password":common::PASSWORD});
    let (code, body) = s
        .http
        .json("POST", "/v1/auth/login", None, Some(&other))
        .await
        .unwrap();
    assert_eq!(code, 200, "{body}");
    let token = body["access_token"].as_str().unwrap();
    assert_eq!(
        s.http
            .json("GET", &format!("{url}/manifest"), Some(token), None)
            .await
            .unwrap()
            .0,
        403
    );
    // S105: every artifact/control endpoint rechecks owner, even with an existing exact ID.
    for (method, path) in [
        ("GET", "/v1/admin/backup-snapshots".to_owned()),
        ("GET", url.clone()),
        ("GET", format!("{url}/export")),
        ("GET", format!("{url}/files/database.sqlite")),
        ("GET", format!("{url}/objects/{root}/{}", "a".repeat(64))),
        ("POST", format!("{url}/verify")),
        ("POST", format!("{url}/release")),
    ] {
        let (status, rejected) = s.http.json(method, &path, Some(token), None).await.unwrap();
        assert_eq!(status, 403, "{method} {path}: {rejected}");
        assert_eq!(rejected["error"]["code"], "ACCESS_DENIED");
        assert!(!rejected.to_string().contains("history-content"));
    }
    assert_eq!(rpc(&lab, "status", id).await["status"], 409);
    eprintln!(
        "native: canceled export/verify keep recoverable incomplete receipts; second active account cannot read backup materials"
    );
    s.kill();
}
