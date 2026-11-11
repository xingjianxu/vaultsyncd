//! Bound the stop helper's lifetime independently of agent IPC and pipe inheritance.
use std::{
    fs::File,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub(crate) fn run(command: &mut Command, log: &Path, timeout: Duration) -> Result<(), String> {
    let output = File::create(log).map_err(|e| format!("无法创建停止日志：{e}"))?;
    let stderr = output
        .try_clone()
        .map_err(|e| format!("无法复制停止日志句柄：{e}"))?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(output)
        .stderr(stderr)
        .spawn()
        .map_err(|e| format!("无法执行同目录 agent 停止入口：{e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child
            .try_wait()
            .map_err(|e| format!("无法等待停止入口：{e}"))?
        {
            Some(status) if status.success() => return Ok(()),
            Some(status) => {
                let mut detail = String::new();
                File::open(log)
                    .and_then(|file| file.take(64 * 1024).read_to_string(&mut detail))
                    .map_err(|e| format!("停止入口返回 {status}，无法读取日志：{e}"))?;
                return Err(format!("agent 停止入口返回 {status}。\n{detail}"));
            }
            None if Instant::now() >= deadline => {
                // This handle belongs only to the short-lived RPC helper we just spawned.
                // Never terminate the authenticated agent; its drain may still be running.
                let cleanup = child.kill().and_then(|()| child.wait().map(|_| ()));
                return Err(format!(
                    "等待停止 agent 超时，尚未确认 agent 已退出；后台可能仍在排空同步。请检查 agent.log 后重试，或关闭自动停止设置后退出界面。停止入口回收：{cleanup:?}"
                ));
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn reports_helper_failure_with_its_diagnostic() {
        let log = std::env::temp_dir().join(format!("s203-output-{}.log", std::process::id()));
        let mut command = Command::new("sh");
        command.args(["-c", "echo stop-diagnostic >&2; exit 7"]);
        let error = run(&mut command, &log, Duration::from_secs(2)).unwrap_err();
        assert!(error.contains("stop-diagnostic") && error.contains('7'));
        std::fs::remove_file(log).unwrap();
    }

    #[test]
    fn a_stalled_helper_is_bounded_and_reaped() {
        let log = std::env::temp_dir().join(format!("s203-timeout-{}.log", std::process::id()));
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let started = Instant::now();
        let error = run(&mut command, &log, Duration::from_millis(100)).unwrap_err();
        assert!(error.contains("超时") && error.contains("Ok(())"));
        assert!(started.elapsed() < Duration::from_secs(2));
        std::fs::remove_file(log).unwrap();
    }
}
