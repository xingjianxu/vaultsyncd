//! 离线 `init` 的进程级验收（design §17.1、C27）：真实二进制 + 真实 SQLite + 真实文件锁。
//!
//! 每条用例一个独立临时目录，`Drop` 时删除；不写用户主目录、不用固定路径、不读环境里的密码。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::state_lock::StateLock;
use vaultsyncd_server_store::users;

const PASSWORD: &str = "correct horse battery staple";

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vaultsyncd-s042-{}-{label}-{index}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("创建临时状态目录");
        Self { path }
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn admin_init(state_dir: &Path, stdin: &str, extra: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vaultsyncd-admin"))
        .arg("init")
        .arg("--state-dir")
        .arg(state_dir)
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("启动 vaultsyncd-admin");
    {
        let mut pipe = child.stdin.take().expect("管道 stdin");
        pipe.write_all(stdin.as_bytes()).expect("写 stdin");
        // 必须显式关闭管道写端：子进程读到 EOF 才会继续，否则这里会一直等。
    }
    child.wait_with_output().expect("等待进程结束")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// 读出唯一账号的密码散列；没有账号时返回 `None`。
fn stored_hash(state_dir: &Path) -> Option<String> {
    let database_path = state_dir.join("server.sqlite");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建测试运行时");
    runtime.block_on(async {
        let database = Database::open(&database_path).await.expect("打开测试库");
        let mut conn = database.pool().acquire().await.expect("取连接");
        let record = users::find_by_login(&mut conn, "owner")
            .await
            .expect("按登录名读账号");
        // 连接必须先还回连接池，否则 `close` 会等它（这正是本卡调试时踩到的坑）。
        drop(conn);
        database.close().await;
        record.map(|record| record.password_hash)
    })
}

#[test]
fn init_from_piped_stdin_writes_an_argon2id_account_without_echoing_secrets() {
    let dir = TestDir::new("init-ok");
    let output = admin_init(&dir.path, &format!("{PASSWORD}\n"), &[]);
    assert!(
        output.status.success(),
        "stdout/stderr: {}",
        text(&output.stderr)
    );

    let stdout = text(&output.stdout);
    assert!(stdout.contains("login=owner"), "{stdout}");
    assert!(stdout.contains("Argon2id"), "{stdout}");
    assert!(
        !stdout.contains(PASSWORD) && !text(&output.stderr).contains(PASSWORD),
        "输出里不得出现密码：{stdout}"
    );
    assert!(
        !stdout.contains("$argon2id$"),
        "输出里不得出现密码散列：{stdout}"
    );

    let hash = stored_hash(&dir.path).expect("账号必须写入");
    assert!(hash.starts_with("$argon2id$v=19$"), "{hash}");
    assert_ne!(hash, PASSWORD);
    assert!(hash.contains("m=19456,t=2,p=1"), "生产参数：{hash}");
}

#[test]
fn a_second_init_is_refused_and_keeps_the_existing_hash() {
    let dir = TestDir::new("init-repeat");
    let first = admin_init(&dir.path, &format!("{PASSWORD}\n"), &[]);
    assert!(first.status.success(), "{}", text(&first.stderr));
    let original = stored_hash(&dir.path).expect("首次初始化写入账号");

    let second = admin_init(&dir.path, "another long enough password\n", &[]);
    assert_eq!(second.status.code(), Some(4), "{}", text(&second.stderr));
    assert!(
        text(&second.stderr).contains("拒绝重复初始化"),
        "{}",
        text(&second.stderr)
    );
    assert_eq!(
        stored_hash(&dir.path).as_deref(),
        Some(original.as_str()),
        "拒绝覆盖意味着旧散列原样保留"
    );
}

#[test]
fn a_short_password_is_refused_and_creates_no_account() {
    let dir = TestDir::new("init-weak");
    let output = admin_init(&dir.path, "too-short\n", &[]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    assert!(
        text(&output.stderr).contains("密码不合规"),
        "{}",
        text(&output.stderr)
    );
    assert!(
        stored_hash(&dir.path).is_none(),
        "输入不合规时不得写入任何账号"
    );
}

#[test]
fn a_password_on_the_command_line_is_a_usage_error() {
    let dir = TestDir::new("init-argv");
    let output = admin_init(&dir.path, "", &["--password", PASSWORD]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    assert!(
        text(&output.stderr).contains("未知参数：--password"),
        "{}",
        text(&output.stderr)
    );
    assert!(
        !dir.path.join("server.sqlite").exists(),
        "参数错误时不得创建数据库"
    );
}

#[test]
fn a_running_server_holds_the_state_lock_so_init_is_refused() {
    let dir = TestDir::new("init-locked");
    let held = StateLock::try_acquire_exclusive(&dir.path).expect("模拟服务端持有锁");
    let output = admin_init(&dir.path, &format!("{PASSWORD}\n"), &[]);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    assert!(
        text(&output.stderr).contains("已被占用"),
        "{}",
        text(&output.stderr)
    );
    assert!(
        stored_hash(&dir.path).is_none(),
        "服务端在运行时 init 不得写账号"
    );
    drop(held);

    // 服务端停止（锁释放）后同一状态目录可以正常初始化。
    let after = admin_init(&dir.path, &format!("{PASSWORD}\n"), &[]);
    assert!(after.status.success(), "{}", text(&after.stderr));
}
