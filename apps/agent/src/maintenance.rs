//! Agent-owned offline upgrade/uninstall transaction. Ordinary writers obey a durable barrier.
pub(crate) mod storage;
use crate::{config::AgentConfig, instance::InstanceGuard, local::error};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use vaultsyncd_agent_core::bindings::control::BindingControls;
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::ids::BindingId;
use vaultsyncd_local_ipc::generated::*;
use vaultsyncd_platform_windows::{files::VaultHandle, ipc::Endpoint};
use vaultsyncd_transfer::ports::PortError;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const LEGACY_SOURCE: &str = "cf1d174c3bd47b4babc2db053514619b2e733b07";
#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Plan {
    program: PathBuf,
    backup: PathBuf,
    action: String,
    paused: Vec<(String, bool)>,
    #[serde(default)]
    zip: bool,
}
fn profile(config: &AgentConfig) -> Result<&Path> {
    Ok(config.state_dir.parent().ok_or("missing agent profile")?)
}
fn marker(config: &AgentConfig) -> Result<PathBuf> {
    Ok(profile(config)?.join("maintenance.json"))
}
/// Trial startup admits status/recovery only and never starts sync/CFAPI before commit.
pub(crate) fn trial_pending(state: &Path) -> std::io::Result<bool> {
    let pending = state
        .parent()
        .ok_or_else(|| std::io::Error::other("profile absent"))?
        .join("auto-update.json");
    let bytes = match fs::read(pending) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let plan: Plan = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    let committed = match fs::read(plan.backup.join("complete.json")) {
        Ok(bytes) => {
            let receipt: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
            if receipt["action"] != "auto-upgrade" || receipt["health"] != "verified" {
                return Err(std::io::Error::other(
                    "invalid automatic update commit receipt",
                ));
            }
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e),
    };
    Ok(!committed)
}
/// A barrier is never silently removed by normal startup, including the login launcher.
/// Program metadata and legacy owner files do not admit or reject ordinary startup.
pub fn admit_startup(config: &AgentConfig) -> std::result::Result<(), PortError> {
    let profile = config
        .state_dir
        .parent()
        .ok_or_else(|| error("profile absent"))?;
    for barrier in ["maintenance.json", "auto-update.json"] {
        if profile.join(barrier).try_exists().map_err(error)? {
            return Err(PortError::rejected(
                "MAINTENANCE_REQUIRED",
                "maintenance or automatic-update transaction is pending; recover it with the retained candidate before starting",
            ));
        }
    }
    Ok(())
}
/// Admit only the installed trial executable while its same-session update controller is alive.
/// Ordinary launchers retain the unconditional maintenance barrier above.
pub(crate) fn admit_update_trial(config: &AgentConfig) -> std::result::Result<(), PortError> {
    let validate = || -> Result<()> {
        if vaultsyncd_platform_windows::single::claim("vaultsyncd-update-apply")?.is_some() {
            return Err("update trial requires an active update controller".into());
        }
        let installed = installed_config()?;
        if fs::canonicalize(&installed.state_dir)? != fs::canonicalize(&config.state_dir)? {
            return Err("update trial state mismatch".into());
        }
        let profile = profile(config)?;
        if profile.join("maintenance.json").try_exists()? {
            return Err("offline maintenance is still in progress".into());
        }
        let plan: Plan = serde_json::from_slice(&fs::read(profile.join("auto-update.json"))?)?;
        let executable = std::env::current_exe()?;
        let program = executable
            .parent()
            .ok_or("trial executable directory absent")?;
        if plan.action != "auto-upgrade"
            || fs::canonicalize(&plan.program)? != fs::canonicalize(program)?
        {
            return Err("update trial program mismatch".into());
        }
        let backups = fs::canonicalize(profile.join("maintenance"))?;
        let backup = fs::canonicalize(&plan.backup)?;
        if backup.parent() != Some(backups.as_path()) {
            return Err("update trial backup outside maintenance directory".into());
        }
        let saved: Plan = serde_json::from_slice(&fs::read(backup.join("trial-plan.json"))?)?;
        let prepared: serde_json::Value =
            serde_json::from_slice(&fs::read(backup.join("prepared.json"))?)?;
        if saved != plan
            || prepared["action"] != "auto-upgrade"
            || prepared["program"] != serde_json::to_value(&plan.program)?
        {
            return Err("update trial plan/receipt mismatch".into());
        }
        let manifest = storage::verify_program(program)?;
        if manifest["source"]["version"].as_str() != Some(env!("CARGO_PKG_VERSION")) {
            return Err("update trial version mismatch".into());
        }
        if plan.zip {
            crate::portable::verify_zip(program)?;
        } else {
            crate::portable::require_installed_target(program)?;
        }
        // Validate the commit receipt too. An uncommitted trial stays read-only and paused.
        trial_pending(&config.state_dir)?;
        Ok(())
    };
    validate().map_err(|e| PortError::rejected("MAINTENANCE_REQUIRED", e.to_string()))
}
async fn rpc(
    endpoint: &Endpoint,
    name: CommandName,
    parameters: Option<CommandParameters>,
    minor: u32,
) -> Result<GetStatusResult> {
    let mut ipc_version = vaultsyncd_local_ipc::version::supported();
    ipc_version.minor = minor;
    let response = endpoint
        .request(&Request {
            request_id: format!("maintenance-{}", std::process::id()),
            ipc_version,
            command: Command { name, parameters },
        })
        .await?;
    if let Some(e) = response.error {
        return Err(format!("maintenance IPC {:?}: {}", e.code, e.message).into());
    }
    Ok(response.result.ok_or("missing management result")?)
}
fn binding_parameters(id: &str) -> Option<CommandParameters> {
    Some(CommandParameters {
        binding_id: Some(id.into()),
        ..Default::default()
    })
}
async fn restore_paused(db: &Arc<Database>, plan: &Plan) -> Result<()> {
    let controls = BindingControls::new(db.clone());
    for (id, paused) in &plan.paused {
        controls
            .set_paused(BindingId::from_hex(id)?, *paused)
            .await?;
    }
    Ok(())
}
async fn rollback(config: &AgentConfig, plan: &Plan) -> Result<()> {
    if plan.backup.join("program.inventory.json").try_exists()? {
        storage::restore(&plan.backup.join("program"), &plan.program)?;
    }
    if plan.backup.join("state.inventory.json").try_exists()? {
        storage::restore(&plan.backup.join("state"), &config.state_dir)?;
        let items: Vec<storage::Item> =
            serde_json::from_slice(&fs::read(plan.backup.join("state.inventory.json"))?)?;
        for name in ["client.sqlite-wal", "client.sqlite-shm"] {
            if !items.iter().any(|i| i.path == name) {
                let p = config.state_dir.join(name);
                if p.try_exists()? {
                    fs::remove_file(p)?;
                }
            }
        }
    }
    // Older ZIP transactions may retain an owner backup. It is inert compatibility evidence:
    // never restore or rewrite it, and never require it to recover the program/state snapshot.
    if !plan.zip
        && plan.action == "auto-upgrade"
        && plan.backup.join("program.inventory.json").try_exists()?
    {
        let old: serde_json::Value =
            serde_json::from_slice(&fs::read(plan.backup.join("program/installed-files.json"))?)?;
        crate::auto_update::registry_version(
            old["source"]["version"]
                .as_str()
                .ok_or("old version missing")?,
        )?;
    }
    // Do not invoke the new migrator after restoring the old whole-state snapshot.
    // Only reverse our known pre-maintenance pause controls on the restored supported schema.
    if !plan.paused.is_empty() {
        use sqlx::Connection;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(config.state_dir.join("client.sqlite"))
            .create_if_missing(false)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let mut connection = sqlx::SqliteConnection::connect_with(&options).await?;
        let quick: String = sqlx::query_scalar("PRAGMA quick_check")
            .fetch_one(&mut connection)
            .await?;
        if quick != "ok" {
            return Err("restored database validation failed; barrier retained".into());
        }
        for (id, paused) in &plan.paused {
            let id = BindingId::from_hex(id)?;
            let changed = sqlx::query("INSERT INTO binding_controls(binding_id,paused) SELECT id,? FROM bindings WHERE id=? ON CONFLICT(binding_id) DO UPDATE SET paused=excluded.paused")
                .bind(*paused)
                .bind(id.as_bytes().as_slice())
                .execute(&mut connection)
                .await?;
            if changed.rows_affected() != 1 {
                return Err("restored pause-control identity missing".into());
            }
        }
        connection.close().await?;
    }
    if !plan.backup.join("rollback-complete.json").try_exists()? {
        storage::json(
            &plan.backup.join("rollback-complete.json"),
            &serde_json::json!({"complete":true,"program":plan.program,"state":"entire fixed offline inventory restored","user_content":"untouched"}),
        )?;
    }
    fs::remove_file(marker(config)?)?;
    Ok(())
}
fn installed_config() -> Result<AgentConfig> {
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA absent")?);
    Ok(AgentConfig::load(
        &local.join("vaultsyncd/agent/agent.json"),
    )?)
}
/// Execute a fixed maintenance action as the agent application, never as a GUI/CLI database consumer.
/// The caller installer disables its own HKCU Run first and starts the fixed launcher only after return.
pub async fn run(action: &str, program: &Path) -> Result<Option<PathBuf>> {
    if !matches!(
        action,
        "upgrade" | "auto-upgrade" | "uninstall-program" | "uninstall-provider" | "recover"
    ) {
        return Err("unknown maintenance action".into());
    }
    let mut config = installed_config()?;
    let zip = !crate::portable::is_installed(program)?;
    let program_root = VaultHandle::open(program)?;
    let state = VaultHandle::open(&config.state_dir)?;
    if program_root.overlaps(&state) {
        return Err("program/state overlap".into());
    }
    let barrier = marker(&config)?;
    if action == "auto-upgrade" && profile(&config)?.join("auto-update.json").try_exists()? {
        return Err("interrupted auto-update exists; use retained candidate --auto-update-recover --program-dir <original program>".into());
    }
    if action == "recover" {
        let plan: Plan = serde_json::from_slice(&fs::read(&barrier)?)?;
        if fs::canonicalize(&plan.program)? != fs::canonicalize(program)? {
            return Err("recovery target mismatch".into());
        }
        let endpoint = Endpoint::current("vaultsyncd-agent")?;
        if endpoint.connect().await.is_ok()
            && !plan.backup.join("state.inventory.json").try_exists()?
        {
            let _process = endpoint
                .maintenance_process(&program.join("vaultsyncd-agent.exe"))
                .await?;
            for (id, paused) in &plan.paused {
                rpc(
                    &endpoint,
                    if *paused {
                        CommandName::PauseBinding
                    } else {
                        CommandName::ResumeBinding
                    },
                    binding_parameters(id),
                    4,
                )
                .await?;
            }
            fs::remove_file(&barrier)?;
            return Ok(None);
        }
        let _guard = InstanceGuard::acquire(&config.state_dir, "vaultsyncd-agent")?;
        rollback(&config, &plan).await?;
        return Ok(None);
    }
    if barrier.try_exists()? {
        return Err("MAINTENANCE_REQUIRED: recover the retained transaction before retry".into());
    }
    let installed = storage::verify_program(program)?;
    let executable = std::env::current_exe()?;
    let candidate = executable.parent().ok_or("candidate directory absent")?;
    if matches!(action, "upgrade" | "auto-upgrade") {
        let next = storage::verify_program(candidate)?;
        if next["source"]["version"].as_str() != Some(env!("CARGO_PKG_VERSION")) {
            return Err("candidate version mismatch".into());
        }
        fn numeric_version(value: &serde_json::Value) -> Result<Vec<u32>> {
            let parts: Vec<u32> = value
                .as_str()
                .ok_or("version absent")?
                .split('.')
                .map(str::parse)
                .collect::<std::result::Result<_, _>>()?;
            if parts.len() != 3 {
                return Err("unsupported product version profile".into());
            }
            Ok(parts)
        }
        if numeric_version(&next["source"]["version"])?
            <= numeric_version(&installed["source"]["version"])?
        {
            return Err("same-version/downgrade replacement refused; use explicit recovery".into());
        }
        if program_root.overlaps(&VaultHandle::open(candidate)?) {
            return Err("upgrade candidate must be separate from installed program".into());
        }
        if action == "auto-upgrade" {
            crate::portable::verify_zip(candidate)?;
        }
    }
    let endpoint = Endpoint::current("vaultsyncd-agent")?;
    // Status 1.0 is the only negotiation request sent before learning the server's minor.
    let online = match endpoint.connect().await {
        Ok(_) => Some(rpc(&endpoint, CommandName::GetStatus, None, 0).await?),
        Err(e) if matches!(e.raw_os_error(), Some(2) | Some(3)) => None,
        Err(e) => return Err(e.into()),
    };
    let process = if online.is_some() {
        Some(
            endpoint
                .maintenance_process(&program.join("vaultsyncd-agent.exe"))
                .await?,
        )
    } else {
        None
    };
    let offline_guard = if online.is_none() {
        Some(InstanceGuard::acquire(
            &config.state_dir,
            "vaultsyncd-agent",
        )?)
    } else {
        None
    };
    let maintenance = profile(&config)?.join("maintenance");
    fs::create_dir_all(&maintenance)?;
    VaultHandle::open(&maintenance)?;
    let mut nonce = [0u8; 16];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut nonce);
    let backup = maintenance.join(vaultsyncd_transfer::hash::to_hex(
        &vaultsyncd_transfer::hash::digest_of(&nonce),
    ));
    fs::create_dir(&backup)?;
    fs::create_dir(backup.join("recovery"))?;
    let paused = online
        .as_ref()
        .and_then(|s| s.management.as_ref())
        .and_then(|m| m.bindings.as_ref())
        .map(|items| {
            items
                .iter()
                .map(|b| (b.binding_id.clone(), b.paused))
                .collect()
        })
        .unwrap_or_default();
    let mut plan = Plan {
        program: program.into(),
        backup,
        action: action.into(),
        paused,
        zip,
    };
    storage::json(&plan.backup.join("plan.json"), &plan)?;
    storage::json(&barrier, &plan)?;
    if let Some(status) = online {
        if status.ipc_protocol.minor >= 5 {
            rpc(
                &endpoint,
                CommandName::PrepareMaintenance,
                Some(CommandParameters {
                    destination: Some(plan.backup.join("recovery").to_string_lossy().into_owned()),
                    ..Default::default()
                }),
                5,
            )
            .await?;
            if !process.as_ref().unwrap().wait(15_000)? {
                return Err("prepared agent did not drain; no files replaced".into());
            }
        } else {
            if installed["source"]["commit"].as_str() != Some(LEGACY_SOURCE)
                || status.agent_version != "0.1.0"
            {
                return Err("legacy maintenance is supported only for the identified S099 unsigned development source".into());
            }
            for (id, _) in &plan.paused {
                rpc(
                    &endpoint,
                    CommandName::PauseBinding,
                    binding_parameters(id),
                    4,
                )
                .await?;
            }
            let paused_status = rpc(&endpoint, CommandName::GetStatus, None, 0).await?;
            if paused_status
                .bindings
                .as_ref()
                .is_none_or(|items| items.iter().any(|b| !b.paused || b.transfers != "0"))
            {
                return Err(
                    "legacy agent still has transfers; preserve/recover and retry after they drain"
                        .into(),
                );
            }
            for (id, _) in &plan.paused {
                rpc(
                    &endpoint,
                    CommandName::ExportRecovery,
                    Some(CommandParameters {
                        binding_id: Some(id.clone()),
                        destination: Some(
                            plan.backup
                                .join("recovery")
                                .join(id)
                                .to_string_lossy()
                                .into_owned(),
                        ),
                        ..Default::default()
                    }),
                    4,
                )
                .await?;
            }
            storage::json(
                &plan.backup.join("legacy-development-stop.json"),
                &serde_json::json!({"source":LEGACY_SOURCE,"paused":true,"transfers":0,"recovery_exported":true,"not_a_formal_previous_release":true}),
            )?;
            process
                .as_ref()
                .unwrap()
                .stop_preserved_development_build()?;
        }
    }
    let _guard = if let Some(guard) = offline_guard {
        guard
    } else {
        InstanceGuard::acquire(&config.state_dir, "vaultsyncd-agent")?
    };
    // The full raw state (including a legacy WAL) is fixed while the writer lock is held.
    storage::backup_state(&config.state_dir, &plan.backup.join("state"))?;
    storage::backup_program(program, &plan.backup.join("program"))?;
    let result=async{
        let db=Arc::new(Database::open(config.state_dir.join("client.sqlite")).await?);
        let work=async{
            crate::management::load_bindings(&db,&mut config).await?;crate::management::admit(&config)?;
            if plan.paused.is_empty(){plan.paused=crate::management::binding_list(&db).await?.iter().map(|b|(b.binding_id.clone(),b.paused)).collect();
                for binding in &config.bindings{let id=BindingId::from_hex(&binding.id)?;let local=Arc::new(crate::local::Local::open(&binding.path,&binding.cache,db.clone(),id).await?);crate::management_export::export(local,&config,plan.backup.join("recovery").join(&binding.id).to_str().ok_or("invalid recovery destination")?).await?;}}
            if action.starts_with("uninstall-"){storage::require_program_idle(program)?;}
            if action=="uninstall-provider"{
                // Preflight every Vault before converting any file. Never unregister partial placeholders.
                for binding in &config.bindings{vaultsyncd_platform_windows::cfapi::maintenance::check_materialized(&binding.path)?;}
                let mut reports=vec![];
                for binding in &config.bindings{
                    let hashes=vaultsyncd_platform_windows::cfapi::maintenance::revert_materialized(&binding.path)?;
                    vaultsyncd_platform_windows::cfapi::maintenance::unregister_materialized(&binding.path)?;
                    let id=BindingId::from_hex(&binding.id)?;if vaultsyncd_platform_windows::shell::registered(id)? { vaultsyncd_platform_windows::shell::unregister(id)?; }
                    BindingControls::new(db.clone()).unbind_keep_data(id).await?;
                    reports.push(serde_json::json!({"binding_id":binding.id,"preserved_hashes":hashes,"only_complete_files":true}));
                }storage::json(&plan.backup.join("provider-removal.json"),&reports)?;
            }else if action=="auto-upgrade" {
                // Trial startup must not sync against a database that may still be rolled back.
                let controls=BindingControls::new(db.clone());
                for binding in &config.bindings { controls.set_paused(BindingId::from_hex(&binding.id)?,true).await?; }
            }else{restore_paused(&db,&plan).await?;}
            Ok::<(),Box<dyn std::error::Error>>(())
        }.await;
        db.pool().close().await;work?;
        if action=="auto-upgrade" { storage::json(&plan.backup.join("trial-plan.json"), &plan)?; }
        if matches!(action,"upgrade"|"auto-upgrade") {
            if zip { storage::replace_zip(candidate,program)?; }
            else { storage::replace_program(candidate,program)?; }
        }
        storage::json(&plan.backup.join(if action=="auto-upgrade"{"prepared.json"}else{"complete.json"}),&serde_json::json!({"action":action,"program":program,"user_data":"preserved","cloud_only":if action=="uninstall-program"{"placeholders retained; unavailable until provider reinstall; not complete files"}else{"no placeholder deletion"},"recovery":plan.backup.join("recovery")}))?;
        if action=="auto-upgrade" { storage::json(&profile(&config)?.join("auto-update.json"),&plan)?; }
        fs::remove_file(&barrier)?;Ok::<(),Box<dyn std::error::Error>>(())
    }.await;
    if let Err(failure) = result {
        match rollback(&config,&plan).await{Ok(())=>return Err(format!("maintenance failed; full offline state/program restored, user data retained: {failure}").into()),Err(recovery)=>return Err(format!("MAINTENANCE_REQUIRED: {failure}; rollback also failed: {recovery}; retained candidate={} backup={}",candidate.display(),plan.backup.display()).into())}
    }
    println!(
        "maintenance=complete action={action} evidence={}",
        plan.backup.display()
    );
    Ok(Some(plan.backup))
}

