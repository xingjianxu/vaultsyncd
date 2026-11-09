//! Windows GUI shell. Synchronization continues in the independent agent process.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod commands;
#[cfg(windows)]
mod file_commands;
#[cfg(windows)]
mod history_commands;
#[cfg(windows)]
mod notifications;
#[cfg(windows)]
mod shell;
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
        if std::env::args().skip(1).eq(["--register-shortcut"]) {
            if let Err(error) = vaultsyncd_platform_windows::shell::toast::register_shortcut() {
                eprintln!("shortcut registration failed: {error}");
                std::process::exit(1);
            }
        } else {
            // One window per user session: a second Start/double-click must not open a
            // duplicate window on the same agent. The mutex lives for this process lifetime.
            match vaultsyncd_platform_windows::single::claim(DESKTOP_SCOPE) {
                Ok(Some(_held)) => shell::run(),
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
