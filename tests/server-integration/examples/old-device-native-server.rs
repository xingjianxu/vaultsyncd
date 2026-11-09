//! S102 专属测试控制器；只绑定回环，通过本 run 的 SSH 隧道访问。
//! 故障 SQL 只在服务停止并持 StateLock 时执行，绝不修改在线数据库。
#[path = "../tests/common/mod.rs"]
mod common;
use common::*;
use serde_json::{Value, json};
use std::{
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, oneshot},
};
use vaultsyncd_server::{bootstrap, config::Config};

struct Service {
    http: Running,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}
impl Service {
    async fn start(config: Config) -> Self {
        let service = bootstrap::prepare(config).await.expect("bootstrap");
        let address = service.local_addr().unwrap();
        let (stop, wait) = oneshot::channel();
        let task = tokio::spawn(async move {
            service
                .serve(async {
                    let _ = wait.await;
                })
                .await
                .expect("serve");
        });
        Self {
            http: Running::attach(format!("http://{address}")),
            stop,
            task,
        }
    }
    async fn stop(self) {
        let _ = self.stop.send(());
        self.task.await.unwrap();
    }
}
struct Host {
    lab: Lab,
    service: Option<Service>,
    vault: String,
    state: PathBuf,
    work: PathBuf,
    archive: PathBuf,
    restored: bool,
}
impl Host {
    async fn admin(&self, args: Vec<String>) -> Value {
        let binary = std::env::var_os("VAULTSYNCD_ADMIN_BIN").expect("admin binary");
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(binary)
                .args(args)
                .arg("--json")
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "admin exit={:?}: {} {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    async fn backup(&self, args: &[&str]) -> Value {
        let mut command = vec!["backup".to_owned()];
        command.extend(args.iter().map(|s| (*s).to_owned()));
        command.extend([
            "--socket".into(),
            self.state.join("admin/socket").display().to_string(),
        ]);
        self.admin(command).await
    }
    async fn control(&mut self, action: &str) -> Value {
        match action {
            "backup" => {
                let id = "51020000000000000000000000000001";
                self.backup(&["prepare", "--id", id]).await;
                self.backup(&["verify-source", id]).await;
                self.backup(&["export", id, "--output", self.archive.to_str().unwrap()])
                    .await;
                self.admin(vec![
                    "backup".into(),
                    "verify".into(),
                    self.archive.display().to_string(),
                ])
                .await;
                self.backup(&["release", id]).await;
                json!({"backup":"verified"})
            }
            "expire_cursor" | "expire_history" => {
                self.service.take().unwrap().stop().await;
                let _lock = vaultsyncd_server_store::state_lock::StateLock::try_acquire_exclusive(
                    &self.state,
                )
                .expect("offline lock");
                let db =
                    vaultsyncd_server_store::db::Database::open(self.state.join("server.sqlite"))
                        .await
                        .unwrap();
                let mut tx = db.begin_write().await.unwrap();
                let changes = sqlx::query("UPDATE changes SET created_at=1")
                    .execute(&mut *tx)
                    .await
                    .unwrap()
                    .rows_affected();
                let mut versions = 0;
                if action == "expire_history" {
                    versions = sqlx::query(
                        "UPDATE versions SET superseded_at=1 WHERE superseded_at IS NOT NULL",
                    )
                    .execute(&mut *tx)
                    .await
                    .unwrap()
                    .rows_affected();
                    sqlx::query("UPDATE trash_entries SET deleted_at=1,expires_at=2")
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                    sqlx::query("UPDATE tombstones SET deleted_at=1,expires_at=2")
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                    sqlx::query("UPDATE entries SET deleted_at=1 WHERE deleted_at IS NOT NULL")
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                }
                tx.commit().await.unwrap();
                let mut collection = Vec::new();
                if action == "expire_history" {
                    // Exercise real mark/revalidate/unlink, with a fixture-only clock crossing
                    // the grace period. No product defaults or online SQL are changed.
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64;
                    for time in [now, now + 3601] {
                        let report = vaultsyncd_server::gc::run_once(
                            &db,
                            &self.state,
                            vaultsyncd_domain::version::UtcTimestamp::from_unix_seconds(time)
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                        collection.push(format!("{report:?}"));
                    }
                }
                db.close().await;
                drop(_lock);
                self.service = Some(Service::start(self.lab.config()).await);
                json!({"offline":true,"changes_aged":changes,"versions_aged":versions,"gc":collection,"fixture_time":1,"product_retention_unchanged":true})
            }
            "restore" => {
                assert!(!self.restored, "restore once per run");
                self.service.take().unwrap().stop().await;
                let parent = self.lab.state().parent().unwrap().to_path_buf();
                let state = parent.join("restored-state");
                let work = parent.join("restored-work");
                std::fs::create_dir(&state).unwrap();
                std::fs::create_dir(&work).unwrap();
                for path in [&state, &work] {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
                }
                let args = vec![
                    "restore".into(),
                    "apply".into(),
                    "--archive".into(),
                    self.archive.display().to_string(),
                    "--state-dir".into(),
                    state.display().to_string(),
                    "--vault".into(),
                    format!("{}={}", self.vault, work.display()),
                ];
                let result = self.admin(args).await;
                let config_path = state.join("server.toml");
                let config = Config::parse(
                    &std::fs::read_to_string(&config_path).unwrap(),
                    &config_path,
                )
                .unwrap();
                self.state = state;
                self.work = work;
                self.restored = true;
                self.service = Some(Service::start(config).await);
                result
            }
            action if action.starts_with("event-") => {
                assert!(matches!(
                    action,
                    "event-loss-first"
                        | "event-loss-second"
                        | "event-duplicate-first"
                        | "event-duplicate-second"
                        | "event-reorder-first"
                        | "event-reorder-second"
                ));
                let content = format!("S102 remote {action}\n");
                let hash = hex(&vaultsyncd_platform_linux_server::files::hash::digest(
                    content.as_bytes(),
                ));
                std::fs::write(self.work.join("event.txt"), content).unwrap();
                let http = &self.service.as_ref().unwrap().http;
                let token = common::login(http).await.unwrap();
                wait_for("remote event adopted", || async {
                    Ok(common::snapshot(http, &token, &self.vault)
                        .await
                        .iter()
                        .any(|row| row["name"] == "event.txt" && row["hash"] == hash))
                })
                .await
                .unwrap();
                json!({"external_event":action,"hash":hash})
            }
            "info" => json!({"work_dir":self.work,"state_dir":self.state,"restored":self.restored}),
            _ => panic!("unknown test control action"),
        }
    }
}

#[tokio::main]
async fn main() {
    tokio::task::LocalSet::new().run_until(main_local()).await;
}
async fn main_local() {
    let manifest = PathBuf::from(std::env::args_os().nth(1).expect("manifest"));
    let stop = PathBuf::from(std::env::args_os().nth(2).expect("stop file"));
    let volume =
        PathBuf::from(std::env::var_os("VAULTSYNCD_S102_VOLUME").expect("dedicated btrfs volume"));
    let lab = Lab::new("s102-old-device", Some(&volume), Some(&volume));
    lab.init_owner().await;
    let service = Service::start(lab.config()).await;
    let issued = service
        .http
        .json(
            "POST",
            "/v1/auth/login",
            None,
            Some(&json!({"login":LOGIN,"password":PASSWORD,"device_label":"S102 controller"})),
        )
        .await
        .unwrap()
        .1;
    let token = issued["access_token"].as_str().unwrap();
    let vault = register_vault(&service.http, token, &lab.work())
        .await
        .unwrap();
    let seeds = [
        ("clean.txt", "S102 clean old baseline\n"),
        ("dirty.txt", "S102 dirty old baseline\n"),
        ("pending-delete.txt", "S102 delete must not replay\n"),
        ("event.txt", "S102 event baseline\n"),
    ];
    let mut seeded = Vec::new();
    for (path, content) in seeds {
        std::fs::write(lab.work().join(path), content).unwrap();
        seeded.push(json!({"path":path,"hash":hex(&vaultsyncd_platform_linux_server::files::hash::digest(content.as_bytes())),"size":content.len().to_string()}));
    }
    wait_for("seed adoption", || async {
        let rows = common::snapshot(&service.http, token, &vault).await;
        Ok(seeded.iter().all(|seed| {
            rows.iter()
                .any(|r| r["name"] == seed["path"] && r["hash"] == seed["hash"])
        }))
    })
    .await
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = json!({"url":format!("http://{address}"),"vault":vault,"issued":issued,"login":LOGIN,"password":PASSWORD,"server_version":env!("CARGO_PKG_VERSION"),"seeded":seeded,"work_dir":lab.work()});
    let host = Arc::new(Mutex::new(Host {
        state: lab.state(),
        work: lab.work(),
        archive: lab.state().parent().unwrap().join("old.vsbk"),
        lab,
        service: Some(service),
        vault,
        restored: false,
    }));
    let task_host = host.clone();
    let task = tokio::task::spawn_local(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let host = task_host.clone();
            tokio::task::spawn_local(async move {
                if let Err(e) = connection(stream, host).await {
                    eprintln!("controller connection: {e}");
                }
            });
        }
    });
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(manifest)
        .unwrap();
    file.write_all(serde_json::to_string(&fixture).unwrap().as_bytes())
        .unwrap();
    file.sync_all().unwrap();
    println!("READY http://{address}");
    let deadline = std::time::Instant::now() + Duration::from_secs(2400);
    while !stop.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    task.abort();
    let mut host = host.lock().await;
    if let Some(service) = host.service.take() {
        service.stop().await;
    }
}
async fn connection(mut stream: TcpStream, host: Arc<Mutex<Host>>) -> std::io::Result<()> {
    let mut head = Vec::new();
    // Read only the HTTP head: forwarding must retain every byte of a possible request body.
    while !head.ends_with(b"\r\n\r\n") {
        head.push(stream.read_u8().await?);
        if head.len() > 65536 {
            return Err(std::io::Error::other("head too large"));
        }
    }
    let text = String::from_utf8_lossy(&head);
    let path = text.split_whitespace().nth(1).unwrap_or("");
    if let Some(action) = path.strip_prefix("/__s102/") {
        let value = host.lock().await.control(action).await;
        let body = serde_json::to_vec(&value).unwrap();
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await?;
        stream.write_all(&body).await?;
        stream.shutdown().await?;
    } else {
        let uri = host.lock().await.service.as_ref().unwrap().http.uri("");
        let mut upstream = TcpStream::connect(uri.strip_prefix("http://").unwrap()).await?;
        upstream.write_all(&head).await?;
        tokio::io::copy_bidirectional(&mut stream, &mut upstream).await?;
    }
    Ok(())
}
