//! 进程级验收：真实启动、健康探测、优雅停止与重启样本（实施手册 S041「本步验收」）。
//!
//! 这些用例运行真正的 `vaultsyncd-server` 二进制（`CARGO_BIN_EXE_*`），因此覆盖的是进程入口、
//! 配置加载、数据库打开、启动恢复门、监听绑定、SIGTERM/SIGINT 停止顺序与退出码，而不是单个函数。
//!
//! 覆盖的验收项：
//!
//! 1. 启动 → `GET /v1/health` 报 `ok` → SIGTERM → 退出码 0 → 用同一状态目录重启仍 `ok`；
//! 2. 损坏配置（未知键、非法监听地址、缺失状态目录）一律退出码 2，且不会创建数据库；
//! 3. 有未完成意图时 readiness 为假（503 `NEEDS_RECOVERY`）；优雅停止后意图行仍在（证据保留），
//!    重启后仍然为假——绝不因为“重启过”就把未完成的恢复当成完成。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use sqlx::SqlitePool;
use vaultsyncd_domain::ids::{EntryId, ID_LEN, IntentId, UserId, VaultId};
use vaultsyncd_domain::version::{ServerEpoch, UtcTimestamp};
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::intents::{self, FsIntentPhase, NewFsIntent};
use vaultsyncd_server_store::vaults::{self, NewVault};

/// 每个用例独占的状态目录；`Drop` 时删除（测试只写临时目录，不碰真实实例）。
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("vaultsyncd-s041-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&path).expect("创建测试状态目录");
        TestDir { path }
    }

    fn config(&self, label: &str, listen: &str) -> PathBuf {
        let path = self.path.join(format!("{label}.toml"));
        let text = format!(
            "[server]\nlisten = \"{listen}\"\n\n[state]\ndir = \"{}\"\n\n[log]\nlevel = \"info\"\nformat = \"text\"\n",
            self.path.display()
        );
        std::fs::write(&path, text).expect("写配置文件");
        path
    }

    fn database(&self) -> PathBuf {
        self.path.join("server.sqlite")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_vaultsyncd-server")
}

fn spawn(config: &Path) -> Child {
    Command::new(binary())
        .arg("--config")
        .arg(config)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("启动 vaultsyncd-server")
}

fn read_stderr(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        stderr.read_to_string(&mut text).expect("读子进程 stderr");
    }
    text
}

fn wait_exit(child: &mut Child, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("查询子进程状态") {
            return status;
        }
        assert!(Instant::now() < deadline, "子进程在 {timeout:?} 内没有退出");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 用 SIGTERM 停止子进程，模拟 systemd 的 `ExecStop`（不 kill -9，走优雅停止路径）。
fn send_sigterm(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("调用 kill -TERM");
    assert!(status.success(), "kill -TERM 失败：{status}");
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("临时绑定端口");
    listener.local_addr().expect("取端口").port()
}

fn http_get(port: u16, path: &str) -> std::io::Result<(u16, String)> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("地址");
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP 响应含头体分隔");
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .expect("HTTP 状态码");
    Ok((status, body.to_owned()))
}

