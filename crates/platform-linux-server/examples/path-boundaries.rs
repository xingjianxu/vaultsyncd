//! Actual target-kernel directory/file/no-follow/openat2/fallback acceptance.
use std::{fs, io::Read, os::unix::fs::symlink, path::Path};
use vaultsyncd_platform_linux_server::files::{
    ResolveMode, VaultHandle,
    restricted::{ResolveMechanism, RestrictedDir},
};
use vaultsyncd_platform_linux_server::mounts::probe::{self, ScanCompleteness, ScanPolicy};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::args_os()
        .nth(1)
        .expect("dedicated empty fixture directory");
    let base = Path::new(&base).canonicalize()?;
    assert!(
        base.starts_with("/data")
            && base
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("s098-path-boundaries-")
    );
    assert_eq!(fs::read_dir(&base)?.count(), 0);
    let vault = base.join("vault");
    fs::create_dir(&vault)?;
    let outside = base.join("outside");
    fs::create_dir(&outside)?;
    fs::write(vault.join("keep.txt"), b"in-vault bytes")?;
    fs::create_dir(vault.join("dir"))?;
    fs::write(outside.join("canary"), b"outside must remain")?;
    fs::write(base.join("regular-file"), b"not a directory")?;
    symlink(&vault, base.join("vault-link"))?;
    symlink(vault.join("keep.txt"), vault.join("file-link"))?;
    symlink(&outside, vault.join("dir-link"))?;
    for (mode, mechanism) in [
        (ResolveMode::Openat2, ResolveMechanism::Openat2),
        (ResolveMode::ComponentWalk, ResolveMechanism::ComponentWalk),
    ] {
        assert!(RestrictedDir::open(&base.join("vault-link"), mode).is_err());
        assert!(RestrictedDir::open(&base.join("regular-file"), mode).is_err());
        let handle = VaultHandle::open(&vault, mode)?;
        assert_eq!(handle.mechanism(), mechanism);
        let mut opened = handle.open_read(Path::new("keep.txt"))?;
        let mut bytes = Vec::new();
        opened.file.read_to_end(&mut bytes)?;
        assert_eq!(bytes, b"in-vault bytes");
        assert!(handle.open_read(Path::new("file-link")).is_err());
        assert!(handle.open_read(Path::new("dir-link/canary")).is_err());
        assert!(handle.open_dir(Path::new("dir-link")).is_err());
        assert!(handle.open_read(Path::new("../outside/canary")).is_err());
        assert!(handle.open_dir(Path::new("dir")).is_ok());
        let first = handle.vault_dir().list_entries()?;
        assert_eq!(first.len(), 4);
        assert_eq!(handle.vault_dir().list_entries()?, first);
        handle.verify()?;
    }
    assert_eq!(fs::read(outside.join("canary"))?, b"outside must remain");
    let nested = vault.join("nested-subvolume");
    assert!(
        std::process::Command::new("btrfs")
            .args(["subvolume", "create"])
            .arg(&nested)
            .status()?
            .success()
    );
    let parent = probe::probe_path(&vault)?.btrfs.unwrap();
    let child = probe::probe_path(&nested)?.btrfs.unwrap();
    assert_eq!(parent.fsid, child.fsid);
    let parent_id = parent.subvolume_id.unwrap();
    let child_id = child
        .subvolume_id
        .unwrap_or_else(|| panic!("subvolume facts: {child:?}"));
    assert!(child_id > 0 && child_id != parent_id);
    let scan = probe::scan_vault(
        &vault,
        parent_id,
        &ScanPolicy {
            max_entries: 100,
            max_depth: 8,
        },
    )?;
    assert_eq!(scan.completeness, ScanCompleteness::Complete);
    assert_eq!(scan.subvolumes.len(), 1);
    assert_eq!(scan.subvolumes[0].subvolume_id, child_id);
    eprintln!(
        "real-subvolume-id={child_id};source={}",
        child.subvolume_id_source.unwrap().as_str()
    );
    let result = r#"{"passed":true,"checks":["real-btrfs-vault","directory-required","vault-symlink-rejected","leaf-and-component-symlinks-rejected","openat2-forced-success","component-walk-forced-success","repeated-pinned-directory-enumeration-complete","outside-canary-preserved"]}"#;
    println!("{result}");
    Ok(())
}
