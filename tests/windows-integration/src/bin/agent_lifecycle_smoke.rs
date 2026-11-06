//! Native S070 lifecycle probes, before synchronization prerequisites are integrated.
#![allow(unsafe_code)]
#[cfg(windows)]
#[path = "../../../../apps/agent/src/credentials.rs"]
mod credentials;
#[cfg(windows)]
#[path = "../../../../apps/agent/src/instance.rs"]
mod instance;
#[cfg(windows)]
#[path = "../../../../apps/agent/src/shutdown.rs"]
mod shutdown;

#[cfg(windows)]
fn main() -> std::io::Result<()> {
    use std::{
        io::{BufRead, Write},
        os::windows::process::CommandExt,
        path::PathBuf,
        process::{Command, Stdio},
        thread,
        time::Duration,
    };
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "child") {
        let guard = instance::InstanceGuard::acquire(&PathBuf::from(&args[2]), &args[3]);
        std::process::exit(if guard.is_err() { 0 } else { 71 });
    }
    if args.get(1).is_some_and(|a| a == "hold") {
        let _guard = instance::InstanceGuard::acquire(&PathBuf::from(&args[2]), &args[3])?;
        println!("ready");
        std::io::stdout().flush()?;
        thread::sleep(Duration::from_secs(20));
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "signal") {
        let stop = shutdown::Shutdown::install()?;
        println!("ready");
        std::io::stdout().flush()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !stop.requested() {
            if std::time::Instant::now() > deadline {
                std::process::exit(72);
            }
            thread::sleep(Duration::from_millis(5));
        }
        return Ok(());
    }
    let root = std::env::current_exe()?
        .parent()
        .unwrap()
        .join(format!("s070-life-{}", std::process::id()));
    std::fs::create_dir(&root)?;
    let scope = format!("s070-life-{}", std::process::id());
    let guard = instance::InstanceGuard::acquire(&root, &scope)?;
    let status = Command::new(std::env::current_exe()?)
        .arg("child")
        .arg(&root)
        .arg(&scope)
        .status()?;
    assert!(status.success());
    println!("check=pass second_process_mutex_refused_before_database");
    let status = Command::new(std::env::current_exe()?)
        .arg("child")
        .arg(&root)
        .arg(format!("{scope}-other-session"))
        .status()?;
    assert!(status.success());
    println!("check=pass different_mutex_same_state_lock_refused");
    drop(guard);
    let second = instance::InstanceGuard::acquire(&root, &scope)?;
    drop(second);
    println!("check=pass clean_restart_reacquires_ownership");
    let mut holder = Command::new(std::env::current_exe()?)
        .arg("hold")
        .arg(&root)
        .arg(&scope)
        .stdout(Stdio::piped())
        .spawn()?;
    let mut ready = String::new();
    std::io::BufReader::new(holder.stdout.take().unwrap()).read_line(&mut ready)?;
    assert_eq!(ready.trim(), "ready");
    holder.kill()?;
    holder.wait()?;
    drop(instance::InstanceGuard::acquire(&root, &scope)?);
    println!("check=pass killed_process_releases_mutex_and_state_ownership");
    let vault = credentials::CredentialVault::new(&format!("{:032x}", std::process::id()))?;
    vault.erase()?;
    assert_eq!(vault.load()?, None);
    vault.store(b"native-test-only-not-a-real-token")?;
    assert_eq!(vault.load()?.unwrap(), b"native-test-only-not-a-real-token");
    vault.erase()?;
    assert_eq!(vault.load()?, None);
    println!("check=pass windows_credentials_roundtrip_and_erase");
    let stop = shutdown::Shutdown::install()?;
    assert!(!stop.requested());
    drop(stop);
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AllocConsole() -> i32;
        fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
    }
    unsafe {
        AllocConsole();
    }
    let mut child = Command::new(std::env::current_exe()?)
        .arg("signal")
        .creation_flags(0x00000200)
        .stdout(Stdio::piped())
        .spawn()?;
    ready.clear();
    std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut ready)?;
    assert_eq!(ready.trim(), "ready");
    if unsafe { GenerateConsoleCtrlEvent(1, child.id()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    assert!(child.wait()?.success());
    println!("check=pass real_ctrl_break_drains_process_loop");
    std::fs::remove_dir_all(&root)?;
    println!("check=pass console_handler_install_remove");
    Ok(())
}
#[cfg(not(windows))]
fn main() {
    eprintln!("Windows native lifecycle probe only");
    std::process::exit(3);
}
