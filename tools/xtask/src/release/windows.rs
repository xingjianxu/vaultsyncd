//! Host NSIS + ZIP adapter. Both share one fresh same-commit Windows cross-build.
use super::*;
const TARGET: &str = "x86_64-pc-windows-msvc";
const FLAGS: &str = "-C target-feature=+crt-static -C link-arg=/ignore:4099";
fn run(command: &[String], root: &Path, build: Option<&Path>) -> Result<(), String> {
    let mut process = Command::new(&command[0]);
    process.args(&command[1..]).current_dir(root);
    if let Some(build) = build {
        process
            .env("CARGO_TARGET_DIR", build)
            .env("RUSTFLAGS", FLAGS)
            .env_remove("CARGO_ENCODED_RUSTFLAGS");
    }
    if !process.status().map_err(|e| e.to_string())?.success() {
        return Err(format!("Windows build step failed: {}", command[0]));
    }
    Ok(())
}
fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).into()).collect()
}
fn production_pe(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    let offset = bytes
        .get(0x3c..0x40)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or("invalid PE header")? as usize;
    if bytes.get(..2) != Some(b"MZ")
        || bytes.get(offset..offset + 4) != Some(b"PE\0\0")
        || bytes.get(offset + 4..offset + 6) != Some(&[0x64, 0x86])
    {
        return Err("Windows x64 executable required".into());
    }
    for token in [
        "acceptance_report",
        "acceptance_file_control",
        "acceptance_fixture_phase",
        "acceptance_toast_history",
        "VAULTSYNCD_S080_REPORT",
        "VAULTSYNCD_S081_FIXTURE",
        "VAULTSYNCD_S082_FIXTURE",
        "VAULTSYNCD_S083_FIXTURE",
    ] {
        let wide: Vec<u8> = token.encode_utf16().flat_map(u16::to_le_bytes).collect();
        if bytes.windows(token.len()).any(|b| b == token.as_bytes())
            || bytes.windows(wide.len()).any(|b| b == wide)
        {
            return Err(format!(
                "test acceptance entry refused in {}",
                path.display()
            ));
        }
    }
    Ok(())
}
pub(super) fn build(root: &Path, directory: &Path, component: &str) -> Result<(), String> {
    let installer = component == "windows-client";
    if std::env::var("VAULTSYNCD_WINDOWS_SHELL").as_deref() != Ok("1") {
        return Err(
            "Windows packaging requires nix develop .#windows and the locked cross toolchain"
                .into(),
        );
    }
    let before = source(root)?;
    if directory.exists() {
        return Err("output exists; use a new artifact directory".into());
    }
    for base in [root.to_path_buf(), root.join("apps/desktop")] {
        if fs::read_dir(base)
            .map_err(|e| e.to_string())?
            .any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().starts_with(".env")))
        {
            return Err("ignored .env input refused for Windows release build".into());
        }
    }
    if std::env::vars().any(|(key, _)| key.starts_with("VITE_")) {
        return Err("unrecorded VITE_ environment override refused".into());
    }
    let mut toolchain = BTreeMap::new();
    for (tool, args) in [
        ("rustc", vec!["-vV"]),
        ("cargo", vec!["--version"]),
        ("node", vec!["--version"]),
        ("pnpm", vec!["--version"]),
        ("python3", vec!["--version"]),
    ] {
        toolchain.insert(tool.into(), output(tool, &args, root)?);
    }
    if installer {
        toolchain.insert("makensis".into(), output("makensis", &["-VERSION"], root)?);
    }
    toolchain.insert(
        "cargo-xwin".into(),
        output("cargo", &["xwin", "--version"], root)?,
    );
    toolchain.insert("rustflags".into(), FLAGS.into());
    fs::create_dir_all(directory.join("payload")).map_err(|e| e.to_string())?;
    let directory = fs::canonicalize(directory).map_err(|e| e.to_string())?;
    let payload = directory.join("payload");
    let build = directory.join("build");
    let mut preparation = vec![];
    for command in [
        strings(&["pnpm", "install", "--frozen-lockfile", "--offline"]),
        strings(&["pnpm", "--filter", "@vaultsyncd/desktop", "build"]),
    ] {
        if command[1] == "--filter" {
            let dist = root.join("apps/desktop/dist");
            if dist.exists() {
                fs::remove_dir_all(dist).map_err(|e| e.to_string())?;
            }
        }
        run(&command, root, None)?;
        preparation.push(command);
    }
    let compile = strings(&[
        "cargo",
        "xwin",
        "build",
        "--locked",
        "--release",
        "--target",
        TARGET,
        "-p",
        "vaultsyncd-agent",
        "-p",
        "vaultsyncd-cli",
        "-p",
        "vaultsyncd-desktop",
        "--bins",
    ]);
    run(&compile, root, Some(&build))?;
    let mut recorded = strings(&["env", "-u", "CARGO_ENCODED_RUSTFLAGS"]);
    recorded.push(format!("CARGO_TARGET_DIR={}", build.display()));
    recorded.push(format!("RUSTFLAGS={FLAGS}"));
    recorded.extend(compile);
    preparation.push(recorded);
    for binary in [
        "vaultsyncd-agent.exe",
        "vaultsyncd-session.exe",
        "vaultsyncd-cli.exe",
        "vaultsyncd-desktop.exe",
    ] {
        let from = build.join(TARGET).join("release").join(binary);
        production_pe(&from)?;
        fs::copy(from, payload.join(binary)).map_err(|e| e.to_string())?;
    }
    let mut executables = vec![];
    inventory(&payload, Path::new(""), &mut executables)?;
    json(
        &payload.join("installed-files.json"),
        &serde_json::json!({"format":1,"source":before,"signature_status":"unsigned-test","files":executables,"startup":{"scope":"current-user","mechanism":"HKCU Run","launcher":"vaultsyncd-session.exe"},"state":"%LOCALAPPDATA%/vaultsyncd/agent"}),
    )?;
    fs::write(payload.join("UNSIGNED-TEST.txt"),"Unsigned test build. No production signing identity was provided. Controlled upgrade preserves recovery/state backups; default uninstall removes programs only. Complete files, dirty content and state are retained; cloud-only placeholders are not complete offline files.\n").map_err(|e|e.to_string())?;
    let command = vec![
        "makensis".into(),
        "-NOCONFIG".into(),
        "-V3".into(),
        "-NOCD".into(),
        format!("-DPRODUCT_VERSION={}", before.version),
        format!("-DPAYLOAD={}", payload.display()),
        format!(
            "-DOUTPUT={}",
            payload
                .join(format!(
                    "vaultsyncd-{}-x64-unsigned-setup.exe",
                    before.version
                ))
                .display()
        ),
        root.join("packaging/windows/installer/vaultsyncd.nsi")
            .display()
            .to_string(),
    ];
    if installer {
        run(&command, root, None)?;
        preparation.push(command.clone());
    }
    let launcher = build.join(TARGET).join("release/vaultsyncd-zip.exe");
    production_pe(&launcher)?;
    let zip_command = vec![
        "python3".into(),
        root.join("packaging/windows/zip/build.py")
            .display()
            .to_string(),
        payload.display().to_string(),
        launcher.display().to_string(),
        root.join("packaging/windows/zip").display().to_string(),
        payload
            .join(format!("vaultsyncd-{}-x64-unsigned.zip", before.version))
            .display()
            .to_string(),
    ];
    run(&zip_command, root, None)?;
    preparation.push(zip_command.clone());
    if before != source(root)? {
        return Err("source changed during Windows build; receipt refused".into());
    }
    fs::remove_dir_all(build).map_err(|e| e.to_string())?;
    let mut files = vec![];
    inventory(&payload, Path::new(""), &mut files)?;
    json(
        &directory.join("receipt.json"),
        &Receipt {
            format: 1,
            component: component.into(),
            target: TARGET.into(),
            source: before,
            toolchain,
            command: if installer { command } else { zip_command },
            preparation,
            files,
        },
    )?;
    println!(
        "built same-source unsigned Windows receipt {}",
        directory.join("receipt.json").display()
    );
    Ok(())
}
