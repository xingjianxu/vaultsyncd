//! Windowless installed session launcher; the agent retains its console/control-signal contract.
#![cfg_attr(windows, windows_subsystem = "windows")]
fn main() {
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
