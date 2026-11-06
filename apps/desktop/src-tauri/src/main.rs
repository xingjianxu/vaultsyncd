//! Windows GUI shell. Synchronization continues in the independent agent process.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod shell;

fn main() {
    #[cfg(windows)]
    shell::run();
    #[cfg(not(windows))]
    {
        eprintln!("vaultsyncd desktop runs on Windows 11 x64");
        std::process::exit(2);
    }
}
