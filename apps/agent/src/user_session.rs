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
use vaultsyncd_platform_windows::files::RootHandle;
fn identity() -> io::Result<String> {
    let mut bytes = [0; 16];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn child(parent: &RootHandle, path: &Path, name: &str) -> io::Result<RootHandle> {
    match std::fs::symlink_metadata(path.join(name)) {
        Ok(_) => RootHandle::open(&path.join(name)),
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
    let base = RootHandle::open(local_app_data)?;
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
    let local = PathBuf::from(
        std::env::var_os("LOCALAPPDATA").ok_or_else(|| io::Error::other("missing LOCALAPPDATA"))?,
    );
    let config = prepare(&local)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(0x0020_0000)
        .open(config.parent().unwrap().join("agent.log"))?;
    if log.metadata()?.file_attributes() & 0x400 != 0 {
        return Err(io::Error::other("agent log must not be a reparse point"));
    }
    Command::new(std::env::current_exe()?)
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .creation_flags(0x0800_0000)
        .spawn()?;
    Ok(())
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
