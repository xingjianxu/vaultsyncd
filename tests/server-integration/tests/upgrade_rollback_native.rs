//! S091整套回退：真实一致备份→合成未来迁移失败→真实服务停写→S089新空目标恢复。
mod common;
use common::{Lab, Running, login, register_vault};
use serde_json::Value;
use std::{
    ffi::OsString,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
struct Server {
    child: Child,
    http: Running,
}
impl Server {
    async fn start(config: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_server-child"))
            .arg(config)
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
        panic!("server never ready")
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
async fn admin(args: Vec<OsString>) -> Value {
    let executable = std::env::var_os("VAULTSYNCD_S091_ADMIN_BIN").expect("built actual admin");
    let out = tokio::task::spawn_blocking(move || {
        Command::new(executable)
            .args(args)
            .arg("--json")
            .output()
            .unwrap()
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
async fn backup(lab: &Lab, args: &[&str]) -> Value {
    let mut all: Vec<OsString> = std::iter::once("backup")
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect();
    all.extend([
        "--socket".into(),
        lab.state().join("admin/socket").into_os_string(),
    ]);
    admin(all).await
}
fn hash(data: &[u8]) -> String {
    common::hex(&vaultsyncd_platform_linux_server::files::hash::digest(data))
}
async fn adopted(server: &Server, token: &str, vault: &str, name: &str, bytes: &[u8]) {
    for _ in 0..200 {
        let entries = common::snapshot(&server.http, token, vault).await;
        if entries
            .iter()
            .any(|e| e["name"] == name && e["hash"] == hash(bytes))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("observer did not collect {name}")
}

#[tokio::test]
#[ignore = "requires dedicated S091 btrfs volume and exact freshly built admin/store test helpers"]
async fn s091_full_archive_recovers_after_partial_future_upgrade_without_losing_history() {
    let volume = PathBuf::from(std::env::var_os("VAULTSYNCD_S091_VOLUME").expect("dedicated run"));
    assert!(volume.starts_with("/data/vaultsyncd-lab/runs/"));
    assert!(
        volume
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with("s091-"))
    );
    let base = volume.join(format!("run-{}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    let lab = Lab::new("u", Some(&base), Some(&base));
    lab.init_owner().await;
    std::fs::write(lab.work().join("note"), b"before upgrade history").unwrap();
    std::fs::write(lab.work().join("trash"), b"retained trash bytes").unwrap();
    let _ = lab.config();
    let source = Server::start(&lab.config_path()).await;
    let token = login(&source.http).await.unwrap();
    let vault = register_vault(&source.http, &token, &lab.work())
        .await
        .unwrap();
    adopted(&source, &token, &vault, "note", b"before upgrade history").await;
    adopted(&source, &token, &vault, "trash", b"retained trash bytes").await;
    let entries = common::snapshot(&source.http, &token, &vault).await;
    let note = common::entry_named(&entries, "note");
    let entry = note["entry_id"].as_str().unwrap().to_owned();
    let version = note["version_id"].as_str().unwrap().to_owned();
    std::fs::write(lab.work().join("note"), b"current committed bytes").unwrap();
    adopted(&source, &token, &vault, "note", b"current committed bytes").await;
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
        if value["items"].as_array().is_some_and(|v| !v.is_empty()) {
            trash = value;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!trash.is_null());
    let (code, prior_capabilities) = source
        .http
        .json("GET", "/v1/capabilities", None, None)
        .await
        .unwrap();
    assert_eq!(code, 200);
    let id = "91010000000000000000000000000001";
    backup(&lab, &["prepare", "--id", id]).await;
    let archive = base.join("consistent.vsbk");
    backup(&lab, &["export", id, "--output", archive.to_str().unwrap()]).await;
    assert_eq!(
        admin(vec![
            "backup".into(),
            "verify".into(),
            archive.as_os_str().into()
        ])
        .await["state"],
        "verified"
    );
    source.kill();
    let actor = std::env::var_os("VAULTSYNCD_S091_SERVER_TEST_BIN")
        .expect("fresh server-store test binary");
    let state = lab.state();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(actor)
            .env("VAULTSYNCD_S091_ACTOR_STATE", state)
            .args([
                "db::s091::s091_server_upgrade_actor",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("S091 synthetic future migration"));
    eprintln!("{text}");
    let result = vaultsyncd_server::bootstrap::prepare(lab.config()).await;
    assert!(matches!(
        result,
        Err(vaultsyncd_server::bootstrap::StartupError::Database(
            vaultsyncd_server_store::db::DbError::SchemaTooNew { .. }
        ))
    ));
    assert_eq!(
        std::fs::read(lab.work().join("note")).unwrap(),
        b"current committed bytes"
    );
    let state = base.join("restored-state");
    let work = base.join("restored-work");
    let extra = base.join("extra-work");
    for p in [&state, &work, &extra] {
        std::fs::create_dir(p).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let args = |op: &str| {
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
    };
    admin(args("validate")).await;
    let restored = admin(args("apply")).await;
    assert_eq!(restored["state"], "restored");
    assert_eq!(restored["server_id"], prior_capabilities["server_id"]);
    assert_ne!(restored["epoch"], prior_capabilities["epoch"]);
    assert_eq!(
        std::fs::read(work.join("note")).unwrap(),
        b"current committed bytes"
    );
    assert!(!work.join("trash").exists());
    // 管理员在本run内显式允许额外空Vault，用于核对恢复后的新注册也属于新epoch。
    let config = state.join("server.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    let text = text
        .lines()
        .map(|l| {
            if l.starts_with("allowed_bases=") {
                format!(
                    "allowed_bases=[{}]",
                    serde_json::to_string(base.to_str().unwrap()).unwrap()
                )
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&config, text).unwrap();
    let running = Server::start(&config).await;
    let fresh = login(&running.http).await.unwrap();
    let (code, bytes, _) = running
        .http
        .download(
            &format!("/v1/vaults/{vault}/entries/{entry}/versions/{version}/content"),
            &fresh,
        )
        .await
        .unwrap();
    assert_eq!(code, 200);
    assert_eq!(bytes, b"before upgrade history");
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
    let (code, caps) = running
        .http
        .json("GET", "/v1/capabilities", None, None)
        .await
        .unwrap();
    assert_eq!(code, 200);
    assert_eq!(caps["server_id"], prior_capabilities["server_id"]);
    assert_eq!(caps["epoch"], restored["epoch"]);
    let extra_vault = register_vault(&running.http, &fresh, &extra).await.unwrap();
    assert_eq!(
        common::epoch_of(&running.http, &fresh, &extra_vault).await,
        caps["epoch"]
    );
    running.kill();
    println!(
        "S091 native full rollback=verified partial_future_schema=refused archive=complete server_id=retained new_epoch=verified current_and_history_bytes=verified trash=retained new_vault_epoch=consistent; future migrations are injected fixtures, not released versions"
    );
}