/// Complete trial startup over IPC; commit before resuming any original binding.
pub(crate) async fn finish_auto(program: &Path, backup: &Path) -> Result<()> {
    let config = installed_config()?;
    let plan: Plan = serde_json::from_slice(&fs::read(backup.join("trial-plan.json"))?)?;
    if plan.program != program || plan.action != "auto-upgrade" {
        return Err("auto-update target mismatch".into());
    }
    let endpoint = Endpoint::current("vaultsyncd-agent")?;
    let _process = endpoint
        .maintenance_process(&program.join("vaultsyncd-agent.exe"))
        .await?;
    let status = rpc(&endpoint, CommandName::GetStatus, None, 0).await?;
    if status.agent_version != env!("CARGO_PKG_VERSION") {
        return Err("updated agent version mismatch".into());
    }
    if !plan.zip {
        crate::auto_update::registry_version(env!("CARGO_PKG_VERSION"))?;
    }
    if !backup.join("complete.json").try_exists()? {
        storage::json(
            &backup.join("complete.json"),
            &serde_json::json!({"action":"auto-upgrade","health":"verified","user_data":"preserved"}),
        )?;
    }
    // After this durable marker there is no database rollback: resumed work can make new commits.
    for (id, paused) in &plan.paused {
        if !paused {
            rpc(
                &endpoint,
                CommandName::ResumeBinding,
                binding_parameters(id),
                4,
            )
            .await?;
        }
    }
    let pending = profile(&config)?.join("auto-update.json");
    if pending.try_exists()? {
        fs::remove_file(pending)?;
    }
    Ok(())
}

