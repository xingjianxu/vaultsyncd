//! Independent per-user session control. Program directories never own the local profile.
use crate::{config::AgentConfig, instance::InstanceGuard, maintenance::storage, user_session};
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use vaultsyncd_local_ipc::generated::*;
use vaultsyncd_platform_windows::{files::VaultHandle, ipc::Endpoint};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const DESKTOP_SCOPE: &str = "vaultsyncd-desktop";
pub(crate) const ZIP_FILES: &[&str] = &[
    "vaultsyncd-agent.exe",
    "vaultsyncd-session.exe",
    "vaultsyncd-cli.exe",
    "vaultsyncd-desktop.exe",
    "installed-files.json",
    "UNSIGNED-TEST.txt",
    "Start vaultsyncd.cmd",
    "Stop vaultsyncd.cmd",
    "README-ZIP.txt",
];
fn local() -> io::Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("LOCALAPPDATA").ok_or_else(|| io::Error::other("LOCALAPPDATA absent"))?,
    ))
}
fn program() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    Ok(executable
        .parent()
        .ok_or_else(|| io::Error::other("program directory absent"))?
        .to_path_buf())
}
/// Candidate verification belongs only to explicit update transactions, never ordinary startup.
pub(crate) fn verify_zip(directory: &Path) -> io::Result<String> {
    let installed = storage::verify_program(directory)?;
    let vault = VaultHandle::open(directory)?;
    let _manifest_pin = vault.open_read("zip-files.json")?;
    let path = directory.join("zip-files.json");
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?;
    let files = manifest["files"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing update package inventory"))?;
    if manifest["format"] != 1
        || manifest["signature_status"] != "unsigned-test"
        || manifest["source"] != installed["source"]
        || files.len() != ZIP_FILES.len()
    {
        return Err(io::Error::other("update package inventory mismatch"));
    }
    let mut pins = Vec::new();
    for &name in ZIP_FILES {
        pins.push(vault.open_read(name)?);
        let entries: Vec<_> = files
            .iter()
            .filter(|f| f["path"].as_str() == Some(name))
            .collect();
        if entries.len() != 1
            || entries[0]["sha256"].as_str() != Some(&storage::hash(&directory.join(name))?)
            || entries[0]["bytes"].as_u64() != Some(fs::metadata(directory.join(name))?.len())
        {
            return Err(io::Error::other(format!(
                "update package file hash/size mismatch: {name}"
            )));
        }
    }
    storage::hash(&path)
}
/// Installed maintenance retains its real registered target; portable folders need no owner file.
pub(crate) fn is_installed(directory: &Path) -> io::Result<bool> {
    let Some(registered) = registration::installed_directory()? else {
        return Ok(false);
    };
    same_program(&registered, directory)
}
fn same_program(registered: &Path, directory: &Path) -> io::Result<bool> {
    match fs::canonicalize(registered) {
        Ok(registered) => Ok(registered == fs::canonicalize(directory)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}
pub(crate) fn require_installed_target(directory: &Path) -> io::Result<()> {
    if !is_installed(directory)? {
        return Err(io::Error::other(
            "installed program registration target mismatch",
        ));
    }
    Ok(())
}
async fn rpc(
    endpoint: &Endpoint,
    name: CommandName,
    parameters: Option<CommandParameters>,
) -> Result<(
    GetStatusResult,
    vaultsyncd_platform_windows::ipc::ServerProcess,
)> {
    let (response, process) = endpoint
        .request_authenticated(&Request {
            request_id: format!("session-{}", std::process::id()),
            ipc_version: vaultsyncd_local_ipc::version::supported(),
            command: Command { name, parameters },
        })
        .await?;
    if let Some(error) = response.error {
        return Err(format!("session IPC {:?}: {}", error.code, error.message).into());
    }
    Ok((response.result.ok_or("missing IPC health result")?, process))
}
fn missing_pipe(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(2) | Some(3))
}
/// Only spawn when the authenticated endpoint is absent. A same-user agent from another folder
/// is reusable; an unhealthy/busy endpoint never permits starting a competing writer.
async fn ensure_ready(config_path: &Path, endpoint: &Endpoint) -> Result<()> {
    let mut child = match endpoint.connect().await {
        Ok(_) => None,
        Err(e) if missing_pipe(&e) => Some(user_session::spawn(config_path)?),
        Err(e) => return Err(e.into()),
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match rpc(endpoint, CommandName::GetStatus, None).await {
            Ok((_, process)) => {
                if process.wait(0)? {
                    return Err("agent exited during health check".into());
                }
                return Ok(());
            }
            Err(e)
                if e.downcast_ref::<io::Error>().is_some_and(missing_pipe)
                    && Instant::now() < deadline =>
            {
                if let Some(child) = &mut child
                    && let Some(status) = child.try_wait()?
                {
                    // A simultaneous Start may have won the writer lock; only real IPC health,
                    // never a launcher's exit code, can establish readiness.
                    if status.code() != Some(2) {
                        return Err(
                            format!("agent startup failed: {status}; inspect agent.log").into()
                        );
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => {
                return Err(format!(
                    "agent IPC not healthy; no GUI opened: {e}; inspect agent.log"
                )
                .into());
            }
        }
    }
}
/// No package manifests, program provenance, or legacy owner files participate in startup.
/// WebView2 is required by the desktop's actual WebView creation, not by background session IPC.
pub async fn ensure_user_session() -> Result<()> {
    let config_path = user_session::prepare(&local()?)?;
    let config = AgentConfig::load(&config_path)?;
    crate::maintenance::admit_startup(&config)?;
    ensure_ready(&config_path, &Endpoint::current("vaultsyncd-agent")?).await
}
/// No shell interpolation, registration, or lifetime coupling to the desktop process.
pub async fn run(action: &str) -> Result<()> {
    if !matches!(action, "start" | "stop" | "health") {
        return Err("usage: vaultsyncd-agent --user-session start|stop|health".into());
    }
    let config_path = if action == "start" {
        user_session::prepare(&local()?)?
    } else {
        local()?.join("vaultsyncd/agent/agent.json")
    };
    let config = AgentConfig::load(&config_path)?;
    crate::maintenance::admit_startup(&config)?;
    let endpoint = Endpoint::current("vaultsyncd-agent")?;
    if action == "start" {
        ensure_ready(&config_path, &endpoint).await?;
        // The desktop itself takes the mutex; this probe avoids launching a duplicate window.
        if vaultsyncd_platform_windows::single::claim(DESKTOP_SCOPE)?.is_none() {
            println!("Agent IPC healthy; the desktop window is already open in this session.");
            return Ok(());
        }
        ProcessCommand::new(program()?.join("vaultsyncd-desktop.exe"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        println!(
            "Agent IPC healthy; desktop opened. Closing the desktop does not stop synchronization."
        );
        return Ok(());
    }
    let (status, _) = match rpc(&endpoint, CommandName::GetStatus, None).await {
        Ok(verified) => verified,
        Err(e) if e.downcast_ref::<io::Error>().is_some_and(missing_pipe) && action == "stop" => {
            let _guard = InstanceGuard::acquire(&config.state_dir, "vaultsyncd-agent")?;
            println!("Agent is already stopped; writer lock is free. No state was changed.");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    if action == "health" {
        println!("{}", serde_json::to_string(&status)?);
        return Ok(());
    }
    // Preparation pauses, exports and exits; never auto-resume a user's paused bindings.
    let base = config_path.parent().ok_or("profile absent")?;
    let vault = VaultHandle::open(base)?;
    let name = format!(
        "session-stop-{}-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        std::process::id()
    );
    vault.create_new_directory(&name)?;
    let destination = base.join(name);
    // Wait on the process from the exact authenticated connection that prepared maintenance,
    // not a prior health probe, path lookup, arbitrary PID, or replacement pipe instance.
    let (_, process) = rpc(
        &endpoint,
        CommandName::PrepareMaintenance,
        Some(CommandParameters {
            destination: Some(destination.to_str().ok_or("invalid export path")?.into()),
            ..Default::default()
        }),
    )
    .await?;
    if !process.wait(60_000)? {
        return Err("maintenance preparation did not exit; no force kill attempted; bindings may be paused, inspect agent.log".into());
    }
    let _guard = InstanceGuard::acquire(&config.state_dir, "vaultsyncd-agent")?;
    println!(
        "Stopped safely; writer lock released. Recovery exports: {}. Bindings remain PAUSED after restart; explicitly Resume each desired binding in the GUI/CLI. Vault, cache, dirty content and provider registration were retained.",
        destination.display()
    );
    Ok(())
}
mod registration;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn update_inventory_refuses_missing_duplicate_and_changed_files_without_a_launcher() {
        let base =
            std::env::temp_dir().join(format!("s185-update-inventory-{}", std::process::id()));
        fs::create_dir(&base).unwrap();
        assert!(!ZIP_FILES.contains(&"vaultsyncd-zip.exe"));
        for &name in ZIP_FILES {
            fs::write(base.join(name), name.as_bytes()).unwrap();
        }
        let item = |name: &str| serde_json::json!({"path":name,"bytes":fs::metadata(base.join(name)).unwrap().len(),"sha256":storage::hash(&base.join(name)).unwrap()});
        fs::write(base.join("installed-files.json"), serde_json::to_vec(&serde_json::json!({"signature_status":"unsigned-test","files":ZIP_FILES[..4].iter().map(|n|item(n)).collect::<Vec<_>>()})).unwrap()).unwrap();
        let manifest = serde_json::json!({"format":1,"signature_status":"unsigned-test","files":ZIP_FILES.iter().map(|n|item(n)).collect::<Vec<_>>()});
        let path = base.join("zip-files.json");
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(verify_zip(&base).is_ok());
        let mut duplicate = manifest.clone();
        duplicate["files"][4] = duplicate["files"][0].clone();
        fs::write(&path, serde_json::to_vec(&duplicate).unwrap()).unwrap();
        assert!(verify_zip(&base).is_err());
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        fs::write(base.join("Start vaultsyncd.cmd"), b"changed launcher").unwrap();
        assert!(verify_zip(&base).is_err());
        fs::remove_file(base.join("Start vaultsyncd.cmd")).unwrap();
        assert!(verify_zip(&base).is_err());
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn installed_target_classification_does_not_claim_other_program_folders() {
        let base = std::env::temp_dir().join(format!("s185-program-target-{}", std::process::id()));
        let installed = base.join("installed");
        let portable = base.join("portable");
        fs::create_dir_all(&installed).unwrap();
        fs::create_dir(&portable).unwrap();
        assert!(same_program(&installed, &installed).unwrap());
        assert!(!same_program(&installed, &portable).unwrap());
        assert!(!same_program(&base.join("removed-installed"), &portable).unwrap());
        fs::remove_dir_all(base).unwrap();
    }
}
