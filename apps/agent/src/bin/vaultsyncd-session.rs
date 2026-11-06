//! Windowless installed session launcher; the agent retains its console/control-signal contract.
#![cfg_attr(windows, windows_subsystem = "windows")]
fn main() {
    if std::env::args_os().nth(1).is_some() {
        eprintln!("vaultsyncd session launcher accepts no arguments");
        std::process::exit(2);
    }
    #[cfg(windows)]
    if let Err(error) = vaultsyncd_agent::user_session::launch() {
        eprintln!("user session startup failed: {error}");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    {
        eprintln!("vaultsyncd session startup requires Windows");
        std::process::exit(2);
    }
}
