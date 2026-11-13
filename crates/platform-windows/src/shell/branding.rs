//! Durable Explorer presentation resources, independent of the portable executable location.
use crate::files::VaultHandle;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use windows::Win32::{
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_LocalAppData, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath},
};

const ICON: &[u8] = include_bytes!("../../../../apps/desktop/src-tauri/icons/icon.ico");

pub(super) fn display_name(path: &Path) -> io::Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| path.is_absolute() && !name.is_empty() && !name.contains('\0'))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid sync root name"))
}

fn icon_resource(path: &Path) -> io::Result<String> {
    let path = path
        .to_str()
        .filter(|text| path.is_absolute() && !text.contains(['\0', '"']))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid shell icon path"))?;
    // Shell's icon-location syntax uses a comma before the index. Quote the path so
    // Unicode user profiles and names containing commas remain a single file name.
    Ok(format!("\"{path}\",0"))
}

pub(super) fn persistent_icon(vault: &VaultHandle) -> io::Result<String> {
    // Resolve the current user's known folder, not an environment override or the EXE path.
    let allocated =
        unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KNOWN_FOLDER_FLAG(0), None) }
            .map_err(super::map)?;
    let local = unsafe { allocated.to_string() }.map_err(io::Error::other);
    unsafe { CoTaskMemFree(Some(allocated.0.cast())) };
    let mut directory = PathBuf::from(local?);
    let mut pinned = VaultHandle::open(&directory)?;
    // Create only handle-relative children under continuously pinned ancestors. Even
    // a binding of LocalAppData (or vaultsyncd itself) must not receive our internal ICO.
    for name in ["vaultsyncd", "shell-icons"] {
        if pinned.is_within(vault) {
            return Err(io::Error::other(
                "shell icons would be inside the sync root",
            ));
        }
        directory.push(name);
        pinned = match VaultHandle::open(&directory) {
            Ok(existing) => existing,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match pinned.create_new_directory(name) {
                    Ok(created) => created,
                    // Another binding/process may have created this same private directory.
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        VaultHandle::open(&directory)?
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
    }
    if pinned.overlaps(vault) {
        return Err(io::Error::other(
            "shell icon directory overlaps the sync root",
        ));
    }
    install_icon(&directory)
}

// Access denied can also mean a brief delete-sharing conflict during a rename on
// Windows. Retry only these OS errors, for at most 475 ms; persistent ACL/read-only
// failures must still reach the caller with their original error code.
const ICON_IO_ATTEMPTS: usize = 20;
fn retry_icon_io<T>(mut work: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    for attempt in 0..ICON_IO_ATTEMPTS {
        match work() {
            Err(error)
                if matches!(error.raw_os_error(), Some(5 | 32 | 33))
                    && attempt + 1 < ICON_IO_ATTEMPTS =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            result => return result,
        }
    }
    unreachable!("the last attempt always returns its result")
}

fn install_icon(directory: &Path) -> io::Result<String> {
    // Most callers already use the Shell worker, but tests/other in-process users
    // may arrive together. Avoid needlessly replacing a file another thread just installed.
    static INSTALL: Mutex<()> = Mutex::new(());
    let _install = INSTALL
        .lock()
        .map_err(|_| io::Error::other("shell icon installation lock poisoned"))?;
    let path = directory.join(format!(
        "vaultsyncd-{}.ico",
        crate::cfapi::content::sha256_hex(ICON)
    ));
    let resource = icon_resource(&path)?;
    match retry_icon_io(|| fs::read(&path)) {
        Ok(bytes) if bytes == ICON => return Ok(resource),
        Ok(_) => {} // Repair an incomplete/corrupt old copy by atomic replacement below.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (temporary, mut file) = loop {
        let temporary = directory.join(format!(
            ".icon-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        file.write_all(ICON)?;
        file.sync_all()?;
        drop(file);
        // Both files are in the same private directory. Never expose partially written
        // ICO bytes to Explorer; keep older content-addressed versions for registrations
        // from another running version.
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(source: *const u16, target: *const u16, flags: u32) -> i32;
        }
        let source = crate::cfapi::prototype::wide(&temporary);
        let target = crate::cfapi::prototype::wide(&path);
        retry_icon_io(|| {
            if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0x1 | 0x8) } != 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            // A competing process may already have published the exact same content.
            // Never accept unreadable/partial/different bytes as a successful install.
            if fs::read(&path).is_ok_and(|bytes| bytes == ICON) {
                return Ok(());
            }
            Err(error)
        })?;
        Ok(resource)
    })();
    // Also clean up when a different process won publication and our temp was unused.
    // A scanner can briefly hold that temp too; don't leave concurrent-install debris.
    if let Err(error) = retry_icon_io(|| fs::remove_file(&temporary)) {
        if error.kind() != io::ErrorKind::NotFound && result.is_ok() {
            return Err(error);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_shell_names_and_icon_paths() {
        for name in ["家庭 资料, Ω", "@work & files (2026)", "文档", "second"] {
            let path = Path::new("C:\\test").join(name);
            assert_eq!(display_name(&path).unwrap(), name);
        }
        for path in ["C:\\", "relative", "C:\\bad\0name"] {
            assert!(display_name(Path::new(path)).is_err(), "{path:?}");
        }
        assert_eq!(
            icon_resource(Path::new("C:\\用户, Ω\\shell-icons\\logo.ico")).unwrap(),
            "\"C:\\用户, Ω\\shell-icons\\logo.ico\",0"
        );
        for path in ["relative.ico", "C:\\bad\0.ico", "C:\\bad\".ico"] {
            assert!(icon_resource(Path::new(path)).is_err());
        }
    }

    #[test]
    fn native_shell_icon_is_durable_atomic_and_decodable() {
        let directory = super::super::native_tests::scratch("用户, Ω 图标");
        fs::create_dir_all(&directory).unwrap();
        let resource = install_icon(&directory).unwrap();
        let path = super::super::native_tests::decode_icon(&resource);
        assert_eq!(fs::read(&path).unwrap(), ICON);
        let before = VaultHandle::open(&directory)
            .unwrap()
            .inspect(path.file_name().unwrap().to_str().unwrap())
            .unwrap();
        assert_eq!(install_icon(&directory).unwrap(), resource);
        let after = VaultHandle::open(&directory)
            .unwrap()
            .inspect(path.file_name().unwrap().to_str().unwrap())
            .unwrap();
        assert_eq!(
            before.identity, after.identity,
            "restart should reuse valid ICO"
        );
        fs::write(&path, b"incomplete icon").unwrap();
        assert_eq!(install_icon(&directory).unwrap(), resource);
        assert_eq!(fs::read(&path).unwrap(), ICON);
        for round in 0..4 {
            fs::remove_file(&path).unwrap();
            let start = std::sync::Arc::new(std::sync::Barrier::new(8));
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    let directory = directory.clone();
                    let start = start.clone();
                    std::thread::spawn(move || {
                        start.wait();
                        install_icon(&directory)
                    })
                })
                .collect();
            for worker in workers {
                let installed = worker.join().unwrap();
                assert!(installed.is_ok(), "concurrent round {round}: {installed:?}");
                assert_eq!(installed.unwrap(), resource);
            }
            assert_eq!(fs::read(&path).unwrap(), ICON);
            assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        }
        fs::remove_file(&path).unwrap();
        let children: Vec<_> = (0..8)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "shell::branding::tests::native_shell_icon_install_child",
                        "--nocapture",
                    ])
                    .env("VAULTSYNCD_S222_ICON_DIRECTORY", &directory)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(fs::read(&path).unwrap(), ICON);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        super::super::native_tests::decode_icon(&resource);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_shell_icon_install_child() {
        let Some(directory) = std::env::var_os("VAULTSYNCD_S222_ICON_DIRECTORY") else {
            return;
        };
        let resource = install_icon(Path::new(&directory)).unwrap();
        let path = super::super::native_tests::decode_icon(&resource);
        assert_eq!(fs::read(path).unwrap(), ICON);
    }

    #[test]
    fn native_shell_icon_sharing_retry_preserves_permission_errors() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = super::super::native_tests::scratch("图标共享冲突");
        fs::create_dir_all(&directory).unwrap();
        let resource = install_icon(&directory).unwrap();
        let path = super::super::native_tests::decode_icon(&resource);
        // First deny reads; then allow reads but deny delete-sharing, blocking only
        // MoveFileEx. Both transient failure sites must recover after the handle closes.
        for share in [0, 0x1 | 0x2] {
            fs::write(&path, b"incomplete icon").unwrap();
            let blocker = OpenOptions::new()
                .read(true)
                .share_mode(share)
                .open(&path)
                .unwrap();
            let release = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                drop(blocker);
            });
            let installed = install_icon(&directory);
            release.join().unwrap();
            assert_eq!(installed.unwrap(), resource);
            assert_eq!(fs::read(&path).unwrap(), ICON);
            assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        }
        // A persistent E5 must not be mistaken for a concurrent successful install.
        // Use a corrupt read-only destination rather than touching user ACLs.
        fs::write(&path, b"do not accept corrupt read-only content").unwrap();
        let original_permissions = fs::metadata(&path).unwrap().permissions();
        let mut readonly = original_permissions.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&path, readonly).unwrap();
        let denied = install_icon(&directory);
        fs::set_permissions(&path, original_permissions).unwrap();
        assert_eq!(denied.unwrap_err().raw_os_error(), Some(5));
        assert_eq!(
            fs::read(&path).unwrap(),
            b"do not accept corrupt read-only content"
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        assert_eq!(install_icon(&directory).unwrap(), resource);
        assert_eq!(fs::read(&path).unwrap(), ICON);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_shell_icon_retry_is_bounded_and_os_error_specific() {
        let mut calls = 0;
        let recovered = retry_icon_io(|| {
            calls += 1;
            if calls < 3 {
                Err(io::Error::from_raw_os_error(5))
            } else {
                Ok(())
            }
        });
        recovered.unwrap();
        assert_eq!(calls, 3);
        calls = 0;
        let denied: io::Result<()> = retry_icon_io(|| {
            calls += 1;
            Err(io::Error::from_raw_os_error(5))
        });
        assert_eq!(denied.unwrap_err().raw_os_error(), Some(5));
        assert_eq!(calls, ICON_IO_ATTEMPTS);
        calls = 0;
        let missing: io::Result<()> = retry_icon_io(|| {
            calls += 1;
            Err(io::Error::from_raw_os_error(2))
        });
        assert_eq!(missing.unwrap_err().raw_os_error(), Some(2));
        assert_eq!(calls, 1);
    }
}
