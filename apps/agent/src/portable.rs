//! ZIP is program-only, with one persistent local owner; it is not an upgrade bypass.
use crate::{config::AgentConfig, instance::InstanceGuard, maintenance::storage, user_session};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
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
    "vaultsyncd-zip.exe",
    "installed-files.json",
    "UNSIGNED-TEST.txt",
    "Start vaultsyncd.cmd",
    "Stop vaultsyncd.cmd",
    "README-ZIP.txt",
];
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    format: u32,
    program: PathBuf,
    manifest: String,
}
fn local() -> io::Result<PathBuf> {
    let path = PathBuf::from(
        std::env::var_os("LOCALAPPDATA").ok_or_else(|| io::Error::other("LOCALAPPDATA absent"))?,
    );
    VaultHandle::open(&path)?;
    Ok(path)
}
fn program() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .ok_or_else(|| io::Error::other("program directory absent"))?;
    // Validate before canonicalizing: canonicalize must not silently follow a junction.
    VaultHandle::open(directory)?;
    let canonical = fs::canonicalize(directory)?;
    // Windows canonicalize returns a verbatim DOS path, while VaultHandle deliberately accepts
    // only ordinary absolute drive paths. Revalidate after removing that API-only prefix.
    let text = canonical
        .to_str()
        .ok_or_else(|| io::Error::other("program path is not Unicode"))?;
    let directory = PathBuf::from(text.strip_prefix("\\\\?\\").unwrap_or(text));
    VaultHandle::open(&directory)?;
    Ok(directory)
}
fn mismatch() -> io::Error {
    io::Error::other(
        "ZIP_OWNER_MISMATCH: this local profile belongs to another program path/source. Keep the original ZIP; moving, overwriting or switching to setup is not a supported upgrade. Do not delete zip-owner.json or state to bypass maintenance.",
    )
}
pub(crate) fn verify_zip(directory: &Path) -> io::Result<String> {
    let installed = storage::verify_program(directory)?;
    let vault = VaultHandle::open(directory)?;
    let _manifest_pin = vault.open_read("zip-files.json")?;
    let path = directory.join("zip-files.json");
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?;
    let files = manifest["files"].as_array().ok_or_else(mismatch)?;
    if manifest["format"] != 1
        || manifest["signature_status"] != "unsigned-test"
        || manifest["source"] != installed["source"]
        || files.len() != ZIP_FILES.len()
    {
        return Err(mismatch());
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
                "ZIP file hash/size mismatch: {name}"
            )));
        }
    }
    storage::hash(&path)
}
/// Validate the original ZIP ownership before an explicit offline upgrade.
pub(crate) fn maintenance_owner(directory: &Path) -> io::Result<Vec<u8>> {
    let owner = local()?.join("vaultsyncd/zip-owner.json");
    check_owner(&owner, directory, &verify_zip(directory)?)?;
    fs::read(owner)
}
pub(crate) fn installed_owner(directory: &Path) -> io::Result<()> {
    let registered = webview::installed_directory()?
        .ok_or_else(|| io::Error::other("installed program registration missing"))?;
    VaultHandle::open(&registered)?;
    VaultHandle::open(directory)?;
    if fs::canonicalize(registered)? != fs::canonicalize(directory)? {
        return Err(io::Error::other(
            "installed program registration target mismatch",
        ));
    }
    Ok(())
}
/// Only called while the writer lock and durable maintenance barrier are held.
pub(crate) fn replace_owner(bytes: &[u8]) -> io::Result<()> {
    let base = local()?.join("vaultsyncd");
    let vault = VaultHandle::open(&base)?;
    let _: Owner = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    let mut file = vault.open_exclusive_write("zip-owner.json")?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()
}
pub(crate) fn upgrade_owner(directory: &Path) -> io::Result<()> {
    let manifest = verify_zip(directory)?;
    replace_owner(
        &serde_json::to_vec_pretty(&Owner {
            format: 1,
            program: directory.into(),
            manifest,
        })
        .map_err(io::Error::other)?,
    )
}
fn check_owner(path: &Path, directory: &Path, manifest: &str) -> io::Result<()> {
    let vault = VaultHandle::open(
        path.parent()
            .ok_or_else(|| io::Error::other("owner parent absent"))?,
    )?;
    let _pin = vault.open_read("zip-owner.json")?;
    let owner: Owner = serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
    if owner.format != 1 || owner.program != directory || owner.manifest != manifest {
        return Err(mismatch());
    }
    Ok(())
}
/// Also enforced by ordinary --config and installed session startup, before opening SQLite.
pub(crate) fn admit(config: &AgentConfig) -> io::Result<()> {
    let base = local()?.join("vaultsyncd");
    let owner = base.join("zip-owner.json");
    let directory = program()?;
    if owner.try_exists()? {
        if fs::canonicalize(&config.state_dir)? != fs::canonicalize(base.join("agent/state"))? {
            return Err(mismatch());
        }
        let manifest = verify_zip(&directory)?;
        check_owner(&owner, &directory, &manifest)?;
    } else if directory.join("zip-files.json").try_exists()? {
        return Err(io::Error::other(
            "Use Start vaultsyncd.cmd to initialize ZIP ownership first",
        ));
    }
    Ok(())
}
fn claim() -> Result<PathBuf> {
    let local = local()?;
    let directory = program()?;
    let manifest = verify_zip(&directory)?;
    if webview::installed_program()? {
        return Err("ZIP_INSTALLED_CLIENT: a setup-managed client is registered for this user; use its S100 maintenance path, not ZIP".into());
    }
    let base = local.join("vaultsyncd");
    let local_vault = VaultHandle::open(&local)?;
    if !base.try_exists()? {
        local_vault.create_new_directory("vaultsyncd")?;
    }
    let base_vault = VaultHandle::open(&base)?;
    if base_vault.overlaps(&VaultHandle::open(&directory)?) {
        return Err("ZIP program/state directories must not overlap".into());
    }
    let owner = base.join("zip-owner.json");
    if owner.try_exists()? {
        check_owner(&owner, &directory, &manifest)?;
        return Ok(user_session::prepare(&local)?);
    }
    // Never adopt even an offline installed/older profile. Partial first claim is recoverable only
    // when all that exists is our empty directory/lock scaffolding, not configuration or data.
    let profile = base.join("agent");
    if !profile.try_exists()? {
        base_vault.create_new_directory("agent")?;
    }
    let profile_root = VaultHandle::open(&profile)?;
    let state = profile.join("state");
    if !state.try_exists()? {
        profile_root.create_new_directory("state")?;
    }
    VaultHandle::open(&state)?;
    let _guard = InstanceGuard::acquire(&state, "vaultsyncd-agent")?;
    if owner.try_exists()? {
        check_owner(&owner, &directory, &manifest)?;
    } else {
        for entry in fs::read_dir(&profile)? {
            if entry?.file_name() != "state" {
                return Err("ZIP_EXISTING_PROFILE: existing installed/unknown profile is preserved; ZIP cannot adopt it".into());
            }
        }
        for entry in fs::read_dir(&state)? {
            if entry?.file_name() != "agent.writer.lock" {
                return Err("ZIP_EXISTING_STATE: existing local state is preserved".into());
            }
        }
        let mut file = base_vault.create_new_file("zip-owner.json")?;
        file.write_all(&serde_json::to_vec_pretty(&Owner {
            format: 1,
            program: directory,
            manifest,
        })?)?;
        file.sync_all()?;
    }
    Ok(user_session::prepare(&local)?)
}
async fn rpc(
    endpoint: &Endpoint,
    expected: &Path,
    name: CommandName,
    parameters: Option<CommandParameters>,
) -> Result<(
    GetStatusResult,
    vaultsyncd_platform_windows::ipc::ServerProcess,
)> {
    let (response, process) = endpoint
        .request_verified(
            expected,
            &Request {
                request_id: format!("zip-{}", std::process::id()),
                ipc_version: vaultsyncd_local_ipc::version::supported(),
                command: Command { name, parameters },
            },
        )
        .await?;
    if let Some(error) = response.error {
        return Err(format!("ZIP IPC {:?}: {}", error.code, error.message).into());
    }
    Ok((response.result.ok_or("missing IPC health result")?, process))
}
fn missing_pipe(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(2) | Some(3))
}
/// Start only when the authenticated session endpoint is absent; health, never process exit,
/// establishes readiness. This is shared by ZIP Start and the direct desktop entry point.
async fn ensure_ready(config_path: &Path, endpoint: &Endpoint, agent: &Path) -> Result<()> {
    let mut child = match endpoint.connect().await {
        Ok(_) => None,
        Err(e) if missing_pipe(&e) => Some(user_session::spawn(config_path)?),
        Err(e) => return Err(e.into()),
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match rpc(endpoint, agent, CommandName::GetStatus, None).await {
            Ok((_, process)) => {
                if process.wait(0)? {
                    return Err("agent exited during health check".into());
                }
                break;
            }
            Err(e)
                if e.downcast_ref::<io::Error>().is_some_and(missing_pipe)
                    && Instant::now() < deadline =>
            {
                if let Some(child) = &mut child
                    && let Some(status) = child.try_wait()?
                {
                    // Another simultaneous Start may have won the writer lock. Only real IPC
                    // health, never the spawn exit code, can establish readiness.
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
    Ok(())
}
/// Fixed sibling launcher for a direct desktop start. Preserve existing configuration and enforce
/// ZIP ownership and maintenance barriers before starting a new background process.
pub async fn ensure_user_session() -> Result<()> {
    let directory = program()?;
    let config_path = if directory.join("zip-files.json").try_exists()? {
        webview::require()?;
        claim()?
    } else {
        user_session::prepare(&local()?)?
    };
    let config = AgentConfig::load(&config_path)?;
    crate::maintenance::admit_startup(&config)?;
    let endpoint = Endpoint::current("vaultsyncd-agent")?;
    ensure_ready(
        &config_path,
        &endpoint,
        &directory.join("vaultsyncd-agent.exe"),
    )
    .await
}
/// No shell interpolation, registration, or lifetime coupling to the desktop process.
pub async fn run(action: &str) -> Result<()> {
    if !matches!(action, "start" | "stop" | "health") {
        return Err("usage: vaultsyncd-zip start|stop|health".into());
    }
    if action == "start" {
        webview::require()?;
    }
    let config_path = if action == "start" {
        claim()?
    } else {
        local()?.join("vaultsyncd/agent/agent.json")
    };
    let config = AgentConfig::load(&config_path)?;
    admit(&config)?;
    crate::maintenance::admit_startup(&config)?;
    let directory = program()?;
    let endpoint = Endpoint::current("vaultsyncd-agent")?;
    let agent = directory.join("vaultsyncd-agent.exe");
    if action == "start" {
        ensure_ready(&config_path, &endpoint, &agent).await?;
        // The desktop holds a per-user mutex, so a second Start reuses the open window instead
        // of creating a duplicate one on the same agent.
        if vaultsyncd_platform_windows::single::claim(DESKTOP_SCOPE)?.is_none() {
            println!("Agent IPC healthy; the desktop window is already open in this session.");
            return Ok(());
        }
        ProcessCommand::new(directory.join("vaultsyncd-desktop.exe"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        println!(
            "Agent IPC healthy; desktop opened. Closing the desktop does not stop synchronization."
        );
        return Ok(());
    }
    let (status, _) = match rpc(&endpoint, &agent, CommandName::GetStatus, None).await {
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
    // PrepareMaintenance persists pause controls before exporting and exiting. They deliberately
    // remain paused on next start; never auto-resume a binding the user had already paused.
    let base = config_path.parent().ok_or("profile absent")?;
    let vault = VaultHandle::open(base)?;
    let name = format!(
        "zip-stop-{}-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        std::process::id()
    );
    vault.create_new_directory(&name)?;
    let destination = base.join(name);
    // Retain the process from the very connection that executes preparation, not a prior probe.
    let (_, process) = rpc(
        &endpoint,
        &agent,
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
mod webview;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zip_inventory_refuses_missing_duplicate_and_changed_files() {
        let base = std::env::temp_dir().join(format!("s109 ZIP 清单 {}", std::process::id()));
        fs::create_dir(&base).unwrap();
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
        fs::write(base.join("vaultsyncd-zip.exe"), b"changed launcher").unwrap();
        assert!(verify_zip(&base).is_err());
        fs::remove_file(base.join("vaultsyncd-zip.exe")).unwrap();
        assert!(verify_zip(&base).is_err());
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn ownership_is_exact_and_corruption_is_not_adopted() {
        let base = std::env::temp_dir().join(format!("s109 中文 ownership {}", std::process::id()));
        fs::create_dir(&base).unwrap();
        let owner = base.join("zip-owner.json");
        let directory = base.join("程序 空格");
        fs::write(
            &owner,
            serde_json::to_vec(&Owner {
                format: 1,
                program: directory.clone(),
                manifest: "source-a".into(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(check_owner(&owner, &directory, "source-a").is_ok());
        assert!(check_owner(&owner, &base.join("other"), "source-a").is_err());
        assert!(check_owner(&owner, &directory, "source-b").is_err());
        fs::write(&owner, b"broken").unwrap();
        assert!(check_owner(&owner, &directory, "source-a").is_err());
        fs::remove_dir_all(base).unwrap();
    }
}
