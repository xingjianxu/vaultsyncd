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
use vaultsyncd_platform_windows::{files::RootHandle, ipc::Endpoint};
use vaultsyncd_transfer::ports::PortError;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const LEGACY_SOURCE: &str = "cf1d174c3bd47b4babc2db053514619b2e733b07";
#[derive(Serialize, Deserialize)]
struct Plan {
    program: PathBuf,
    backup: PathBuf,
    action: String,
    paused: Vec<(String, bool)>,
}
fn profile(config: &AgentConfig) -> Result<&Path> {
    Ok(config.state_dir.parent().ok_or("missing agent profile")?)
}
fn marker(config: &AgentConfig) -> Result<PathBuf> {
    Ok(profile(config)?.join("maintenance.json"))
}
/// A barrier is never silently removed by normal startup, including the login launcher.
pub fn admit_startup(config: &AgentConfig) -> std::result::Result<(), PortError> {
    crate::portable::admit(config).map_err(error)?;
    if config
        .state_dir
        .parent()
        .ok_or_else(|| error("profile absent"))?
        .join("maintenance.json")
        .try_exists()
        .map_err(error)?
    {
        return Err(PortError::rejected(
            "MAINTENANCE_REQUIRED",
            "maintenance barrier exists; recover using the retained maintenance candidate",
        ));
    }
    Ok(())
}
async fn rpc(
    endpoint: &Endpoint,
    name: CommandName,
    parameters: Option<CommandParameters>,
    minor: u32,
) -> Result<GetStatusResult> {
    let response = endpoint
        .request(&Request {
            request_id: format!("maintenance-{}", std::process::id()),
            ipc_version: IpcVersion { major: 1, minor },
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
pub async fn run(action: &str, program: &Path) -> Result<()> {
    if !matches!(
        action,
        "upgrade" | "uninstall-program" | "uninstall-provider" | "recover"
    ) {
        return Err("unknown maintenance action".into());
    }
    let mut config = installed_config()?;
    if PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA absent")?)
        .join("vaultsyncd/zip-owner.json")
        .try_exists()?
    {
        return Err("ZIP_PROFILE: installed maintenance cannot adopt ZIP state; keep original ZIP and use its safe Stop entry. ZIP upgrades are not supported by this release".into());
    }
    let program_root = RootHandle::open(program)?;
    let state = RootHandle::open(&config.state_dir)?;
    if program_root.overlaps(&state) {
        return Err("program/state overlap".into());
    }
    let barrier = marker(&config)?;
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
            return Ok(());
        }
        let _guard = InstanceGuard::acquire(&config.state_dir, "vaultsyncd-agent")?;
        return rollback(&config, &plan).await;
    }
    if barrier.try_exists()? {
        return Err("MAINTENANCE_REQUIRED: recover the retained transaction before retry".into());
    }
    let installed = storage::verify_program(program)?;
    let executable = std::env::current_exe()?;
    let candidate = executable.parent().ok_or("candidate directory absent")?;
    if action == "upgrade" {
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
        if program_root.overlaps(&RootHandle::open(candidate)?) {
            return Err("upgrade candidate must be separate from installed program".into());
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
    RootHandle::open(&maintenance)?;
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
            crate::management::load_connection(&db,&mut config).await?;crate::management::load_bindings(&db,&mut config).await?;crate::management::admit(&config)?;
            if plan.paused.is_empty(){plan.paused=crate::management::binding_list(&db).await?.iter().map(|b|(b.binding_id.clone(),b.paused)).collect();
                for binding in &config.bindings{let id=BindingId::from_hex(&binding.id)?;let local=Arc::new(crate::local::Local::open(&binding.path,&binding.cache,db.clone(),id).await?);crate::management_export::export(local,&config,plan.backup.join("recovery").join(&binding.id).to_str().ok_or("invalid recovery destination")?).await?;}}
            if action.starts_with("uninstall-"){storage::require_program_idle(program)?;}
            if action=="uninstall-provider"{
                // Preflight every Root before converting any file. Never unregister partial placeholders.
                for binding in &config.bindings{vaultsyncd_platform_windows::cfapi::maintenance::check_materialized(&binding.path)?;}
                let mut reports=vec![];
                for binding in &config.bindings{
                    let hashes=vaultsyncd_platform_windows::cfapi::maintenance::revert_materialized(&binding.path)?;
                    vaultsyncd_platform_windows::cfapi::maintenance::unregister_materialized(&binding.path)?;
                    let id=BindingId::from_hex(&binding.id)?;if vaultsyncd_platform_windows::shell::registered(id)? { vaultsyncd_platform_windows::shell::unregister(id)?; }
                    BindingControls::new(db.clone()).unbind_keep_data(id).await?;
                    reports.push(serde_json::json!({"binding_id":binding.id,"preserved_hashes":hashes,"only_complete_files":true}));
                }storage::json(&plan.backup.join("provider-removal.json"),&reports)?;
            }else{restore_paused(&db,&plan).await?;}
            Ok::<(),Box<dyn std::error::Error>>(())
        }.await;
        db.pool().close().await;work?;
        if action=="upgrade"{storage::replace_program(candidate,program)?;}
        storage::json(&plan.backup.join("complete.json"),&serde_json::json!({"action":action,"program":program,"user_data":"preserved","cloud_only":if action=="uninstall-program"{"placeholders retained; unavailable until provider reinstall; not complete files"}else{"no placeholder deletion"},"recovery":plan.backup.join("recovery")}))?;
        fs::remove_file(&barrier)?;Ok::<(),Box<dyn std::error::Error>>(())
    }.await;
    if let Err(failure) = result {
        match rollback(&config,&plan).await{Ok(())=>return Err(format!("maintenance failed; full offline state/program restored, user data retained: {failure}").into()),Err(recovery)=>return Err(format!("MAINTENANCE_REQUIRED: {failure}; rollback also failed: {recovery}; retained candidate={} backup={}",candidate.display(),plan.backup.display()).into())}
    }
    println!(
        "maintenance=complete action={action} evidence={}",
        plan.backup.display()
    );
    Ok(())
}
