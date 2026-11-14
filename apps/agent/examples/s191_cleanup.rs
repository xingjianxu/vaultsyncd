//! Remove only a stopped S191 lab root after preserving every fully materialized file.
#[cfg(not(windows))]
fn main() {
    eprintln!("S191 cleanup requires the Windows lab");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::{Path, PathBuf};
    use vaultsyncd_domain::ids::BindingId;
    use vaultsyncd_platform_windows::{cfapi::maintenance, shell};

    let binding = BindingId::from_hex(&std::env::args().nth(1).ok_or("binding argument")?)?;
    let path = PathBuf::from(std::env::args_os().nth(2).ok_or("root argument")?);
    let canonical = path.canonicalize()?;
    let base = Path::new("C:/vaultsyncd-lab").canonicalize()?;
    let relative = canonical.strip_prefix(&base)?;
    let mut components = relative.components();
    if !components
        .next()
        .is_some_and(|c| c.as_os_str().to_string_lossy().starts_with("S191-"))
        || !components.next().is_some_and(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some("root-a" | "root-b" | "legacy-root")
            )
        })
        || components.next().is_some()
    {
        return Err("refuse root outside the exact S191 run layout".into());
    }
    maintenance::check_materialized(&path)?;
    let hashes = maintenance::revert_materialized(&path)?;
    maintenance::unregister_materialized(&path)?;
    if shell::registered(binding)? {
        shell::unregister(binding)?;
    }
    println!(
        "{}",
        serde_json::json!({"binding":binding.to_string(),"preserved":hashes})
    );
    Ok(())
}
