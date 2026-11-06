//! S075-c：真实 SIGKILL 后重启必须自动回收遗留的管理 socket，并对新 socket 正常响应。
//!
//! 崩溃残留的判定依据是启动顺序：`bootstrap::prepare` 先独占 `<state>/server.lock`，之后才绑定
//! `<state>/admin/socket`。持有锁时看到的既有 socket 只能是上次进程留下的 inode，因此可以回收；
//! 活着的第二个服务端在抢锁阶段就被拒绝，根本到不了删除路径。非 socket 占位、符号链接或他人
//! 属主则明确失败，不删除、不覆盖。

mod common;

use std::io::{BufRead, BufReader};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{Lab, Running};
use serde_json::Value;
use vaultsyncd_platform_linux_server::ipc;
use vaultsyncd_server::bootstrap::{self, StartupError};

/// AF_UNIX `sun_path` 含结尾 NUL 共 108 字节；这里给测试自己一个与检出深度无关的短基目录。
///
/// `/tmp/vs075c-<pid>-<label>` 远短于上限，且不受 `TMPDIR`/`CARGO_TARGET_TMPDIR`/工作树路径影响，
/// 避免父集成树因 worktree 更深而把测试变成路径长度探测；`Drop` 删除本测试的短基目录。
struct ShortBase {
    path: PathBuf,
}

impl ShortBase {
    fn new(label: &str) -> Self {
        let path = PathBuf::from("/tmp").join(format!("vs075c-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建短测试基目录");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ShortBase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 真实服务端子进程：只在 SIGKILL 场景里用，确保 socket 的 Drop 永远不会执行。
struct ServerChild {
    child: Child,
}

impl ServerChild {
    async fn spawn(lab: &Lab) -> Self {
        let _ = lab.config();
        let mut child = Command::new(env!("CARGO_BIN_EXE_server-child"))
            .arg(lab.config_path())
            .stdout(Stdio::piped())
            .spawn()
            .expect("启动服务端子进程");
        let stdout = child.stdout.take().expect("stdout");
        let line = tokio::task::spawn_blocking(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).map(|_| line)
        })
        .await
        .expect("读取任务")
        .expect("读取监听地址");
        assert!(
            line.trim().starts_with("LISTEN "),
            "子进程没有给出监听地址：{line:?}"
        );
        Self { child }
    }

    fn kill(mut self) {
        self.child.kill().expect("SIGKILL");
        let status = self.child.wait().expect("回收");
        assert!(!status.success(), "子进程必须是被强杀的：{status:?}");
    }
}

impl Drop for ServerChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn admin_status(state_dir: &Path) -> Value {
    let path = state_dir.join("admin/socket");
    let mut stream = ipc::connect(&path).await.expect("连接重启后的管理 socket");
    ipc::write_frame(&mut stream, br#"["v1","status"]"#)
        .await
        .expect("发送 status");
    let bytes = ipc::read_frame(&mut stream).await.expect("读取 status");
    serde_json::from_slice(&bytes).expect("管理响应是 JSON")
}

#[tokio::test]
async fn killed_server_restart_reclaims_stale_admin_socket_and_answers() {
    let base = ShortBase::new("kill");
    let lab = Lab::new("s075c-kill-admin", Some(base.path()), Some(base.path()));
    lab.init_owner().await;
    let socket = lab.state().join("admin/socket");
    assert!(
        socket.as_os_str().len() <= 107,
        "测试 socket 路径超过 AF_UNIX 上限：{}",
        socket.display()
    );

    let child = ServerChild::spawn(&lab).await;
    let before = std::fs::symlink_metadata(&socket).expect("子进程已创建 socket");
    assert!(before.file_type().is_socket());
    child.kill();

    // SIGKILL 不跑 Drop：socket 必须仍在，否则这个用例没有复现崩溃现场。
    let leftover = std::fs::symlink_metadata(&socket).expect("崩溃遗留 socket");
    assert!(leftover.file_type().is_socket());
    assert_eq!(leftover.ino(), before.ino(), "残留必须是原 socket inode");

    // 重启成功即证明锁后回收生效；随后新 socket 必须提供正常响应。
    let running = Running::start(&lab).await.expect("重启回收遗留 socket");
    let after = std::fs::symlink_metadata(&socket).expect("重启后 socket");
    assert!(after.file_type().is_socket());
    assert_ne!(after.ino(), before.ino(), "必须是新绑定的 inode");
    let status = admin_status(&lab.state()).await;
    assert_eq!(status["status"], 200, "{status}");
    assert_eq!(status["result"]["phase"], "ready", "{status}");
    assert_eq!(status["result"]["root_count"], "0", "{status}");
    running.stop().await;

    // 优雅停止仍只清理自己；再次启动并响应，确认不是一次性的偶然成功。
    let running = Running::start(&lab).await.expect("第二次启动");
    let status = admin_status(&lab.state()).await;
    assert_eq!(status["status"], 200, "{status}");
    running.stop().await;
}

#[tokio::test]
async fn non_socket_placeholder_is_refused_without_deleting_evidence() {
    let base = ShortBase::new("placeholder");
    let lab = Lab::new(
        "s075c-admin-placeholder",
        Some(base.path()),
        Some(base.path()),
    );
    lab.init_owner().await;
    let admin = lab.state().join("admin");
    std::fs::create_dir(&admin).expect("建 admin 目录");
    std::fs::set_permissions(&admin, std::fs::Permissions::from_mode(0o700)).expect("0700");
    std::fs::write(admin.join("socket"), b"recovery evidence").expect("写入占位");

    match bootstrap::prepare(lab.config()).await {
        Err(StartupError::Admin(error)) => {
            assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
        }
        Err(other) => panic!("必须是管理 socket 绑定失败，实际：{other}"),
        Ok(_) => panic!("非 socket 占位必须拒绝启动"),
    }
    assert_eq!(
        std::fs::read(admin.join("socket")).expect("占位仍在"),
        b"recovery evidence"
    );
}

#[tokio::test]
async fn live_service_keeps_its_socket_because_the_state_lock_is_held() {
    let base = ShortBase::new("live");
    let lab = Lab::new("s075c-admin-live", Some(base.path()), Some(base.path()));
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动");
    let socket = lab.state().join("admin/socket");
    let live = std::fs::symlink_metadata(&socket).expect("live socket");

    // 第二个服务端先抢状态锁就失败，不会走到“回收 socket”；现场必须原样保留。
    match bootstrap::prepare(lab.config()).await {
        Err(StartupError::StateLock(_)) => (),
        Err(other) => panic!("必须先被状态锁拒绝，实际：{other}"),
        Ok(_) => panic!("同一状态目录不能启动第二个服务端"),
    }
    let kept = std::fs::symlink_metadata(&socket).expect("live socket 仍在");
    assert_eq!(kept.ino(), live.ino(), "不得删除或替换活着的 socket");
    let status = admin_status(&lab.state()).await;
    assert_eq!(status["status"], 200, "{status}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    running.stop().await;
}
