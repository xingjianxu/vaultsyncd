//! Candidate-owned automatic upgrade; the GUI only downloads and extracts the package.
use crate::{maintenance, maintenance::storage};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    time::{Duration, Instant},
};
use vaultsyncd_local_ipc::generated::*;
use vaultsyncd_platform_windows::{files::VaultHandle, ipc::Endpoint, single};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn launch_agent(program: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    ProcessCommand::new(program.join("vaultsyncd-session.exe"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .spawn()?;
    Ok(())
}
async fn health(program: &Path) -> Result<()> {
    let endpoint = Endpoint::current("vaultsyncd-agent")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let result = endpoint
            .request_verified(
                &program.join("vaultsyncd-agent.exe"),
                &Request {
                    request_id: "auto-update-health".into(),
                    ipc_version: vaultsyncd_local_ipc::version::supported(),
                    command: Command {
                        name: CommandName::GetStatus,
                        parameters: None,
                    },
                },
            )
            .await;
        match result {
            Ok((response, process))
                if response.error.is_none() && response.result.is_some() && !process.wait(0)? =>
            {
                return Ok(());
            }
            Ok(_) => return Err("updated agent returned an unhealthy IPC response".into()),
            Err(e)
                if matches!(e.raw_os_error(), Some(2) | Some(3)) && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            Err(e) => return Err(e.into()),
        }
    }
}
pub(crate) fn registry_version(version: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let windows =
        PathBuf::from(std::env::var_os("SystemRoot").ok_or("Windows system directory missing")?);
    if !windows.is_absolute() {
        return Err("Windows system directory must be absolute".into());
    }
    let registry = windows.join("System32/reg.exe");
    for (key, value) in [
        (r"HKCU\Software\vaultsyncd", "Version"),
        (
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\vaultsyncd",
            "DisplayVersion",
        ),
    ] {
        if !ProcessCommand::new(&registry)
            .args([
                "add", key, "/v", value, "/t", "REG_SZ", "/d", version, "/f", "/reg:64",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .status()?
            .success()
        {
            return Err("installed version registration failed".into());
        }
    }
    Ok(())
}
async fn apply(program: &Path) -> Result<()> {
    storage::verify_program(program)?;
    let backup = maintenance::run("auto-upgrade", program)
        .await?
        .ok_or("missing update backup")?;
    let trial = async {
        launch_agent(program)?;
        health(program).await?;
        maintenance::finish_auto(program, &backup).await
    }
    .await;
    if let Err(failure) = trial {
        if backup.join("complete.json").try_exists()? {
            return Err(format!(
                "更新已完成，但恢复部分同步任务失败，请在界面检查暂停状态：{failure}"
            )
            .into());
        }
        maintenance::recover_auto(program, &backup).await?;
        return Err(format!("更新启动失败，已恢复旧程序及整套状态：{failure}").into());
    }
    Ok(())
}

/// Wait for the old GUI, apply a verified candidate, then restart agent and GUI.
/// An interrupted trial has an explicit recovery entry; it is never silently adopted.
pub async fn run(program: &Path, recover: bool) -> Result<()> {
    VaultHandle::open(program)?;
    let _updating =
        single::claim("vaultsyncd-update-apply")?.ok_or("another update is already applying")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let desktop = loop {
        if let Some(held) = single::claim("vaultsyncd-desktop")? {
            break held;
        }
        if Instant::now() >= deadline {
            return Err("desktop did not exit; update refused".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let result = async {
        if recover {
            let local =
                PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA absent")?);
            let pending = local.join("vaultsyncd/agent/auto-update.json");
            let plan: serde_json::Value = serde_json::from_slice(&fs::read(&pending)?)?;
            let backup = PathBuf::from(plan["backup"].as_str().ok_or("pending backup missing")?);
            if backup.join("complete.json").try_exists()? {
                if Endpoint::current("vaultsyncd-agent")?
                    .connect()
                    .await
                    .is_err()
                {
                    launch_agent(program)?;
                }
                health(program).await?;
                maintenance::finish_auto(program, &backup).await
            } else {
                maintenance::recover_auto(program, &backup).await
            }
        } else {
            apply(program).await
        }
    }
    .await;
    // The session launcher validates the remaining barrier/ownership; never force a writer open.
    let restarted = if result.is_err() || recover {
        async {
            launch_agent(program)?;
            health(program).await
        }
        .await
    } else {
        Ok(())
    };
    drop(desktop);
    ProcessCommand::new(program.join("vaultsyncd-desktop.exe"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let outcome = match (result, restarted) {
        (Err(error), Err(restart)) => {
            Err(format!("{error}; 原客户端重启检查失败：{restart}").into())
        }
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    };
    if let Err(error) = outcome {
        vaultsyncd_platform_windows::shell::dialog::update_result(&format!(
            "自动更新未完成：{error}\n\n日志与恢复备份已保留。"
        ));
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Explicit native-only fixture setup, before any agent opens this database.
    #[tokio::test]
    #[ignore = "requires an isolated S162 native run with no online writer"]
    async fn seed_update_fixture() {
        use crate::config::AgentConfig;
        use std::sync::Arc;
        use vaultsyncd_agent_core::bindings::control::BindingControls;
        use vaultsyncd_client_store::{
            bindings::{NewBinding, SyncMode, insert_binding},
            db::Database,
        };
        use vaultsyncd_domain::{
            ids::{BindingId, DeviceId, ServerId, VaultId},
            version::ServerEpoch,
        };
        let base =
            PathBuf::from(std::env::var_os("VAULTSYNCD_S162_FIXTURE").expect("fixture root"));
        assert!(
            base.to_string_lossy()
                .starts_with(r"C:\vaultsyncd-lab\s162-")
        );
        let config = AgentConfig::load(&base.join("local/vaultsyncd/agent/agent.json")).unwrap();
        let db = Arc::new(
            Database::open(config.state_dir.join("client.sqlite"))
                .await
                .unwrap(),
        );
        for n in [1, 2] {
            let root = base.join(format!("vault{n}"));
            let cache = base.join(format!("cache{n}"));
            fs::create_dir(&root).unwrap();
            fs::create_dir(&cache).unwrap();
            fs::write(
                root.join("unconfirmed.txt"),
                format!("S162 unconfirmed content {n}"),
            )
            .unwrap();
            fs::write(
                cache.join("keep-cache.bin"),
                format!("S162 complete cache {n}"),
            )
            .unwrap();
            let id = BindingId::from_hex(&format!("{n:032x}")).unwrap();
            let mut tx = db.begin_write().await.unwrap();
            insert_binding(
                &mut tx,
                &NewBinding {
                    id,
                    server: ServerId::from_hex(&config.server.id).unwrap(),
                    device: DeviceId::from_hex(&config.device_id).unwrap(),
                    vault: VaultId::from_hex(&format!("{:032x}", n + 10)).unwrap(),
                    local_path: root.to_string_lossy().into_owned(),
                    cache_path: cache.to_string_lossy().into_owned(),
                    mode: SyncMode::OnDemand,
                    rules_version: "1".into(),
                    server_epoch: ServerEpoch::new(1).unwrap(),
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            BindingControls::new(db.clone())
                .set_paused(id, n == 2)
                .await
                .unwrap();
        }
        db.pool().close().await;
    }

    #[tokio::test]
    #[ignore = "requires an isolated S162 native run"]
    async fn stop_update_fixture() {
        let base =
            PathBuf::from(std::env::var_os("VAULTSYNCD_S162_FIXTURE").expect("fixture root"));
        assert!(
            base.to_string_lossy()
                .starts_with(r"C:\vaultsyncd-lab\s162-")
        );
        let program = base.join("中文 程序");
        let destination = base.join("stop-export");
        fs::create_dir(&destination).unwrap();
        let endpoint = Endpoint::current("vaultsyncd-agent").unwrap();
        let (response, process) = endpoint
            .request_verified(
                &program.join("vaultsyncd-agent.exe"),
                &Request {
                    request_id: "S162-fixture-stop".into(),
                    ipc_version: vaultsyncd_local_ipc::version::supported(),
                    command: Command {
                        name: CommandName::PrepareMaintenance,
                        parameters: Some(CommandParameters {
                            destination: Some(destination.to_string_lossy().into_owned()),
                            ..Default::default()
                        }),
                    },
                },
            )
            .await
            .unwrap();
        assert!(response.error.is_none(), "{response:?}");
        assert!(process.wait(60_000).unwrap());
    }
}
