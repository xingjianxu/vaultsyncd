//! Windowless installed session launcher; the agent retains its console/control-signal contract.
#![cfg_attr(windows, windows_subsystem = "windows")]
fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let trial = args.as_slice() == ["--update-trial"];
    if !args.is_empty() && !trial {
        eprintln!("vaultsyncd session launcher accepts only the internal --update-trial option");
        std::process::exit(2);
    }
    #[cfg(windows)]
    if let Err(error) = if trial {
        vaultsyncd_agent::user_session::launch_update_trial()
    } else {
        vaultsyncd_agent::user_session::launch()
    } {
        eprintln!("user session startup failed: {error}");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    {
        eprintln!("vaultsyncd session startup requires Windows");
        std::process::exit(2);
    }
}