/// 轮询健康探测直到拿到响应（启动完成）或超时。
fn wait_health(port: u16, timeout: Duration) -> (u16, String) {
    let deadline = Instant::now() + timeout;
    loop {
        match http_get(port, "/v1/health") {
            Ok((status, body)) => return (status, body),
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "健康探测在 {timeout:?} 内没有成功：{error}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// 在状态目录里建库并写入种子：一个账号、一个 Vault（必需）、以及给定数量的 `prepared` 意图。
fn seed(state_dir: &Path, intents_to_write: &[(&str, &str)]) -> (UserId, VaultId) {
    let user = UserId::from_bytes([0x11; ID_LEN]);
    let vault = VaultId::from_bytes([0x22; ID_LEN]);
    let entry = EntryId::from_bytes([0x33; ID_LEN]);
    let database = state_dir.join("server.sqlite");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建测试运行时");
    runtime.block_on(async {
        let db = Database::open(&database).await.expect("打开测试库");
        let mut tx = db.begin_write().await.expect("写事务");
        // 账号表还没有 typed API（S042 提供），测试直接写那一张表，与 server-store 自己的
        // 测试装置做法一致；其余行都走公开接口。
        sqlx::query("INSERT INTO users (id, login, password_hash, status) VALUES (?1, ?2, ?3, ?4)")
            .bind(user.as_bytes().as_slice())
            .bind("owner")
            .bind("seed-not-a-real-hash")
            .bind("active")
            .execute(&mut *tx)
            .await
            .expect("写种子账号");
        vaults::insert_vault(
            &mut tx,
            &NewVault {
                id: vault,
                entry,
                owner: user,
                absolute_path: "/srv/vaultsyncd-s041-seed".to_owned(),
                volume_identity: vec![0x44; 16],
                vault_file_identity: vec![0x55; 16],
                epoch: ServerEpoch::new(1).expect("epoch"),
            },
        )
        .await
        .expect("写种子 Vault");
        let created_at = UtcTimestamp::from_unix_seconds(1_700_000_000).expect("时间");
        for (index, (target, desired)) in intents_to_write.iter().enumerate() {
            intents::begin_intent(
                &mut tx,
                user,
                &NewFsIntent {
                    intent_id: IntentId::from_bytes([0x66 + index as u8; ID_LEN]),
                    vault,
                    kind: intents::FsIntentKind::PutContent,
                    target_path: (*target).to_owned(),
                    staging_path: None,
                    expected_before: r#"{"kind":"absent"}"#.to_owned(),
                    desired_after: (*desired).to_owned(),
                },
                created_at,
            )
            .await
            .expect("写种子意图");
        }
        tx.commit().await.expect("提交种子事务");
        db.close().await;
    });

    (user, vault)
}

/// 直接读库核对证据：未完成意图的阶段与条数（进程已退出后调用的独立连接）。
fn unfinished_phases(state_dir: &Path, user: UserId, vault: VaultId) -> Vec<FsIntentPhase> {
    let database = state_dir.join("server.sqlite");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建测试运行时");
    runtime.block_on(async {
        let pool: SqlitePool = SqlitePool::connect(&format!("sqlite://{}", database.display()))
            .await
            .expect("连测试库");
        let mut conn = pool.acquire().await.expect("取连接");
        let rows = intents::unfinished_intents(&mut conn, user, vault)
            .await
            .expect("读未完成意图");
        drop(conn);
        pool.close().await;
        rows.into_iter().map(|intent| intent.phase).collect()
    })
}

#[test]
fn second_server_on_the_same_state_dir_is_refused_by_the_state_lock() {
    let dir = TestDir::new("state-lock");
    let port = free_port();
    let config = dir.config("server", &format!("127.0.0.1:{port}"));

    let mut child = spawn(&config);
    let (status, body) = wait_health(port, Duration::from_secs(20));
    assert_eq!(status, 200, "第一个服务端应正常就绪：{body}");

    // 第二个进程指向同一状态目录：必须在打开数据库之前被状态目录锁挡住（不是“先跑起来再报端口占用”）。
    let mut second = spawn(&config);
    let status = wait_exit(&mut second, Duration::from_secs(20));
    let stderr = read_stderr(&mut second);
    assert_ne!(status.code(), Some(0), "第二个服务端不得成功启动：{stderr}");
    assert!(
        stderr.contains("state_locked") || stderr.contains("已被另一个 vaultsyncd 进程占用"),
        "拒绝原因必须是状态目录锁：{stderr}"
    );
    // 第一个服务端不受影响。
    let (status, _) = wait_health(port, Duration::from_secs(5));
    assert_eq!(status, 200, "第二个进程被拒后第一个应继续服务");

    send_sigterm(&child);
    let status = wait_exit(&mut child, Duration::from_secs(20));
    assert_eq!(status.code(), Some(0), "第一个服务端应正常退出");

    // 停止后锁释放：同一状态目录可以再次启动。
    let mut restarted = spawn(&config);
    let (status, body) = wait_health(port, Duration::from_secs(20));
    assert_eq!(status, 200, "锁随进程退出释放：{body}");
    send_sigterm(&restarted);
    let status = wait_exit(&mut restarted, Duration::from_secs(20));
    assert_eq!(status.code(), Some(0));
}

#[test]
fn start_serves_health_then_sigterm_stops_and_restart_works() {
    let dir = TestDir::new("start-stop-restart");
    let port = free_port();
    let config = dir.config("server", &format!("127.0.0.1:{port}"));

    let mut child = spawn(&config);
    let (status, body) = wait_health(port, Duration::from_secs(20));
    assert_eq!(status, 200, "干净启动后健康探测应为 200：{body}");
    assert_eq!(body, r#"{"status":"ok"}"#);
    assert!(dir.database().exists(), "启动应创建服务端数据库");

    send_sigterm(&child);
    let status = wait_exit(&mut child, Duration::from_secs(20));
    let stderr = read_stderr(&mut child);
    assert_eq!(status.code(), Some(0), "优雅停止应退出 0：{stderr}");
    assert!(stderr.contains("服务端已停止"), "停止日志缺失：{stderr}");
    assert!(
        stderr.contains("draining"),
        "停止流程应先进入 draining：{stderr}"
    );

    // 重启样本：同一状态目录、同一端口，仍然可用（数据库可重开、监听可重绑）。
    let mut restarted = spawn(&config);
    let (status, body) = wait_health(port, Duration::from_secs(20));
    assert_eq!(status, 200, "重启后健康探测应为 200：{body}");
    assert_eq!(body, r#"{"status":"ok"}"#);
    send_sigterm(&restarted);
    let status = wait_exit(&mut restarted, Duration::from_secs(20));
    let stderr = read_stderr(&mut restarted);
    assert_eq!(status.code(), Some(0), "重启后优雅停止应退出 0：{stderr}");
}

#[test]
fn corrupt_config_exits_2_and_never_opens_state() {
    let dir = TestDir::new("corrupt-config");

    let unknown_key = dir.path.join("unknown-key.toml");
    std::fs::write(
        &unknown_key,
        format!(
            "[server]\nlistne = \"127.0.0.1:8721\"\n\n[state]\ndir = \"{}\"\n",
            dir.path.display()
        ),
    )
    .expect("写配置");
    let mut child = spawn(&unknown_key);
    let status = wait_exit(&mut child, Duration::from_secs(20));
    let stderr = read_stderr(&mut child);
    assert_eq!(status.code(), Some(2), "配置错误必须退出 2：{stderr}");
    assert!(
        stderr.contains("config_invalid"),
        "缺少稳定错误码：{stderr}"
    );
    assert!(stderr.contains("listne"), "错误信息应指出未知键：{stderr}");

    let bad_listen = dir.path.join("bad-listen.toml");
    std::fs::write(
        &bad_listen,
        format!(
            "[server]\nlisten = \"not-an-address\"\n\n[state]\ndir = \"{}\"\n",
            dir.path.display()
        ),
    )
    .expect("写配置");
    let mut child = spawn(&bad_listen);
    let status = wait_exit(&mut child, Duration::from_secs(20));
    let stderr = read_stderr(&mut child);
    assert_eq!(status.code(), Some(2), "非法监听地址必须退出 2：{stderr}");

    let missing_state = dir.path.join("missing-state.toml");
    std::fs::write(
        &missing_state,
        "[server]\nlisten = \"127.0.0.1:8721\"\n\n[state]\ndir = \"/nonexistent/s041-state\"\n",
    )
    .expect("写配置");
    let mut child = spawn(&missing_state);
    let status = wait_exit(&mut child, Duration::from_secs(20));
    let stderr = read_stderr(&mut child);
    assert_eq!(status.code(), Some(2), "状态目录不可用必须退出 2：{stderr}");

    assert!(
        !dir.database().exists(),
        "配置不合法时不得创建或打开服务端数据库"
    );
}

#[test]
fn unfinished_intent_keeps_readiness_false_and_survives_graceful_stop() {
    let dir = TestDir::new("unfinished-intent");
    let (user, vault) = seed(
        &dir.path,
        &[("report.txt", r#"{"kind":"file","length":12}"#)],
    );
    assert_eq!(
        unfinished_phases(&dir.path, user, vault),
        vec![FsIntentPhase::Prepared],
        "种子应写入一条 prepared 意图"
    );

    let port = free_port();
    let config = dir.config("server", &format!("127.0.0.1:{port}"));

    let mut child = spawn(&config);
    let (status, body) = wait_health(port, Duration::from_secs(20));
    assert_eq!(
        status, 503,
        "有未完成意图时不得报告可用（恢复没完成就不能开放写入）：{body}"
    );
    assert!(body.contains("NEEDS_RECOVERY"), "{body}");
    assert!(body.contains("\"retryable\":false"), "{body}");

    send_sigterm(&child);
    let status = wait_exit(&mut child, Duration::from_secs(20));
    let stderr = read_stderr(&mut child);
    assert_eq!(status.code(), Some(0), "只读状态也必须能优雅停止：{stderr}");
    assert!(
        stderr.contains("unfinished_intents=1"),
        "停止时未统计并保留未完成意图：{stderr}"
    );

    // 证据保留：进程退出后意图行仍是 prepared，没有被“停止/重启”清掉或推进。
    assert_eq!(
        unfinished_phases(&dir.path, user, vault),
        vec![FsIntentPhase::Prepared],
        "优雅停止不得清理或改写任务证据"
    );

    // 重启后仍然拒绝报告可用：重启本身不是恢复完成的证据。
    let mut restarted = spawn(&config);
    let (status, body) = wait_health(port, Duration::from_secs(20));
    assert_eq!(status, 503, "重启后仍不得报告可用：{body}");
    assert!(body.contains("NEEDS_RECOVERY"), "{body}");
    send_sigterm(&restarted);
    let status = wait_exit(&mut restarted, Duration::from_secs(20));
    assert_eq!(status.code(), Some(0), "重启后优雅停止应退出 0");
}
