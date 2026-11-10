//! Windows user-session agent process. No GUI process is required.
#[cfg(windows)]
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        if args.len() == 4
            && matches!(args[1].as_str(), "--auto-update" | "--auto-update-recover")
            && args[2] == "--program-dir"
        {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(vaultsyncd_agent::auto_update::run(
                std::path::Path::new(&args[3]),
                args[1] == "--auto-update-recover",
            ))?;
            return Ok(());
        }
        if args.len() == 5 && args[1] == "--maintenance" && args[3] == "--program-dir" {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(vaultsyncd_agent::maintenance::run(
                &args[2],
                std::path::Path::new(&args[4]),
            ))?;
            return Ok(());
        }
        if args.len() == 2 && args[1] == "--ensure-user-session" {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(vaultsyncd_agent::portable::ensure_user_session())?;
            return Ok(());
        }
        if args.len() == 3 && args[1] == "--user-session" {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(vaultsyncd_agent::portable::run(&args[2]))?;
            return Ok(());
        }
        if args.len() == 2 && args[1] == "--user-session" {
            vaultsyncd_agent::user_session::launch()?;
            return Ok(());
        }
        let update_trial = args.len() == 4 && args[3] == "--update-trial";
        if args.len() < 3
            || args[1] != "--config"
            || args[3..]
                .iter()
                .any(|a| a != "--login-stdin" && a != "--once" && !update_trial)
        {
            return Err(
                "usage: vaultsyncd-agent --user-session start|stop|health, or --config <file.json> [--login-stdin|--once]".into(),
            );
        }
        let config = vaultsyncd_agent::config::AgentConfig::load(std::path::Path::new(&args[2]))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            if update_trial {
                vaultsyncd_agent::runtime::run_update_trial(config).await
            } else {
                vaultsyncd_agent::runtime::run(
                    config,
                    args.iter().any(|a| a == "--login-stdin"),
                    args.iter().any(|a| a == "--once"),
                )
                .await
            }
        })?;
        Ok(())
    })();
    if let Err(error) = result {
        let code = error
            .downcast_ref::<vaultsyncd_transfer::ports::PortError>()
            .and_then(|e| e.code())
            .unwrap_or("STARTUP_FAILED");
        eprintln!(
            "event=agent-failed code={code} detail={:?}",
            error.to_string().chars().take(512).collect::<String>()
        );
        std::process::exit(if code == "ALREADY_RUNNING" { 2 } else { 1 });
    }
}
#[cfg(not(windows))]
fn main() {
    eprintln!("vaultsyncd-agent requires a Windows user session");
    std::process::exit(3);
}
