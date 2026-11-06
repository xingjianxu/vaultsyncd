//! Actual target-kernel directory/file/no-follow/openat2/fallback acceptance.
use std::{fs, io::Read, os::unix::fs::symlink, path::Path};
use vaultsyncd_platform_linux_server::files::{
    ResolveMode, RootHandle,
    restricted::{ResolveMechanism, RestrictedDir},
};

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
    let root = base.join("root");
    fs::create_dir(&root)?;
    let outside = base.join("outside");
    fs::create_dir(&outside)?;
    fs::write(root.join("keep.txt"), b"in-root bytes")?;
    fs::create_dir(root.join("dir"))?;
    fs::write(outside.join("canary"), b"outside must remain")?;
    fs::write(base.join("regular-file"), b"not a directory")?;
    symlink(&root, base.join("root-link"))?;
    symlink(root.join("keep.txt"), root.join("file-link"))?;
    symlink(&outside, root.join("dir-link"))?;
    for (mode, mechanism) in [
        (ResolveMode::Openat2, ResolveMechanism::Openat2),
        (ResolveMode::ComponentWalk, ResolveMechanism::ComponentWalk),
    ] {
        assert!(RestrictedDir::open(&base.join("root-link"), mode).is_err());
        assert!(RestrictedDir::open(&base.join("regular-file"), mode).is_err());
        let handle = RootHandle::open(&root, mode)?;
        assert_eq!(handle.mechanism(), mechanism);
        let mut opened = handle.open_read(Path::new("keep.txt"))?;
        let mut bytes = Vec::new();
        opened.file.read_to_end(&mut bytes)?;
        assert_eq!(bytes, b"in-root bytes");
        assert!(handle.open_read(Path::new("file-link")).is_err());
        assert!(handle.open_read(Path::new("dir-link/canary")).is_err());
        assert!(handle.open_dir(Path::new("dir-link")).is_err());
        assert!(handle.open_read(Path::new("../outside/canary")).is_err());
        assert!(handle.open_dir(Path::new("dir")).is_ok());
        let first = handle.root_dir().list_entries()?;
        assert_eq!(first.len(), 4);
        assert_eq!(handle.root_dir().list_entries()?, first);
        handle.verify()?;
    }
    assert_eq!(fs::read(outside.join("canary"))?, b"outside must remain");
    let result = r#"{"passed":true,"checks":["real-btrfs-root","directory-required","root-symlink-rejected","leaf-and-component-symlinks-rejected","openat2-forced-success","component-walk-forced-success","repeated-pinned-directory-enumeration-complete","outside-canary-preserved"]}"#;
    println!("{result}");
    Ok(())
}
