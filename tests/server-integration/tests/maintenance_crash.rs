//! S104：真实 HTTP/SQLite/btrfs 的 GC、下载、提交和备份联合故障验收。
#![cfg(feature = "test-faults")]
mod common;
#[path = "s104/download.rs"]
mod download_faults;

use common::{
    Lab, Running, db, entry_named, epoch_of, identity, login, register_vault, snapshot, submit,
    upload,
};
use serde_json::{Value, json};
use sqlx::{Connection, Row};
use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use vaultsyncd_platform_linux_server::{files::hash::digest, ipc};
use vaultsyncd_server_core::objects::ContentDigest;

const OLD: &[u8] = b"old confirmed bytes before crash\n";
const NEW: &[u8] = b"new confirmed bytes after crash\n";
const BACKUP: &str = "104b0000000000000000000000000001";
const PUT_POINTS: [&str; 10] = [
    "put_pending",
    "put_before_prepared",
    "put_after_prepared",
    "put_before_publish",
    "put_after_publish",
    "put_before_published",
    "put_after_published",
    "put_before_metadata",
    "put_after_metadata",
    "put_before_ack",
];

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
fn hash(bytes: &[u8]) -> String {
    ContentDigest::from_bytes(digest(bytes)).to_hex()
}
fn volume() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("VAULTSYNCD_S104_VOLUME").expect("dedicated S104 btrfs volume"),
    );
    assert!(path.starts_with("/data/vaultsyncd-dev/vaultsyncd-lab/runs/"));
    assert!(
        path.components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with("s104-"))
    );
    path
}
fn trace(value: Value) {
    eprintln!("S104 {value}");
    let path = std::env::var_os("VAULTSYNCD_S104_TRACE").expect("durable minimum trace path");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    writeln!(f, "{value}").unwrap();
    f.sync_all().unwrap();
}
fn control(lab: &Lab) -> PathBuf {
    let path = lab.state().join("s104-control");
    if !path.exists() {
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    path
}
fn arm(lab: &Lab, points: &[&str]) {
    std::fs::write(control(lab).join("armed"), points.join("\n")).unwrap();
}
async fn wait_path(path: &Path) {
    for _ in 0..1500 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("boundary timeout: {}", path.display());
}
async fn reached(lab: &Lab, point: &str) {
    wait_path(&control(lab).join(format!("{point}.reached"))).await;
    trace(
        json!({"event":"reached","point":point,"pid":std::fs::read_to_string(control(lab).join(format!("{point}.reached"))).unwrap()}),
    );
}

struct Server {
    child: Child,
    http: std::sync::Arc<Running>,
}
impl Server {
    async fn start(lab: &Lab) -> Self {
        let _ = lab.config();
        Self::config(&lab.config_path(), &control(lab)).await
    }
    async fn config(config: &Path, control: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_s104-child"))
            .arg(config)
            .arg(control)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let line = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::task::spawn_blocking(move || {
                let mut line = String::new();
                BufReader::new(output).read_line(&mut line).unwrap();
                line
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let addr = line
            .trim()
            .strip_prefix("LISTEN ")
            .unwrap_or_else(|| panic!("server failed: {line:?}"));
        let http = std::sync::Arc::new(Running::attach(format!("http://{addr}")));
        for _ in 0..100 {
            if http
                .json("GET", "/v1/health", None, None)
                .await
                .is_ok_and(|r| r.0 == 200)
            {
                return Self { child, http };
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("not healthy");
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

async fn rpc(lab: &Lab, action: &str, id: &str) -> Value {
    let mut socket = ipc::connect(&lab.state().join("admin/socket"))
        .await
        .unwrap();
    ipc::write_frame(
        &mut socket,
        &serde_json::to_vec(&["v1", "backup", action, id]).unwrap(),
    )
    .await
    .unwrap();
    let response: Value =
        serde_json::from_slice(&ipc::read_frame(&mut socket).await.unwrap()).unwrap();
    assert!(
        response["status"] == 200 || response["status"] == 202,
        "{response}"
    );
    response["result"].clone()
}
async fn backup_phase(lab: &Lab, id: &str, phase: &str) -> Value {
    for _ in 0..300 {
        let value = rpc(lab, "status", id).await;
        if value["phase"] == phase && value["job_pending"] == false {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "backup never became {phase}: {}",
        rpc(lab, "status", id).await
    );
}
async fn prepare(lab: &Lab, id: &str) -> Value {
    rpc(lab, "prepare", id).await;
    backup_phase(lab, id, "ready").await
}
async fn release(lab: &Lab, id: &str) {
    rpc(lab, "release", id).await;
    backup_phase(lab, id, "released").await;
}
async fn gc_start(lab: &Lab, time: i64) {
    for name in ["gc-started", "gc-result"] {
        let _ = std::fs::remove_file(control(lab).join(name));
    }
    // 消费者轮询可见名字；必须先完整写入/刷盘再原子发布，不能读到create后的空前缀。
    let temporary = control(lab).join("gc-request.pending");
    std::fs::write(&temporary, time.to_string()).unwrap();
    std::fs::File::open(&temporary).unwrap().sync_all().unwrap();
    std::fs::rename(temporary, control(lab).join("gc-request")).unwrap();
    std::fs::File::open(control(lab))
        .unwrap()
        .sync_all()
        .unwrap();
    wait_path(&control(lab).join("gc-started")).await;
}
async fn gc_done(lab: &Lab) -> String {
    wait_path(&control(lab).join("gc-result")).await;
    let report = std::fs::read_to_string(control(lab).join("gc-result")).unwrap();
    assert!(report.starts_with("ok "), "{report}");
    trace(json!({"event":"gc","report":report}));
    report
}
async fn gc(lab: &Lab, time: i64) -> String {
    gc_start(lab, time).await;
    gc_done(lab).await
}
async fn adopt(s: &Server, token: &str, vault: &str, name: &str, bytes: &[u8]) -> Value {
    for _ in 0..200 {
        let entries = snapshot(&s.http, token, vault).await;
        if let Some(e) = entries
            .iter()
            .find(|e| e["name"] == name && e["hash"] == hash(bytes))
        {
            return e.clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("not adopted {name}");
}
async fn setup(label: &str, bytes: &[u8]) -> (Lab, Server, String, String, Value) {
    let volume = volume();
    let lab = Lab::new(label, Some(&volume), Some(&volume));
    lab.init_owner().await;
    std::fs::write(lab.work().join("note"), bytes).unwrap();
    let server = Server::start(&lab).await;
    let token = login(&server.http).await.unwrap();
    let vault = register_vault(&server.http, &token, &lab.work())
        .await
        .unwrap();
    let entry = adopt(&server, &token, &vault, "note", bytes).await;
    (lab, server, token, vault, entry)
}
fn content_path(vault: &str, e: &Value) -> String {
    format!(
        "/v1/vaults/{vault}/entries/{}/versions/{}/content",
        e["entry_id"].as_str().unwrap(),
        e["version_id"].as_str().unwrap()
    )
}
async fn all_objects(lab: &Lab) -> usize {
    let mut conn = db(lab).await;
    let rows = sqlx::query("SELECT hash,size,storage_path FROM objects WHERE gc_state='live'")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    for row in &rows {
        let bytes = std::fs::read(
            lab.state()
                .join("objects")
                .join(row.get::<String, _>("storage_path")),
        )
        .unwrap();
        assert_eq!(bytes.len() as i64, row.get::<i64, _>("size"));
        assert_eq!(digest(&bytes).as_slice(), row.get::<Vec<u8>, _>("hash"));
    }
    let dangling: i64 = sqlx::query_scalar("SELECT count(*) FROM versions v LEFT JOIN objects o ON o.vault_id=v.vault_id AND o.hash=v.object_hash WHERE o.hash IS NULL").fetch_one(&mut conn).await.unwrap();
    assert_eq!(dangling, 0);
    rows.len()
}
async fn facts(lab: &Lab, point: &str) {
    let mut conn = db(lab).await;
    let ops: Vec<(String,)> = sqlx::query_as("SELECT state FROM operations ORDER BY op_id")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    let intents: Vec<(String,)> = sqlx::query_as("SELECT phase FROM fs_intents ORDER BY intent_id")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    let versions: i64 = sqlx::query_scalar("SELECT count(*) FROM versions")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    trace(
        json!({"event":"disk-facts","point":point,"operations":ops,"intents":intents,"versions":versions,"work_hash":hash(&std::fs::read(lab.work().join("note")).unwrap())}),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "S104 dedicated btrfs and explicit test-faults helper required"]
async fn each_real_put_boundary_preserves_download_backup_and_gc_references() {
    for point in PUT_POINTS {
        trace(json!({"event":"case-start","point":point}));
        let (lab, server, token, vault, entry) = setup(point, OLD).await;
        let epoch = epoch_of(&server.http, &token, &vault).await;
        let (upload_id, _, _) = upload(&server.http, &token, &vault, NEW, 16).await.unwrap();
        prepare(&lab, BACKUP).await;
        // 固定交错：后台GC先停在独占门之外，避免writer优先调度让第二个真实读者
        // 在故障装置的无限暂停期间排到GC后。随后仅显式GC竞争同一个真实门。
        arm(&lab, &["gc_before_lock"]);
        reached(&lab, "gc_before_lock").await;
        // 已真实读取一段，下一段在持久下载租约建立后停住；杀服务即真正断开 TCP。
        let url = content_path(&vault, &entry);
        let http = reqwest::Client::new();
        let first = http
            .get(server.http.uri(&url))
            .bearer_auth(&token)
            .header("range", "bytes=0-7")
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), 206);
        assert_eq!(first.bytes().await.unwrap(), &OLD[..8]);
        arm(&lab, &["download_open", point]);
        let pending_download = {
            let target = server.http.uri(&url);
            let token = token.clone();
            tokio::spawn(async move {
                reqwest::Client::new()
                    .get(target)
                    .bearer_auth(token)
                    .header("range", format!("bytes=8-{}", OLD.len() - 1))
                    .send()
                    .await
            })
        };
        reached(&lab, "download_open").await;
        let mut conn = db(&lab).await;
        let leases: i64 =
            sqlx::query_scalar("SELECT count(*) FROM object_leases WHERE purpose='download'")
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(leases, 1, "active real download lease");
        drop(conn);
        let body = json!({"op_id":identity(0x41),"device_id":identity(0x42),"server_epoch":epoch,
            "kind":"put_content","entry_id":entry["entry_id"],"base_revision":entry["revision"],
            "upload_id":upload_id,"local_generation":"104","conflict_policy":"preserve_both"});
        let pending_put = {
            let http = server.http.clone();
            let token = token.clone();
            let vault = vault.clone();
            let body = body.clone();
            tokio::spawn(async move {
                http.json(
                    "POST",
                    &format!("/v1/vaults/{vault}/operations"),
                    Some(&token),
                    Some(&body),
                )
                .await
            })
        };
        reached(&lab, point).await;
        gc_start(&lab, now()).await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            !control(&lab).join("gc-result").exists(),
            "GC must wait for real shared object gate"
        );
        // before_metadata 正在持写事务，只读 WAL 查询仍能核对提交前事实。
        facts(&lab, point).await;
        server.kill();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), pending_download)
                .await
                .unwrap()
                .unwrap()
                .is_err(),
            "interrupted request must not supply empty success"
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(3), pending_put)
                .await
                .unwrap()
                .unwrap()
                .is_err(),
            "parked operation cannot send a committed ACK before SIGKILL"
        );
        arm(&lab, &[]);
        let server = Server::start(&lab).await;
        let token = login(&server.http).await.unwrap();
        let bytes = std::fs::read(lab.work().join("note")).unwrap();
        if matches!(point, "put_pending" | "put_before_prepared") {
            assert_eq!(bytes, OLD);
            let (code, result) = server
                .http
                .json(
                    "GET",
                    &format!(
                        "/v1/vaults/{vault}/operations/{}?device_id={}",
                        identity(0x41),
                        identity(0x42)
                    ),
                    Some(&token),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(code, 202, "{result}");
            assert_eq!(
                result["state"], "pending",
                "no durable intent cannot invent a commit"
            );
        } else {
            assert_eq!(bytes, NEW);
            let (code, result) = submit(&server.http, &token, &vault, body.clone()).await;
            assert_eq!(code, 200, "{point}: {result}");
            assert_eq!(result["state"], "committed");
            assert_eq!(result["replayed"], true);
            let second = submit(&server.http, &token, &vault, body).await.1;
            assert_eq!(second, result, "replay remains identical");
        }
        let rest = reqwest::Client::new()
            .get(server.http.uri(&url))
            .bearer_auth(&token)
            .header("range", format!("bytes=8-{}", OLD.len() - 1))
            .send()
            .await
            .unwrap();
        assert_eq!(rest.status(), 206);
        let rest = rest.bytes().await.unwrap();
        let full = [&OLD[..8], rest.as_ref()].concat();
        assert_eq!(full, OLD);
        let report = gc(&lab, now() + 3601).await;
        assert!(report.contains("objects=0"));
        assert!(all_objects(&lab).await >= 2);
        let mut conn = db(&lab).await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM versions")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(
            count,
            if matches!(point, "put_pending" | "put_before_prepared") {
                1
            } else {
                2
            }
        );
        drop(conn);
        facts(&lab, &format!("{point}-restarted")).await;
        release(&lab, BACKUP).await;
        server.kill();
        trace(
            json!({"event":"case-pass","point":point,"resumed_bytes":OLD.len()-8,"retransmitted_prefix_bytes":0}),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "S104 dedicated btrfs and explicit test-faults helper required"]
async fn snapshot_prepare_and_release_sigkill_are_explainable_and_idempotent() {
    for point in [
        "backup_preparing",
        "backup_leased",
        "backup_frozen",
        "backup_ready",
        "backup_releasing",
        "backup_removed",
    ] {
        let (lab, server, _, _, _) = setup(point, OLD).await;
        let releasing = point.starts_with("backup_releas") || point == "backup_removed";
        if releasing {
            prepare(&lab, BACKUP).await;
        }
        arm(&lab, &[point]);
        rpc(&lab, if releasing { "release" } else { "prepare" }, BACKUP).await;
        reached(&lab, point).await;
        if point == "backup_frozen" {
            let output = Command::new("btrfs")
                .args(["property", "get", "-ts"])
                .arg(lab.state().join("backups").join(BACKUP).join("snapshot"))
                .arg("ro")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, b"ro=true\n");
        }
        server.kill();
        arm(&lab, &[]);
        let server = Server::start(&lab).await;
        let phase = if releasing {
            "released"
        } else if point == "backup_ready" {
            "ready"
        } else {
            "failed"
        };
        let status = backup_phase(&lab, BACKUP, phase).await;
        let again = rpc(&lab, if releasing { "release" } else { "prepare" }, BACKUP).await;
        assert_eq!(again["phase"], phase);
        assert_eq!(all_objects(&lab).await, 1);
        let mut conn = db(&lab).await;
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM backup_leases")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        drop(conn);
        assert_eq!(n, i64::from(phase == "ready"));
        if phase != "ready" {
            assert!(!lab.state().join("backups").join(BACKUP).exists());
        } else {
            release(&lab, BACKUP).await;
        }
        server.kill();
        trace(
            json!({"event":"backup-crash-pass","point":point,"recovered":status,"lease_count":n}),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "S104 dedicated btrfs and explicit test-faults helper required"]
async fn real_gc_mark_authorize_unlink_sigkill_never_removes_current_objects() {
    for point in ["gc_marked", "gc_deleting", "gc_unlinked"] {
        let (lab, server, token, vault, _) = setup(point, OLD).await;
        upload(&server.http, &token, &vault, NEW, 16).await.unwrap();
        server.kill();
        // 此 fixture 的上传已完成但从未提交；停服持同一StateLock老化会话，不改产品保留期。
        let lock =
            vaultsyncd_server_store::state_lock::StateLock::try_acquire_exclusive(&lab.state())
                .unwrap();
        let mut conn = db(&lab).await;
        sqlx::query("UPDATE uploads SET expires_at=1")
            .execute(&mut conn)
            .await
            .unwrap();
        drop(conn);
        drop(lock);
        let server = Server::start(&lab).await;
        if point != "gc_marked" {
            assert!(gc(&lab, now()).await.contains("objects=0"));
        }
        arm(&lab, &[point]);
        gc_start(&lab, now() + 3601).await;
        reached(&lab, point).await;
        server.kill();
        arm(&lab, &[]);
        let server = Server::start(&lab).await;
        let result = gc(&lab, now() + 7202).await;
        assert!(
            result.contains("objects=1") || result.contains("objects=0"),
            "{point}: {result}"
        );
        // 产品观察循环也会在启动时真实运行GC；deleting可由它先补完，显式轮次随后幂等返回0。
        let mut conn = db(&lab).await;
        let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM gc_candidates")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(pending, 0);
        drop(conn);
        assert!(gc(&lab, now() + 7203).await.contains("objects=0"));
        assert_eq!(all_objects(&lab).await, 1);
        assert_eq!(std::fs::read(lab.work().join("note")).unwrap(), OLD);
        server.kill();
        trace(json!({"event":"gc-crash-pass","point":point}));
    }
}

async fn admin(args: Vec<OsString>) -> Value {
    let exe = std::env::var_os("VAULTSYNCD_ADMIN_BIN").expect("actual admin binary");
    let out = tokio::task::spawn_blocking(move || {
        Command::new(exe).args(args).arg("--json").output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
async fn backup_cli(lab: &Lab, args: &[&str]) -> Value {
    let mut args: Vec<OsString> = std::iter::once("backup")
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect();
    args.extend([
        "--socket".into(),
        lab.state().join("admin/socket").into_os_string(),
    ]);
    admin(args).await
}
fn deterministic(size: usize) -> Vec<u8> {
    let mut state = 0x104b_7c89_2134_89abu64;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}
fn usage(paths: &[&Path]) -> (u64, u64, u64, String) {
    let out = Command::new("btrfs")
        .args(["filesystem", "du", "--raw", "--summarize"])
        .args(paths)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let mut total = (0, 0, 0);
    for line in text.lines().skip(1) {
        let columns: Vec<_> = line.split_whitespace().collect();
        assert!(columns.len() >= 4, "{text}");
        total.0 += columns[0].parse::<u64>().unwrap();
        total.1 += columns[1].parse::<u64>().unwrap();
        total.2 += columns[2].parse::<u64>().unwrap();
    }
    (total.0, total.1, total.2, text)
}
fn filesystem_sync(path: &Path) {
    let out = Command::new("btrfs")
        .args(["filesystem", "sync"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "S104 dedicated btrfs, admin and explicit test-faults helper required"]
async fn expired_live_objects_remain_in_snapshot_and_complete_archive_restores_every_reference() {
    let old = deterministic(16 * 1024 * 1024);
    let (lab, server, token, vault, old_entry) = setup("snapshot-extents", &old).await;
    let epoch = epoch_of(&server.http, &token, &vault).await;
    let (uploaded, _, _) = upload(&server.http, &token, &vault, NEW, 16).await.unwrap();
    let (code, committed) = submit(&server.http, &token, &vault, json!({
        "op_id":identity(0x51),"device_id":identity(0x52),"server_epoch":epoch,"kind":"put_content",
        "entry_id":old_entry["entry_id"],"base_revision":old_entry["revision"],"upload_id":uploaded,
        "local_generation":"2","conflict_policy":"preserve_both"
    })).await;
    assert_eq!(code, 200, "{committed}");
    std::fs::create_dir(lab.work().join("nested")).unwrap();
    std::fs::write(lab.work().join("nested/current"), b"nested current bytes").unwrap();
    std::fs::write(lab.work().join("trash"), b"trash bytes retained in backup").unwrap();
    adopt(&server, &token, &vault, "current", b"nested current bytes").await;
    adopt(
        &server,
        &token,
        &vault,
        "trash",
        b"trash bytes retained in backup",
    )
    .await;
    std::fs::remove_file(lab.work().join("trash")).unwrap();
    for _ in 0..200 {
        let (code, trash) = server
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
        if trash["items"].as_array().is_some_and(|x| !x.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.kill();
    // 仅停服的本fixture历史时间老化，真实GC继续使用产品30天/1小时规则。
    let lock = vaultsyncd_server_store::state_lock::StateLock::try_acquire_exclusive(&lab.state())
        .unwrap();
    let mut conn = db(&lab).await;
    sqlx::query("UPDATE versions SET superseded_at=1 WHERE superseded_at IS NOT NULL")
        .execute(&mut conn)
        .await
        .unwrap();
    drop(conn);
    drop(lock);
    let server = Server::start(&lab).await;
    let prepared = backup_cli(&lab, &["prepare", "--id", BACKUP]).await;
    assert_eq!(prepared["phase"], "ready");
    let snap = lab.state().join("backups").join(BACKUP).join("snapshot");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(snap.join("manifest.json")).unwrap()).unwrap();
    assert!(
        manifest["database_contents"]["trash_entries"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    let old_object = manifest["objects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["sha256"] == hash(&old))
        .unwrap();
    let live = lab
        .state()
        .join("objects")
        .join(old_object["storage_path"].as_str().unwrap());
    let frozen = snap.join(old_object["archive_path"].as_str().unwrap());
    filesystem_sync(&volume());
    let before = usage(&[&live, &frozen]);
    assert!(
        before.2 >= old.len() as u64,
        "actual shared extents must be measured: {}",
        before.3
    );
    assert!(gc(&lab, now()).await.contains("objects=0"));
    assert!(
        gc(&lab, now() + 3601).await.contains("objects=0"),
        "active backup lease protects expired history"
    );
    assert!(live.exists());
    // 使用确定性测试时钟跨过备份租约到期；只读快照尚未清理的窗口不会立即释放其extent。
    let after_expiry = now() + 86401;
    assert!(gc(&lab, after_expiry).await.contains("objects=0"));
    assert!(gc(&lab, after_expiry + 3601).await.contains("objects=1"));
    assert!(!live.exists());
    assert_eq!(digest(&std::fs::read(&frozen).unwrap()), digest(&old));
    filesystem_sync(&volume());
    let held = usage(&[&frozen]);
    assert_eq!(
        held.0,
        old.len() as u64,
        "snapshot still accounts for all physical content"
    );
    trace(
        json!({"event":"extent-accounting","before":before.3,"after_live_gc":held.3,"logical_live_deleted":old.len(),"snapshot_content_retained":old.len(),"clock":"GC-only deterministic lease expiry; product retention unchanged"}),
    );

    backup_cli(&lab, &["verify-source", BACKUP]).await;
    let archive = lab.state().parent().unwrap().join("complete.vsbk");
    backup_cli(
        &lab,
        &["export", BACKUP, "--output", archive.to_str().unwrap()],
    )
    .await;
    let verified = admin(vec![
        "backup".into(),
        "verify".into(),
        archive.as_os_str().into(),
    ])
    .await;
    assert_eq!(verified["state"], "verified");
    let restored = lab.state().parent().unwrap().join("restored");
    let work = lab.vault().join("restored-work");
    for dir in [&restored, &work] {
        std::fs::create_dir(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let restored_report = admin(vec![
        "restore".into(),
        "apply".into(),
        "--archive".into(),
        archive.as_os_str().into(),
        "--state-dir".into(),
        restored.as_os_str().into(),
        "--vault".into(),
        format!("{vault}={}", work.display()).into(),
    ])
    .await;
    assert_eq!(restored_report["state"], "restored");
    let mut conn = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(restored.join("server.sqlite"))
            .read_only(true),
    )
    .await
    .unwrap();
    let rows = sqlx::query("SELECT hash,size,storage_path FROM objects")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    assert_eq!(rows.len(), manifest["objects"].as_array().unwrap().len());
    for row in &rows {
        let bytes = std::fs::read(
            restored
                .join("objects")
                .join(row.get::<String, _>("storage_path")),
        )
        .unwrap();
        assert_eq!(digest(&bytes).as_slice(), row.get::<Vec<u8>, _>("hash"));
        assert_eq!(bytes.len() as i64, row.get::<i64, _>("size"));
    }
    let dangling:i64=sqlx::query_scalar("SELECT count(*) FROM versions v LEFT JOIN objects o ON o.vault_id=v.vault_id AND o.hash=v.object_hash WHERE o.hash IS NULL").fetch_one(&mut conn).await.unwrap();
    assert_eq!(dangling, 0);
    for table in ["versions", "entries", "trash_entries"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count.to_string(), manifest["database_contents"][table]);
    }
    drop(conn);
    assert_eq!(std::fs::read(work.join("note")).unwrap(), NEW);
    assert_eq!(
        std::fs::read(work.join("nested/current")).unwrap(),
        b"nested current bytes"
    );
    let restored_control = restored.join("s104-control");
    std::fs::create_dir(&restored_control).unwrap();
    let config = restored.join("restored-server.toml");
    std::fs::write(&config,format!("[server]\nlisten=\"127.0.0.1:0\"\n[state]\ndir=\"{}\"\n[vaults]\nallowed_bases=[\"{}\"]\n",restored.display(),work.display())).unwrap();
    let target = Server::config(&config, &restored_control).await;
    let token = login(&target.http).await.unwrap();
    let (status, bytes, _) = target
        .http
        .download(&content_path(&vault, &old_entry), &token)
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(digest(&bytes), digest(&old));
    let entries = snapshot(&target.http, &token, &vault).await;
    assert_eq!(entry_named(&entries, "note")["hash"], hash(NEW));
    target.kill();
    release(&lab, BACKUP).await;
    server.kill();
    trace(
        json!({"event":"complete-restore-pass","objects":rows.len(),"dangling":dangling,"history_bytes":old.len(),"restore":restored_report}),
    );
}
