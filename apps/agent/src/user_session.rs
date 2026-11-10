//! Fixed per-user installed entry point. The GUI never owns the agent process lifetime.
use crate::config::{AgentConfig, ServerConfig};
use rand_core::{OsRng, RngCore};
use std::{
    fs::OpenOptions,
    io::{self, Write},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use vaultsyncd_platform_windows::files::VaultHandle;
fn identity() -> io::Result<String> {
    let mut bytes = [0; 16];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn child(parent: &VaultHandle, path: &Path, name: &str) -> io::Result<VaultHandle> {
    match std::fs::symlink_metadata(path.join(name)) {
        Ok(_) => VaultHandle::open(&path.join(name)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => parent.create_new_directory(name),
        Err(e) => Err(e),
    }
}
/// Create a credential-free initial profile only when absent. Existing state is never replaced.
pub fn prepare(local_app_data: &Path) -> io::Result<PathBuf> {
    if !local_app_data.is_absolute() {
        return Err(io::Error::other(
            "LOCALAPPDATA must be an absolute user directory",
        ));
    }
    let base = VaultHandle::open(local_app_data)?;
    let product = child(&base, local_app_data, "vaultsyncd")?;
    let product_path = local_app_data.join("vaultsyncd");
    let profile = child(&product, &product_path, "agent")?;
    let profile_path = product_path.join("agent");
    let _state = child(&profile, &profile_path, "state")?;
    let path = profile_path.join("agent.json");
    match profile.open_read("agent.json") {
        Ok(_held) => {
            AgentConfig::load(&path)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let config = AgentConfig {
                server: ServerConfig {
                    id: identity()?,
                    url: "https://localhost".into(),
                    development_http: false,
                    ca_file: None,
                    username: None,
                },
                device_id: identity()?,
                state_dir: profile_path.join("state"),
                poll_millis: 2000,
                bindings: vec![],
            };
            config.validate()?;
            let mut file = profile.create_new_file("agent.json")?;
            file.write_all(&serde_json::to_vec_pretty(&config).map_err(io::Error::other)?)?;
            file.sync_all()?;
        }
        Err(e) => return Err(e),
    }
    Ok(path)
}
/// The startup entry spawns this installed executable with a fixed config and redirects diagnostics.
/// Arguments are passed directly through CreateProcess; no shell or user-selected executable.
pub fn launch() -> io::Result<()> {
    launch_mode(false)
}
/// Start only the controller-owned update trial, with the ordinary startup barrier preserved.
pub fn launch_update_trial() -> io::Result<()> {
    launch_mode(true)
}
fn launch_mode(trial: bool) -> io::Result<()> {
    let local = PathBuf::from(
        std::env::var_os("LOCALAPPDATA").ok_or_else(|| io::Error::other("missing LOCALAPPDATA"))?,
    );
    let config = prepare(&local)?;
    spawn_mode(&config, trial)?;
    Ok(())
}
/// Shared process adapter for installed and ZIP entry points. The caller owns readiness checks.
pub(crate) fn spawn(config: &Path) -> io::Result<std::process::Child> {
    spawn_mode(config, false)
}
fn spawn_mode(config: &Path, trial: bool) -> io::Result<std::process::Child> {
    let parsed = AgentConfig::load(config)?;
    if trial {
        crate::maintenance::admit_update_trial(&parsed)
    } else {
        crate::maintenance::admit_startup(&parsed)
    }
    .map_err(|e| io::Error::other(e.to_string()))?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(0x0020_0000)
        .open(config.parent().unwrap().join("agent.log"))?;
    if log.metadata()?.file_attributes() & 0x400 != 0 {
        return Err(io::Error::other("agent log must not be a reparse point"));
    }
    let executable = std::env::current_exe()?;
    let agent = executable
        .parent()
        .ok_or_else(|| io::Error::other("installed program directory missing"))?
        .join("vaultsyncd-agent.exe");
    let mut command = Command::new(agent);
    command.arg("--config").arg(config);
    if trial {
        command.arg("--update-trial");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .creation_flags(0x0800_0000)
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installed_profile_preserves_identity_and_refuses_to_replace_invalid_existing_config() {
        let base = std::env::temp_dir().join(format!(
            "s099 用户 profile-{}-{}",
            std::process::id(),
            identity().unwrap()
        ));
        std::fs::create_dir(&base).unwrap();
        let config = prepare(&base).unwrap();
        let first = std::fs::read(&config).unwrap();
        let value = AgentConfig::load(&config).unwrap();
        assert!(value.bindings.is_empty());
        assert!(!value.server.development_http);
        std::fs::write(
            value.state_dir.join("preserve.bin"),
            b"saved unconfirmed state",
        )
        .unwrap();
        assert_eq!(prepare(&base).unwrap(), config);
        assert_eq!(std::fs::read(&config).unwrap(), first);
        std::fs::write(&config, b"invalid existing configuration").unwrap();
        assert!(prepare(&base).is_err());
        assert_eq!(
            std::fs::read(&config).unwrap(),
            b"invalid existing configuration"
        );
        assert_eq!(
            std::fs::read(value.state_dir.join("preserve.bin")).unwrap(),
            b"saved unconfirmed state"
        );
        assert!(prepare(Path::new("relative")).is_err());
        std::fs::remove_dir_all(base).unwrap();
    }
}
