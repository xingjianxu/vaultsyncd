//! Offline durable inventories and rollback; never copy an online SQLite state.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
use vaultsyncd_transfer::hash::{ContentHasher, to_hex};
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Item {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}
pub(super) const PROGRAM_FILES: &[&str] = &[
    "vaultsyncd-agent.exe",
    "vaultsyncd-session.exe",
    "vaultsyncd-cli.exe",
    "vaultsyncd-desktop.exe",
    "installed-files.json",
    "UNSIGNED-TEST.txt",
];
pub(super) fn json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let mut f = OpenOptions::new().write(true).create_new(true).open(path)?;
    f.write_all(&serde_json::to_vec_pretty(value).map_err(io::Error::other)?)?;
    f.sync_all()
}
pub(super) fn hash(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut h = ContentHasher::new();
    let mut b = [0; 65536];
    loop {
        let n = file.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(to_hex(&h.finish()))
}
fn safe(path: &Path) -> io::Result<fs::Metadata> {
    let m = fs::symlink_metadata(path)?;
    if m.file_attributes() & 0x400 != 0 {
        return Err(io::Error::other("maintenance refuses reparses"));
    }
    Ok(m)
}
fn copy_new(from: &Path, to: &Path) -> io::Result<()> {
    safe(from)?;
    let mut r = File::open(from)?;
    let mut w = OpenOptions::new().write(true).create_new(true).open(to)?;
    io::copy(&mut r, &mut w)?;
    w.sync_all()
}
fn walk(from: &Path, to: &Path, relative: &str, out: &mut Vec<Item>) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if relative.is_empty() && name == "agent.writer.lock" {
            continue;
        }
        let rel = if relative.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{relative}/{}", name.to_string_lossy())
        };
        let meta = safe(&entry.path())?;
        let target = to.join(&name);
        if meta.is_dir() {
            fs::create_dir(&target)?;
            walk(&entry.path(), &target, &rel, out)?;
        } else if meta.is_file() {
            copy_new(&entry.path(), &target)?;
            out.push(Item {
                path: rel,
                bytes: meta.len(),
                sha256: hash(&target)?,
            });
        } else {
            return Err(io::Error::other("unsupported state object"));
        }
    }
    Ok(())
}
pub(super) fn backup_state(state: &Path, directory: &Path) -> io::Result<()> {
    fs::create_dir(directory)?;
    let mut items = vec![];
    walk(state, directory, "", &mut items)?;
    json(&directory.with_extension("inventory.json"), &items)
}
pub(super) fn backup_program(program: &Path, directory: &Path) -> io::Result<()> {
    fs::create_dir(directory)?;
    let mut items = vec![];
    for name in PROGRAM_FILES.iter().copied().chain(["uninstall.exe"]) {
        let from = program.join(name);
        if !from.try_exists()? {
            continue;
        }
        copy_new(&from, &directory.join(name))?;
        items.push(Item {
            path: name.into(),
            bytes: fs::metadata(&from)?.len(),
            sha256: hash(&from)?,
        });
    }
    json(&directory.with_extension("inventory.json"), &items)
}
pub(super) fn restore(directory: &Path, target: &Path) -> io::Result<()> {
    let items: Vec<Item> =
        serde_json::from_slice(&fs::read(directory.with_extension("inventory.json"))?)
            .map_err(io::Error::other)?;
    for item in &items {
        if item
            .path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
            || item.path.contains('\\')
            || item.path.contains(':')
        {
            return Err(io::Error::other("invalid rollback inventory"));
        }
        let from = directory.join(&item.path);
        if hash(&from)? != item.sha256 || fs::metadata(&from)?.len() != item.bytes {
            return Err(io::Error::other("rollback hash mismatch"));
        }
    }
    for item in items {
        let from = directory.join(&item.path);
        let to = target.join(&item.path);
        if to.try_exists()? {
            safe(&to)?;
            if hash(&to)? == item.sha256 {
                continue;
            }
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, &to)?;
        OpenOptions::new().write(true).open(&to)?.sync_all()?;
    }
    Ok(())
}
pub(super) fn verify_program(directory: &Path) -> io::Result<serde_json::Value> {
    vaultsyncd_platform_windows::files::RootHandle::open(directory)?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("installed-files.json"))?)
            .map_err(io::Error::other)?;
    let files = manifest["files"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing executable provenance"))?;
    let names = &PROGRAM_FILES[..4];
    if files.len() != names.len() {
        return Err(io::Error::other("unexpected executable inventory"));
    }
    for name in names {
        let matches: Vec<_> = files
            .iter()
            .filter(|f| f["path"].as_str() == Some(name))
            .collect();
        if matches.len() != 1 {
            return Err(io::Error::other("unexpected executable name"));
        }
        let entry = matches[0];
        let path = directory.join(name);
        safe(&path)?;
        if hash(&path)? != entry["sha256"].as_str().unwrap_or("")
            || fs::metadata(path)?.len() != entry["bytes"].as_u64().unwrap_or(u64::MAX)
        {
            return Err(io::Error::other("executable provenance mismatch"));
        }
    }
    if manifest["signature_status"] != "unsigned-test" {
        return Err(io::Error::other("unsupported signing/provenance profile"));
    }
    Ok(manifest)
}
pub(super) fn replace_program(candidate: &Path, target: &Path) -> io::Result<()> {
    verify_program(candidate)?;
    for name in PROGRAM_FILES {
        let to = target.join(name);
        if to.try_exists()? {
            safe(&to)?;
        }
        fs::copy(candidate.join(name), &to)?;
        OpenOptions::new().write(true).open(to)?.sync_all()?;
    }
    verify_program(target)?;
    Ok(())
}

pub(super) fn require_program_idle(program: &Path) -> io::Result<()> {
    let mut leases = Vec::new();
    for name in &PROGRAM_FILES[..4] {
        leases.push(
            OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0)
                .open(program.join(name))?,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrupted_backup_refuses_restore_before_changing_any_destination() {
        let base = std::env::temp_dir().join(format!(
            "s100-rollback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&base).unwrap();
        let state = base.join("state");
        let backup = base.join("backup");
        fs::create_dir(&state).unwrap();
        fs::write(state.join("first"), b"old first unconfirmed").unwrap();
        fs::write(state.join("second"), b"old second unconfirmed").unwrap();
        backup_state(&state, &backup).unwrap();
        fs::write(state.join("first"), b"new first content").unwrap();
        fs::write(state.join("second"), b"new second content").unwrap();
        fs::write(backup.join("second"), b"corrupted rollback bytes").unwrap();
        assert!(restore(&backup, &state).is_err());
        assert_eq!(fs::read(state.join("first")).unwrap(), b"new first content");
        assert_eq!(
            fs::read(state.join("second")).unwrap(),
            b"new second content"
        );
        fs::remove_dir_all(base).unwrap();
    }
}