/// Roll back a trial only after its authenticated agent drains and releases the writer lock.
pub(crate) async fn recover_auto(program: &Path, backup: &Path) -> Result<()> {
    if backup.join("complete.json").try_exists()? {
        return Err(
            "auto-update committed; inspect paused bindings instead of rolling back".into(),
        );
    }
    let config = installed_config()?;
    let plan: Plan = serde_json::from_slice(&fs::read(backup.join("trial-plan.json"))?)?;
    if plan.program != program || plan.action != "auto-upgrade" {
        return Err("auto-update target mismatch".into());
    }
    let endpoint = Endpoint::current("vaultsyncd-agent")?;
    match endpoint.connect().await {
        Ok(_) => {
            let process = endpoint
                .maintenance_process(&program.join("vaultsyncd-agent.exe"))
                .await?;
            let recovery = backup.join("trial-recovery");
            fs::create_dir(&recovery)?;
            rpc(
                &endpoint,
                CommandName::PrepareMaintenance,
                Some(CommandParameters {
                    destination: Some(recovery.to_string_lossy().into_owned()),
                    ..Default::default()
                }),
                5,
            )
            .await?;
            if !process.wait(60_000)? {
                return Err("trial agent did not drain; rollback refused".into());
            }
        }
        Err(e) if matches!(e.raw_os_error(), Some(2) | Some(3)) => {}
        Err(e) => return Err(e.into()),
    }
    let _guard = InstanceGuard::acquire(&config.state_dir, "vaultsyncd-agent")?;
    if !marker(&config)?.try_exists()? {
        storage::json(&marker(&config)?, &plan)?;
    }
    rollback(&config, &plan).await?;
    let pending = profile(&config)?.join("auto-update.json");
    if pending.try_exists()? {
        fs::remove_file(pending)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn legacy_zip_rollback_ignores_missing_or_corrupt_owner_backups() {
        let base = std::env::temp_dir().join(format!(
            "s185-legacy-rollback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&base).unwrap();
        let config_path = crate::user_session::prepare(&base).unwrap();
        let config = AgentConfig::load(&config_path).unwrap();
        let owner = base.join("vaultsyncd/zip-owner.json");
        fs::write(&owner, b"legacy owner must remain untouched").unwrap();
        let backup = base.join("backup");
        fs::create_dir(&backup).unwrap();
        fs::write(
            config.state_dir.join("unconfirmed.bin"),
            b"retained unconfirmed content",
        )
        .unwrap();
        storage::backup_state(&config.state_dir, &backup.join("state")).unwrap();
        let plan = Plan {
            program: base.join("arbitrary program directory"),
            backup: backup.clone(),
            action: "auto-upgrade".into(),
            paused: vec![],
            zip: true,
        };
        for legacy_backup in [None, Some(b"corrupt historical owner bytes".as_slice())] {
            if let Some(bytes) = legacy_backup {
                fs::write(backup.join("zip-owner.json"), bytes).unwrap();
            }
            fs::write(config.state_dir.join("unconfirmed.bin"), b"trial state").unwrap();
            storage::json(&marker(&config).unwrap(), &plan).unwrap();
            rollback(&config, &plan).await.unwrap();
            assert_eq!(
                fs::read(config.state_dir.join("unconfirmed.bin")).unwrap(),
                b"retained unconfirmed content"
            );
            assert_eq!(
                fs::read(&owner).unwrap(),
                b"legacy owner must remain untouched"
            );
            assert!(!marker(&config).unwrap().exists());
        }
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn trial_gate_requires_a_complete_valid_health_receipt() {
        let base = std::env::temp_dir().join(format!(
            "s162-trial-gate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state = base.join("state");
        let backup = base.join("backup");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir(&backup).unwrap();
        assert!(!trial_pending(&state).unwrap());
        let pending = base.join("auto-update.json");
        fs::write(&pending, b"partial").unwrap();
        assert!(trial_pending(&state).is_err());
        let plan = Plan {
            program: base.join("program"),
            backup: backup.clone(),
            action: "auto-upgrade".into(),
            paused: vec![],
            zip: false,
        };
        fs::write(&pending, serde_json::to_vec(&plan).unwrap()).unwrap();
        assert!(trial_pending(&state).unwrap());
        fs::write(backup.join("complete.json"), b"{\"action\":").unwrap();
        assert!(trial_pending(&state).is_err());
        fs::write(
            backup.join("complete.json"),
            br#"{"action":"auto-upgrade","health":"unknown"}"#,
        )
        .unwrap();
        assert!(trial_pending(&state).is_err());
        fs::write(
            backup.join("complete.json"),
            br#"{"action":"auto-upgrade","health":"verified"}"#,
        )
        .unwrap();
        assert!(!trial_pending(&state).unwrap());
        fs::remove_dir_all(base).unwrap();
    }
}
