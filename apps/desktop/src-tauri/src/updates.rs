//! Tray-triggered download. The candidate agent owns all offline maintenance and restart.
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};
use vaultsyncd_platform_windows::files::VaultHandle;
use vaultsyncd_platform_windows::shell::progress::UpdateProgress;
use vaultsyncd_release_client::GithubUpdates;
static RUNNING: AtomicBool = AtomicBool::new(false);
// Keep native menu resources outside the tray event callback: Tauri drops tray
// callbacks while clearing its icon registry during exit.
pub(crate) struct UpdateMenu(pub(crate) tauri::menu::MenuItem<tauri::Wry>);

fn message(text: &str) {
    vaultsyncd_platform_windows::shell::dialog::update_result(text);
}
fn staging() -> Result<PathBuf, String> {
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("无法定位用户更新目录")?);
    let mut parent = VaultHandle::open(&local).map_err(|e| e.to_string())?;
    let mut path = local;
    for name in ["vaultsyncd", "updates"] {
        path.push(name);
        parent = if path.try_exists().map_err(|e| e.to_string())? {
            VaultHandle::open(&path)
        } else {
            parent.create_new_directory(name)
        }
        .map_err(|e| e.to_string())?;
    }
    use rand_core::RngCore;
    let mut nonce = [0; 16];
    rand_core::OsRng
        .try_fill_bytes(&mut nonce)
        .map_err(|_| "无法建立独立更新目录")?;
    let name: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    parent
        .create_new_directory(&name)
        .map_err(|e| e.to_string())?;
    Ok(path.join(name))
}

pub(crate) fn start(app: &tauri::AppHandle, item: tauri::menu::MenuItem<tauri::Wry>) {
    if RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let app = app.clone();
    let _ = item.set_enabled(false);
    let _ = item.set_text("正在检查更新…");
    tauri::async_runtime::spawn(async move {
        let progress = UpdateProgress::new(
            "正在检查更新…",
            &format!("当前版本 {}", env!("CARGO_PKG_VERSION")),
        );
        let result = match &progress {
            Ok(progress) => download(&item, progress).await,
            Err(error) => Err(format!("无法显示更新进度：{error}")),
        };
        match result {
            Ok(Some(candidate)) => {
                if let Ok(progress) = &progress {
                    if let Err(error) = progress.stage(
                        "正在启动安装程序…",
                        "客户端即将关闭，安装进度将在独立窗口继续显示。",
                    ) {
                        eprintln!("update progress: {error}");
                    }
                }
                let program = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(PathBuf::from));
                let launched = program
                    .ok_or_else(|| "无法定位当前程序目录".to_string())
                    .and_then(|program| {
                        let log = fs::OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .open(candidate.parent().unwrap().join("apply.log"))
                            .map_err(|e| e.to_string())?;
                        use std::os::windows::process::CommandExt;
                        Command::new(candidate.join("vaultsyncd-agent.exe"))
                            .arg("--auto-update")
                            .arg("--program-dir")
                            .arg(program)
                            .stdin(Stdio::null())
                            .stdout(log.try_clone().map_err(|e| e.to_string())?)
                            .stderr(log)
                            .creation_flags(0x0800_0000)
                            .spawn()
                            .map_err(|e| format!("无法启动更新：{e}"))?;
                        Ok(())
                    });
                if let Err(error) = launched {
                    drop(progress);
                    message(&error);
                } else {
                    drop(progress);
                    app.exit(0);
                    return;
                }
            }
            Ok(None) => {
                drop(progress);
                message(&format!(
                    "当前已是最新正式版本（{}）",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            Err(error) => {
                drop(progress);
                message(&error);
            }
        }
        let _ = item.set_text("自动更新");
        let _ = item.set_enabled(true);
        RUNNING.store(false, Ordering::Release);
    });
}
async fn download(
    item: &tauri::menu::MenuItem<tauri::Wry>,
    progress: &UpdateProgress,
) -> Result<Option<PathBuf>, String> {
    let client = GithubUpdates::new()?;
    let Some(update) = client.check(env!("CARGO_PKG_VERSION")).await? else {
        return Ok(None);
    };
    progress
        .stage("正在连接下载…", &format!("版本 {}", update.version()))
        .map_err(|e| e.to_string())?;
    let directory = staging()?;
    let partial = directory.join("download.part");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .map_err(|e| e.to_string())?;
    let mut percent = u64::MAX;
    let result = client
        .download(&update, |chunk, received, total| {
            file.write_all(chunk)
                .map_err(|_| "更新包写入失败，请检查磁盘空间")?;
            let next = received * 100 / total;
            if next != percent {
                percent = next;
                let _ = item.set_text(format!("正在下载更新… {percent}%"));
                progress
                    .download(update.version(), received, total)
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        })
        .await;
    if let Err(error) = result {
        drop(file);
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    file.sync_all().map_err(|_| "更新包写入失败")?;
    drop(file);
    let archive = directory.join(update.filename());
    fs::rename(partial, &archive).map_err(|e| e.to_string())?;
    let candidate = directory.join("candidate");
    let version = update.version().to_owned();
    let output = candidate.clone();
    let _ = item.set_text("正在校验更新…");
    progress
        .stage("正在校验并解压更新…", &format!("版本 {}", update.version()))
        .map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn_blocking(move || {
        crate::update_archive::extract(&archive, &output, &version)
    })
    .await
    .map_err(|_| "更新解压任务失败")?
    .map_err(|e| e.to_string())?;
    Ok(Some(candidate))
}
