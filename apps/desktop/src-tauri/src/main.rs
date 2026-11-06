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

fn main() {
    #[cfg(windows)]
    {
        if std::env::args().skip(1).eq(["--register-shortcut"]) {
            if let Err(error) = vaultsyncd_platform_windows::shell::toast::register_shortcut() {
                eprintln!("shortcut registration failed: {error}");
                std::process::exit(1);
            }
        } else {
            shell::run();
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("vaultsyncd desktop runs on Windows 11 x64");
        std::process::exit(2);
    }
}
