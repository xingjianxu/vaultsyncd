//! Product version mirrors and auditable, same-commit artifact sets (S093).
use crate::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, capture, workspace_root};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MIRRORS: &[&str] = &[
    "package.json",
    "packages/ui/package.json",
    "packages/api-client/package.json",
    "packages/local-ipc-types/package.json",
    "apps/web/package.json",
    "apps/desktop/package.json",
    "apps/desktop/src-tauri/tauri.conf.json",
];
const LOCKS: &[&str] = &[
    "Cargo.lock",
    "pnpm-lock.yaml",
    "flake.lock",
    "rust-toolchain.toml",
    "api/openapi.yaml",
    "api/local-ipc.schema.json",
    "api/generation.toml",
];
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    commit: String,
    tree: String,
    version: String,
    inputs: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    path: String,
    bytes: u64,
    sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    format: u32,
    component: String,
    target: String,
    source: Source,
    toolchain: BTreeMap<String, String>,
    command: Vec<String>,
    files: Vec<File>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Component {
    directory: PathBuf,
    receipt: Receipt,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    source: Source,
    components: Vec<Component>,
}
fn read(path: &Path) -> Result<serde_json::Value, String> {
    serde_json::from_slice(&fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?)
        .map_err(|e| e.to_string())
}
fn json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    fs::write(path, bytes).map_err(|e| e.to_string())
}
fn output(program: &str, args: &[&str], root: &Path) -> Result<String, String> {
    let result = capture(program, args, root)?;
    if !result.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}
