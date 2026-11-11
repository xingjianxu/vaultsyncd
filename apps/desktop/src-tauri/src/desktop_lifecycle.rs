//! Main-window close and explicit tray exit share one orderly-stop coordinator.
use crate::desktop_settings::{CloseAction, DesktopSettings, Settings};
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};
use tauri::Manager;

#[derive(Default)]
pub(crate) struct ExitState(AtomicBool);
pub(crate) fn is_exiting(app: &tauri::AppHandle) -> bool {
    app.state::<ExitState>().0.load(Ordering::Acquire)
}
#[tauri::command]
pub(crate) fn get_desktop_settings(
    state: tauri::State<'_, DesktopSettings>,
) -> Result<Settings, String> {
    state.reload()
}
#[tauri::command]
pub(crate) fn set_desktop_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopSettings>,
    settings: Settings,
) -> Result<Settings, String> {
    if is_exiting(&app) {
        return Err("正在有序退出，请等待完成后再修改配置。".into());
    }
    state.set(settings)
}
pub(crate) fn close_main(window: &tauri::Window, event: &tauri::WindowEvent) {
    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        api.prevent_close();
        let app = window.app_handle();
        if is_exiting(app) {
            return;
        }
        let settings = app.state::<DesktopSettings>().get().unwrap_or_default();
        match settings.close_action {
            CloseAction::Tray => {
                if let Err(error) = window.hide() {
                    report(app, format!("无法最小化到托盘：{error}"));
                }
            }
            CloseAction::Exit => exit(app),
        }
    }
}
fn report(app: &tauri::AppHandle, error: String) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_title("vaultsyncd");
        let _ = window.show();
        let _ = window.set_focus();
    }
    tauri::async_runtime::spawn_blocking(move || {
        vaultsyncd_platform_windows::shell::dialog::desktop_error(&error);
    });
}
pub(crate) fn exit(app: &tauri::AppHandle) {
    if crate::updates::is_running() {
        report(app, "正在自动更新，请等待更新结束后再退出。".into());
        return;
    }
    let settings = match app.state::<DesktopSettings>().get() {
        Ok(settings) => settings,
        Err(error) => {
            report(
                app,
                format!("请先在程序配置页面修复配置，再退出程序。{error}"),
            );
            return;
        }
    };
    if app
        .state::<ExitState>()
        .0
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    if !settings.stop_agent_on_exit {
        app.exit(0);
        return;
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_title("vaultsyncd — 正在停止 agent，请稍候");
        let _ = window.show();
        let _ = window.set_focus();
    }
    // Keep the WebView and tray alive during maintenance. No OS kill or fallback exit is used.
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = tauri::async_runtime::spawn_blocking(stop_agent).await;
        let result = match result {
            Ok(result) => result,
            Err(error) => Err(format!("停止任务失败：{error}")),
        };
        match result {
            Ok(()) => app.exit(0),
            Err(error) => {
                app.state::<ExitState>().0.store(false, Ordering::Release);
                report(
                    &app,
                    format!(
                        "无法有序停止 agent，程序已保留。部分同步可能已暂停，请检查后手动恢复。\n{error}"
                    ),
                );
            }
        }
    });
}
fn stop_agent() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let executable = std::env::current_exe().map_err(|e| format!("无法定位程序目录：{e}"))?;
    let agent = executable
        .parent()
        .ok_or("无法定位程序目录")?
        .join("vaultsyncd-agent.exe");
    let output = Command::new(agent)
        .args(["--user-session", "stop"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|e| format!("无法执行同目录 agent 停止入口：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "agent 停止入口返回 {}。\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        ));
    }
    Ok(())
}
pub(crate) fn load() -> DesktopSettings {
    let path = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("vaultsyncd/desktop-settings.json");
    DesktopSettings::load(path)
}
