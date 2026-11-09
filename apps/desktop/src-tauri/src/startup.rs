//! The GUI ensures a healthy independent agent before opening its WebView.
use rand_core::{OsRng, RngCore};
use std::{
    io::{self, Read},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use vaultsyncd_local_ipc::generated::{Command as IpcCommand, CommandName, Request, ResponseKind};
use vaultsyncd_platform_windows::ipc::Endpoint;

fn missing_pipe(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(2) | Some(3))
}
fn ready(agent: &Path) -> io::Result<bool> {
    tauri::async_runtime::block_on(async {
        let endpoint = Endpoint::current("vaultsyncd-agent")?;
        let mut identity = [0_u8; 16];
        OsRng
            .try_fill_bytes(&mut identity)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let request = Request {
            request_id: identity.iter().map(|byte| format!("{byte:02x}")).collect(),
            ipc_version: vaultsyncd_local_ipc::version::supported(),
            command: IpcCommand {
                name: CommandName::GetStatus,
                parameters: None,
            },
        };
        match endpoint.request_verified(agent, &request).await {
            Ok((response, process)) => {
                if response.kind != ResponseKind::Ok || response.result.is_none() {
                    return Err(io::Error::other(
                        "agent IPC 拒绝状态检查，请检查现有进程与版本。",
                    ));
                }
                if process.wait(0)? {
                    return Err(io::Error::other(
                        "agent 在状态检查期间退出，请检查 agent.log。",
                    ));
                }
                Ok(true)
            }
            Err(error) if missing_pipe(&error) => Ok(false),
            Err(error) => Err(io::Error::other(format!(
                "现有 agent IPC 无法核验，保留现有进程：{error}"
            ))),
        }
    })
}
fn launch(agent: &Path) -> io::Result<()> {
    let mut child = Command::new(agent)
        .arg("--ensure-user-session")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .spawn()
        .map_err(|error| {
            io::Error::other(format!("无法执行同目录 vaultsyncd-agent.exe：{error}"))
        })?;
    let deadline = Instant::now() + Duration::from_secs(70);
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            let mut detail = Vec::new();
            if let Some(stderr) = child.stderr.take() {
                stderr.take(2048).read_to_end(&mut detail)?;
            }
            let detail = String::from_utf8_lossy(&detail);
            return Err(io::Error::other(format!(
                "agent 启动失败（{status}）。{} 请检查本用户 vaultsyncd/agent/agent.log。",
                detail.trim()
            )));
        }
        if Instant::now() >= deadline {
            // Only stop the bounded launcher. A background agent that has already started is
            // independent and must never be killed by the GUI, including on startup failure.
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "等待 agent 就绪超时，请检查本用户 vaultsyncd/agent/agent.log 后重新启动。",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
/// Reuse a verified same-user/session peer first; never start over a busy or untrusted endpoint.
pub(super) fn ensure_agent() -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let agent: PathBuf = executable
        .parent()
        .ok_or_else(|| io::Error::other("无法定位客户端目录。"))?
        .join("vaultsyncd-agent.exe");
    if ready(&agent)? {
        return Ok(());
    }
    launch(&agent)?;
    if !ready(&agent)? {
        return Err(io::Error::other(
            "agent 启动后 IPC 尚未就绪，请检查 agent.log。",
        ));
    }
    Ok(())
}