pub(crate) fn version(root: &Path) -> Result<String, String> {
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("Cargo.toml")).map_err(|e| e.to_string())?)
            .map_err(|e| format!("Cargo.toml: {e}"))?;
    let value = manifest["workspace"]["package"]["version"]
        .as_str()
        .ok_or("missing workspace product version")?;
    if value.split('.').count() != 3
        || !value.split('.').all(|v| {
            !v.is_empty()
                && v.bytes().all(|b| b.is_ascii_digit())
                && (v.len() == 1 || !v.starts_with('0'))
        })
    {
        return Err("product version must be canonical major.minor.patch".into());
    }
    Ok(value.to_owned())
}
pub(crate) fn verify_version(root: &Path) -> Result<(), String> {
    let expected = version(root)?;
    for mirror in MIRRORS {
        if read(&root.join(mirror))?["version"].as_str() != Some(expected.as_str()) {
            return Err(format!(
                "产品版本不同步: {mirror}; run cargo xtask version --sync"
            ));
        }
    }
    Ok(())
}
pub(crate) fn run_version(args: &[String]) -> u8 {
    let sync = match args {
        [] => false,
        [arg] if arg == "--check" => false,
        [arg] if arg == "--sync" => true,
        _ => return EXIT_USAGE,
    };
    let result = (|| {
        let root = workspace_root()?;
        let value = version(&root)?;
        if sync {
            for mirror in MIRRORS {
                let path = root.join(mirror);
                let mut manifest = read(&path)?;
                if manifest["version"].as_str() != Some(value.as_str()) {
                    manifest["version"] = value.clone().into();
                    json(&path, &manifest)?;
                }
            }
        }
        verify_version(&root)?;
        println!("product version {value}; protocol/schema/generator versions independent");
        Ok::<_, String>(())
    })();
    finish(result)
}
fn hash(path: &Path) -> Result<String, String> {
    let result = Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&result.stdout);
    let value = text
        .split_whitespace()
        .next()
        .ok_or("sha256sum returned no hash")?;
    if !result.status.success()
        || value.len() != 64
        || !value.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(format!("cannot hash {}", path.display()));
    }
    Ok(value.to_owned())
}
fn source(root: &Path) -> Result<Source, String> {
    verify_version(root)?;
    let dirty = output(
        "git",
        &["status", "--porcelain", "--untracked-files=all"],
        root,
    )?;
    if !dirty.is_empty() {
        return Err(
            "release requires a clean committed source tree (including untracked inputs)".into(),
        );
    }
    let inputs = LOCKS
        .iter()
        .map(|p| Ok(((*p).to_owned(), hash(&root.join(p))?)))
        .collect::<Result<_, String>>()?;
    Ok(Source {
        commit: output("git", &["rev-parse", "HEAD"], root)?,
        tree: output("git", &["rev-parse", "HEAD^{tree}"], root)?,
        version: version(root)?,
        inputs,
    })
}
fn inventory(directory: &Path, relative: &Path, files: &mut Vec<File>) -> Result<(), String> {
    let mut entries = fs::read_dir(directory.join(relative))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = relative.join(entry.file_name());
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            return Err("release payload must not contain symlinks".into());
        }
        if kind.is_dir() {
            inventory(directory, &path, files)?;
        } else if kind.is_file() {
            let name = path
                .to_str()
                .ok_or("non-UTF8 payload path")?
                .replace('\\', "/");
            files.push(File {
                path: name,
                bytes: entry.metadata().map_err(|e| e.to_string())?.len(),
                sha256: hash(&entry.path())?,
            });
        } else {
            return Err("unsupported artifact type".into());
        }
    }
    Ok(())
}
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for item in fs::read_dir(from).map_err(|e| e.to_string())? {
        let item = item.map_err(|e| e.to_string())?;
        let dest = to.join(item.file_name());
        let kind = item.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            copy_tree(&item.path(), &dest)?;
        } else if kind.is_file() {
            fs::copy(item.path(), dest).map_err(|e| e.to_string())?;
        } else {
            return Err("symlink/special artifact refused".into());
        }
    }
    Ok(())
}
fn run_build(root: &Path, component: &str, directory: &Path) -> Result<(), String> {
    let before = source(root)?;
    if directory.exists() {
        return Err("output exists; use a new artifact directory".into());
    }
    let frontend = match component {
        "web" => Some("web"),
        "desktop-ui" => Some("desktop"),
        "server" | "admin" => None,
        _ => return Err(
            "unsupported component; native Windows packaging belongs to its platform build adapter"
                .into(),
        ),
    };
    let mut toolchain = BTreeMap::new();
    for (tool, args) in [
        ("rustc", vec!["-vV"]),
        ("cargo", vec!["--version"]),
        ("node", vec!["--version"]),
        ("pnpm", vec!["--version"]),
    ] {
        toolchain.insert(tool.into(), output(tool, &args, root)?);
    }
    fs::create_dir_all(directory.join("payload")).map_err(|e| e.to_string())?;
    let target;
    let command;
    if let Some(app) = frontend {
        for base in [root.to_path_buf(), root.join(format!("apps/{app}"))] {
            if fs::read_dir(base).map_err(|e| e.to_string())?.any(|entry| {
                entry.is_ok_and(|e| e.file_name().to_string_lossy().starts_with(".env"))
            }) {
                return Err("ignored .env input refused for release build".into());
            }
        }
        if std::env::vars().any(|(key, _)| key.starts_with("VITE_")) {
            return Err("unrecorded VITE_ environment override refused".into());
        }
        let dist = root.join(format!("apps/{app}/dist"));
        if dist.exists() {
            fs::remove_dir_all(&dist).map_err(|e| e.to_string())?;
        }
        let package = format!("@vaultsyncd/{app}");
        command = vec![
            "pnpm".into(),
            "--filter".into(),
            package.clone(),
            "build".into(),
        ];
        let status = Command::new("pnpm")
            .args(["--filter", &package, "build"])
            .current_dir(root)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("frontend build failed; no receipt produced".into());
        }
        if !dist.join("index.html").is_file() {
            return Err("frontend build has no index.html".into());
        }
        copy_tree(&dist, &directory.join("payload"))?;
        target = "browser-static".to_owned();
    } else {
        let rust = toolchain.get("rustc").ok_or("missing rustc identity")?;
        target = rust
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .ok_or("rustc has no host triple")?
            .to_owned();
        if !target.ends_with("linux-gnu") {
            return Err("Linux native build adapter requires Linux host".into());
        }
        let package = if component == "server" {
            "vaultsyncd-server"
        } else {
            "vaultsyncd-admin"
        };
        command = vec![
            "cargo".into(),
            "build".into(),
            "--locked".into(),
            "--release".into(),
            "--target".into(),
            target.clone(),
            "-p".into(),
            package.into(),
        ];
        // Fresh target directory: a receipt cannot attest a binary copied from an older build.
        let build = directory.join("build");
        let status = Command::new("cargo")
            .args(&command[1..])
            .env("CARGO_TARGET_DIR", &build)
            .current_dir(root)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("native build failed; no receipt produced".into());
        }
        fs::copy(
            build.join(&target).join("release").join(package),
            directory.join("payload").join(package),
        )
        .map_err(|e| e.to_string())?;
        fs::remove_dir_all(build).map_err(|e| e.to_string())?;
    }
    if before != source(root)? {
        return Err("source changed during build; receipt refused".into());
    }
    let mut files = Vec::new();
    inventory(&directory.join("payload"), Path::new(""), &mut files)?;
    if files.is_empty() {
        return Err("empty payload refused".into());
    }
    json(
        &directory.join("receipt.json"),
        &Receipt {
            format: 1,
            component: component.into(),
            target,
            source: before,
            toolchain,
            command,
            files,
        },
    )?;
    println!("built receipt {}", directory.join("receipt.json").display());
    Ok(())
}
fn validate(directory: &Path, receipt: &Receipt, expected: &Source) -> Result<(), String> {
    if receipt.format != 1 || &receipt.source != expected {
        return Err("artifact commit/version/lock inputs are stale or incompatible".into());
    }
    if receipt.command.is_empty() || receipt.toolchain.is_empty() {
        return Err("missing build provenance".into());
    }
    let mut actual = Vec::new();
    inventory(&directory.join("payload"), Path::new(""), &mut actual)?;
    if actual.is_empty()
        || serde_json::to_vec(&actual).map_err(|e| e.to_string())?
            != serde_json::to_vec(&receipt.files).map_err(|e| e.to_string())?
    {
        return Err("artifact payload hash/size/file-set mismatch".into());
    }
    Ok(())
}
fn finish(result: Result<(), String>) -> u8 {
    match result {
        Ok(()) => EXIT_OK,
        Err(error) => {
            eprintln!("xtask release: {error}");
            EXIT_FAILURE
        }
    }
}
pub(crate) fn run(args: &[String]) -> u8 {
    let result = (|| {
        let root = workspace_root()?;
        match args {
            [mode,component,directory] if mode=="build" => run_build(&root,component,&std::path::absolute(directory).map_err(|e|e.to_string())?),
            [mode,path,receipts @ ..] if mode=="assemble" && !receipts.is_empty()=>{
                let expected=source(&root)?;let path=Path::new(path);if path.exists(){return Err("manifest output exists".into());}
                let mut components=Vec::new();let mut names=std::collections::BTreeSet::new();
                for directory in receipts {let directory=fs::canonicalize(directory).map_err(|e|e.to_string())?;
                    let receipt:Receipt=serde_json::from_value(read(&directory.join("receipt.json"))?).map_err(|e|e.to_string())?;
                    validate(&directory,&receipt,&expected)?;
                    if !names.insert((receipt.component.clone(),receipt.target.clone())){return Err("duplicate component target".into());}
                    components.push(Component {directory,receipt});
                }
                components.sort_by(|a,b|(&a.receipt.component,&a.receipt.target).cmp(&(&b.receipt.component,&b.receipt.target)));
                json(path,&Manifest {format:1,source:expected,components})?;println!("assembled {}",path.display());Ok(())
            },
            [mode,path] if mode=="verify"=>{
                let manifest:Manifest=serde_json::from_value(read(Path::new(path))?).map_err(|e|e.to_string())?;
                let expected=source(&root)?;if manifest.format!=1 || manifest.source!=expected || manifest.components.is_empty(){return Err("stale/empty release manifest".into());}
                for component in manifest.components {validate(&component.directory,&component.receipt,&expected)?;}
                println!("release manifest verified");Ok(())
            },
            _=>Err("usage: release build <web|desktop-ui|server|admin> <new-directory> | release assemble <new-manifest.json> <artifact-directory>... | release verify <manifest.json>".into()),
        }
    })();
    finish(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn payload_mutation_and_source_change_are_both_refused() {
        let root = std::env::temp_dir().join(format!(
            "vs-release-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("payload")).unwrap();
        fs::write(root.join("payload/item"), b"old bytes").unwrap();
        let source = Source {
            commit: "c".into(),
            tree: "t".into(),
            version: "0.1.0".into(),
            inputs: BTreeMap::from([("Cargo.lock".into(), "old-lock".into())]),
        };
        let mut files = Vec::new();
        inventory(&root.join("payload"), Path::new(""), &mut files).unwrap();
        let receipt = Receipt {
            format: 1,
            component: "web".into(),
            target: "browser-static".into(),
            source: source.clone(),
            toolchain: BTreeMap::from([("pnpm".into(), "12".into())]),
            command: vec!["build".into()],
            files,
        };
        validate(&root, &receipt, &source).unwrap();
        let mut newer = source.clone();
        newer.inputs.insert("Cargo.lock".into(), "new-lock".into());
        assert!(
            validate(&root, &receipt, &newer)
                .unwrap_err()
                .contains("stale")
        );
        newer = source.clone();
        newer.commit = "new-commit".into();
        assert!(validate(&root, &receipt, &newer).is_err());
        newer = source.clone();
        newer.version = "0.2.0".into();
        assert!(validate(&root, &receipt, &newer).is_err());
        fs::write(root.join("payload/item"), b"new bytes").unwrap();
        assert!(
            validate(&root, &receipt, &source)
                .unwrap_err()
                .contains("hash")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
