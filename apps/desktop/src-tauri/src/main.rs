//! Windows GUI shell. Synchronization continues in the independent agent process.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod commands;
#[cfg(windows)]
mod desktop_lifecycle;
#[cfg(any(windows, test))]
mod desktop_settings;
#[cfg(windows)]
mod file_commands;
#[cfg(windows)]
mod history_commands;
#[cfg(windows)]
mod notifications;
#[cfg(windows)]
mod shell;
#[cfg(windows)]
mod shell_actions;
#[cfg(any(windows, test))]
mod shell_request;
#[cfg(windows)]
mod startup;
#[cfg(any(windows, test))]
mod stop_process;
#[cfg(any(windows, test))]
mod style_csp;
#[cfg(any(windows, test))]
mod update_archive;
#[cfg(windows)]
mod updates;

#[cfg(windows)]
const DESKTOP_SCOPE: &str = "vaultsyncd-desktop";

fn main() {
    #[cfg(windows)]
    {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.first().is_some_and(|arg| arg == "--shell-action") {
            let request = match shell_request::parse(&args) {
                Ok(request) => request,
                Err(error) => {
                    vaultsyncd_platform_windows::shell::dialog::startup_error(error);
                    std::process::exit(2);
                }
            };
            if let Err(error) = startup::ensure_agent() {
                vaultsyncd_platform_windows::shell::dialog::startup_error(&format!(
                    "无法启动本机 agent：{error}"
                ));
                std::process::exit(1);
            }
            // File windows are independent of the one main window and never stop the agent.
            shell::run(Some(request));
        } else if args
            .iter()
            .map(|arg| arg.as_os_str())
            .eq([std::ffi::OsStr::new("--unregister-shell-menu")])
        {
            if let Err(error) = vaultsyncd_platform_windows::shell::context_menu::unregister() {
                eprintln!("右键菜单清理失败：{error}");
                std::process::exit(1);
            }
        } else if std::env::args().skip(1).eq(["--register-shortcut"]) {
            if let Err(error) = vaultsyncd_platform_windows::shell::toast::register_shortcut() {
                eprintln!("shortcut registration failed: {error}");
                std::process::exit(1);
            }
        } else {
            // One window per user session: a second Start/double-click must not open a
            // duplicate window on the same agent. The mutex lives for this process lifetime.
            match vaultsyncd_platform_windows::single::claim(DESKTOP_SCOPE) {
                Ok(Some(_held)) => {
                    if let Err(error) = startup::ensure_agent() {
                        let detail = format!("无法启动本机 agent。{error}");
                        eprintln!("{detail}");
                        vaultsyncd_platform_windows::shell::dialog::startup_error(&detail);
                        std::process::exit(1);
                    }
                    shell::run(None);
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("desktop single-instance check failed: {error}");
                    std::process::exit(1);
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("vaultsyncd desktop runs on Windows 11 x64");
        std::process::exit(2);
    }
}
