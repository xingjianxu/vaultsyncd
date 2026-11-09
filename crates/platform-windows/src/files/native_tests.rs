use super::*;
use crate::watcher::{DirectoryWatcher, RescanReason, WatchBatch};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let path = PathBuf::from(format!(
            "C:/vaultsyncd-lab/s065/run-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn native_identity_tracks_object_not_contents_or_path_spelling() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.0.join("a.txt"), b"one").unwrap();
    let vault = VaultHandle::open(&sandbox.0).unwrap();
    let first = vault.inspect("a.txt").unwrap();
    std::fs::write(sandbox.0.join("a.txt"), b"two").unwrap();
    assert_eq!(vault.inspect("A.TXT").unwrap().identity, first.identity);
    std::fs::write(sandbox.0.join("replacement"), b"two").unwrap();
    std::fs::remove_file(sandbox.0.join("a.txt")).unwrap();
    std::fs::rename(sandbox.0.join("replacement"), sandbox.0.join("a.txt")).unwrap();
    assert_ne!(vault.inspect("a.txt").unwrap().identity, first.identity);
    let report = vault.scan();
    assert_eq!(report.health, ScanHealth::Complete, "{:?}", report.entries);
    assert_eq!(report.entries.len(), 1);
}

#[test]
fn native_names_match_real_case_and_unicode_creation_without_normalization() {
    let sandbox = Sandbox::new();
    for (left, right) in [("a.txt", "A.TXT"), ("ä.txt", "Ä.TXT")] {
        assert!(NativeNames.equivalent(left, right));
        std::fs::write(sandbox.0.join(left), b"kept").unwrap();
        assert_eq!(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(sandbox.0.join(right))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
    }
    assert!(!NativeNames.equivalent("é.txt", "e\u{301}.txt"));
    std::fs::write(sandbox.0.join("é.txt"), b"one").unwrap();
    std::fs::write(sandbox.0.join("e\u{301}.txt"), b"two").unwrap();
    let vault = VaultHandle::open(&sandbox.0).unwrap();
    assert_ne!(
        vault.inspect("é.txt").unwrap().identity,
        vault.inspect("e\u{301}.txt").unwrap().identity
    );
    assert!(vault.scan().permits_missing_comparison());
}

#[test]
fn native_overlap_is_identity_and_component_based_and_pins_prevent_rename() {
    let sandbox = Sandbox::new();
    for dir in ["a/child", "abc"] {
        std::fs::create_dir_all(sandbox.0.join(dir)).unwrap();
    }
    let vault = VaultHandle::open(&sandbox.0.join("a")).unwrap();
    let child = VaultHandle::open(&sandbox.0.join("a/child")).unwrap();
    let sibling = VaultHandle::open(&sandbox.0.join("abc")).unwrap();
    assert!(vault.overlaps(&child));
    assert!(child.overlaps(&vault));
    assert!(!vault.overlaps(&sibling));
    assert!(std::fs::rename(sandbox.0.join("a"), sandbox.0.join("moved")).is_err());
}

#[test]
fn native_drive_alias_and_hard_links_keep_physical_identity() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
    }
    let sandbox = Sandbox::new();
    let drives = unsafe { GetLogicalDrives() };
    assert_ne!(drives, 0);
    let drive = (b'D'..=b'Z')
        .rev()
        .find(|letter| drives & (1u32 << (letter - b'A')) == 0)
        .expect("free test drive letter");
    let letter = format!("{}:", drive as char);
    assert!(
        Command::new("subst")
            .arg(&letter)
            .arg(sandbox.0.to_string_lossy().replace('/', "\\"))
            .output()
            .unwrap()
            .status
            .success()
    );
    struct Alias(String);
    impl Drop for Alias {
        fn drop(&mut self) {
            assert!(
                Command::new("subst")
                    .args([self.0.as_str(), "/D"])
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
    }
    let _alias = Alias(letter.clone());
    std::fs::write(sandbox.0.join("one"), b"kept").unwrap();
    std::fs::hard_link(sandbox.0.join("one"), sandbox.0.join("two")).unwrap();
    let physical = VaultHandle::open(&sandbox.0).unwrap();
    let alias = VaultHandle::open(Path::new(&format!("{letter}\\"))).unwrap();
    assert_eq!(physical.identity(), alias.identity());
    assert!(physical.overlaps(&alias));
    assert_eq!(
        physical.inspect("one").unwrap().identity,
        alias.inspect("two").unwrap().identity
    );
    let report = physical.scan();
    assert!(!report.permits_missing_comparison());
    assert!(report.entries.iter().all(|entry| {
        entry
            .issue
            .as_ref()
            .is_some_and(|issue| issue.contains("hard links"))
    }));
}

#[test]
fn native_junction_is_never_followed_and_external_content_is_unchanged() {
    let sandbox = Sandbox::new();
    std::fs::create_dir(sandbox.0.join("vault")).unwrap();
    std::fs::create_dir(sandbox.0.join("outside")).unwrap();
    std::fs::write(sandbox.0.join("outside/secret"), b"external-proof").unwrap();
    let result = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(
            sandbox
                .0
                .join("vault/link")
                .to_string_lossy()
                .replace('/', "\\"),
        )
        .arg(
            sandbox
                .0
                .join("outside")
                .to_string_lossy()
                .replace('/', "\\"),
        )
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let vault = VaultHandle::open(&sandbox.0.join("vault")).unwrap();
    assert!(vault.inspect("link/secret").is_err());
    assert!(VaultHandle::open(&sandbox.0.join("vault/link")).is_err());
    let report = vault.scan();
    assert!(!report.permits_missing_comparison());
    assert!(
        report
            .entries
            .iter()
            .any(|entry| entry.path == "link" && entry.issue.is_some())
    );
    assert_eq!(
        std::fs::read(sandbox.0.join("outside/secret")).unwrap(),
        b"external-proof"
    );
    drop(vault);
    std::fs::remove_dir(sandbox.0.join("vault/link")).unwrap();
}

#[test]
fn native_file_symlink_and_retarget_never_expose_outside_content() {
    let sandbox = Sandbox::new();
    std::fs::create_dir(sandbox.0.join("vault")).unwrap();
    std::fs::create_dir(sandbox.0.join("outside")).unwrap();
    std::fs::write(sandbox.0.join("outside/secret"), b"first external target").unwrap();
    std::fs::write(sandbox.0.join("outside/other"), b"second external target").unwrap();
    std::fs::write(sandbox.0.join("vault/note.txt"), b"inside content").unwrap();
    let link = sandbox.0.join("vault/link");
    let create = |target: &str| {
        let result = Command::new("cmd")
            .args(["/c", "mklink"])
            .arg(link.to_string_lossy().replace('/', "\\"))
            .arg(sandbox.0.join(target).to_string_lossy().replace('/', "\\"))
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
    };
    create("outside/secret");

    let vault = VaultHandle::open(&sandbox.0.join("vault")).unwrap();
    assert!(
        vault.open_read("link").is_err(),
        "a file symlink must never be opened as vault content"
    );
    if let Ok(metadata) = vault.inspect("link") {
        assert_ne!(metadata.reparse_tag, 0, "symlink must stay a reparse point");
    }
    let report = vault.scan();
    assert!(
        !report.permits_missing_comparison(),
        "a vault containing a symlink cannot claim complete enumeration"
    );
    assert!(
        report
            .entries
            .iter()
            .any(|entry| entry.path == "link" && entry.issue.is_some()),
        "symlink must be reported as an issue, never adopted"
    );
    assert_eq!(
        std::fs::read(sandbox.0.join("outside/secret")).unwrap(),
        b"first external target"
    );

    // Retarget the symlink in place: the rooted handle must still refuse to follow it.
    std::fs::remove_file(&link).unwrap();
    create("outside/other");
    assert!(vault.open_read("link").is_err());
    let report = vault.scan();
    assert!(!report.permits_missing_comparison());
    assert!(
        report
            .entries
            .iter()
            .any(|entry| entry.path == "link" && entry.issue.is_some())
    );
    assert_eq!(
        std::fs::read(sandbox.0.join("outside/secret")).unwrap(),
        b"first external target"
    );
    assert_eq!(
        std::fs::read(sandbox.0.join("outside/other")).unwrap(),
        b"second external target"
    );
    drop(vault);
    std::fs::remove_file(&link).unwrap();
}

#[test]
fn native_in_place_junction_change_cannot_redirect_an_already_pinned_parent() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn DeviceIoControl(
            file: *mut c_void,
            code: u32,
            input: *const c_void,
            input_size: u32,
            output: *mut c_void,
            output_size: u32,
            returned: *mut u32,
            overlapped: *const c_void,
        ) -> i32;
    }
    let sandbox = Sandbox::new();
    std::fs::create_dir(sandbox.0.join("switch")).unwrap();
    std::fs::create_dir(sandbox.0.join("outside")).unwrap();
    std::fs::write(sandbox.0.join("outside/secret"), b"untouched").unwrap();
    let vault = VaultHandle::open(&sandbox.0).unwrap();
    let pinned = open_relative(vault.vault_file(), "switch", true).unwrap();
    let target = sandbox
        .0
        .join("outside")
        .to_string_lossy()
        .replace('/', "\\");
    let substitute: Vec<u16> = format!("\\??\\{target}").encode_utf16().collect();
    let printable: Vec<u16> = target.encode_utf16().collect();
    let mut data = vec![];
    data.extend(0xa000_0003u32.to_le_bytes());
    data.extend(
        ((substitute.len() + printable.len() + 2) * 2 + 8).to_le_bytes()[..2]
            .iter()
            .copied(),
    );
    data.extend(0u16.to_le_bytes());
    for value in [
        0,
        substitute.len() as u16 * 2,
        (substitute.len() as u16 + 1) * 2,
        printable.len() as u16 * 2,
    ] {
        data.extend(value.to_le_bytes());
    }
    for unit in substitute
        .iter()
        .copied()
        .chain([0])
        .chain(printable)
        .chain([0])
    {
        data.extend(unit.to_le_bytes());
    }
    let writer = OpenOptions::new()
        .access_mode(0x4000_0000)
        .share_mode(7)
        .custom_flags(FLAGS)
        .open(sandbox.0.join("switch"))
        .unwrap();
    let mut returned = 0;
    assert_ne!(
        unsafe {
            DeviceIoControl(
                writer.as_raw_handle(),
                0x0009_00a4,
                data.as_ptr().cast(),
                data.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null(),
            )
        },
        0,
        "{}",
        io::Error::last_os_error()
    );
    assert_eq!(metadata(&pinned).unwrap().reparse_tag, 0xa000_0003);
    assert!(
        open_relative(&pinned, "secret", false).is_err(),
        "a rooted open must never follow the parent junction"
    );
    assert!(vault.inspect("switch/secret").is_err());
    assert_eq!(
        std::fs::read(sandbox.0.join("outside/secret")).unwrap(),
        b"untouched"
    );
    drop(writer);
    drop(pinned);
    drop(vault);
    std::fs::remove_dir(sandbox.0.join("switch")).unwrap();
}

#[test]
fn native_path_errors_are_explicit_and_extended_long_paths_work() {
    let sandbox = Sandbox::new();
    let vault = VaultHandle::open(&sandbox.0).unwrap();
    for path in [
        "../outside",
        "a/../outside",
        "CON",
        "a.",
        "a ",
        "C:/outside",
        "\\\\?\\C:\\outside",
    ] {
        assert_eq!(
            vault.inspect(path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "{path}"
        );
    }
    assert_eq!(
        vault.inspect(&"a".repeat(256)).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    let relative = format!(
        "{}/{}/{}/file.txt",
        "x".repeat(90),
        "y".repeat(90),
        "z".repeat(90)
    );
    let extended = PathBuf::from(format!(
        "\\\\?\\{}",
        sandbox.0.to_string_lossy().replace('/', "\\")
    ))
    .join(relative.replace('/', "\\"));
    std::fs::create_dir_all(extended.parent().unwrap()).unwrap();
    std::fs::write(&extended, b"long").unwrap();
    assert_eq!(vault.inspect(&relative).unwrap().size, 4);
    assert!(vault.scan().permits_missing_comparison());
}

#[test]
fn native_acl_failure_is_partial_evidence_never_deletion() {
    let sandbox = Sandbox::new();
    let protected = sandbox.0.join("protected");
    std::fs::create_dir(&protected).unwrap();
    std::fs::write(protected.join("kept"), b"kept").unwrap();
    let vault = VaultHandle::open(&sandbox.0).unwrap();
    assert!(
        Command::new("icacls")
            .arg(&protected)
            .args(["/deny", "*S-1-1-0:(RX)"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let report = vault.scan();
    let restored = Command::new("icacls")
        .arg(&protected)
        .args(["/remove:d", "*S-1-1-0"])
        .output()
        .unwrap();
    assert!(restored.status.success());
    assert_eq!(
        report.health,
        ScanHealth::PermissionDenied,
        "{:?}",
        report.entries
    );
    assert!(!report.permits_missing_comparison());
    assert_eq!(std::fs::read(protected.join("kept")).unwrap(), b"kept");
    assert!(vault.scan().permits_missing_comparison());
    let mut watcher = DirectoryWatcher::new(&vault, 65536).unwrap();
    assert!(
        Command::new("icacls")
            .arg(&sandbox.0)
            .args(["/deny", "*S-1-1-0:(RX)"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let unavailable = vault.scan();
    let lost = watcher.poll(1);
    let restored = Command::new("icacls")
        .arg(&sandbox.0)
        .args(["/remove:d", "*S-1-1-0"])
        .output()
        .unwrap();
    assert!(restored.status.success());
    assert_eq!(unavailable.health, ScanHealth::VaultUnavailable);
    assert_eq!(lost, WatchBatch::Rescan(RescanReason::WatchLost));
}

#[test]
fn native_watch_reports_hints_real_overflow_and_drop_cancels_pending_io() {
    let sandbox = Sandbox::new();
    let vault = VaultHandle::open(&sandbox.0).unwrap();
    let mut watcher = DirectoryWatcher::new(&vault, 64).unwrap();
    assert_eq!(watcher.poll(1), WatchBatch::Idle);
    for i in 0..400 {
        std::fs::write(
            sandbox.0.join(format!("file-{i:04}-with-long-name.txt")),
            b"observed",
        )
        .unwrap();
    }
    let mut overflow = false;
    for _ in 0..8 {
        if watcher.poll(100) == WatchBatch::Rescan(RescanReason::Overflow) {
            overflow = true;
            break;
        }
    }
    assert!(
        overflow,
        "native 64-byte watch must report actual buffer loss"
    );
    let report = vault.scan();
    assert!(report.permits_missing_comparison());
    assert_eq!(report.entries.len(), 400);
    // The scan can overlap further queued native completions. Drain them first;
    // a new zero-byte file needs one short record even in the intentional tiny buffer.
    for _ in 0..16 {
        if watcher.poll(1) == WatchBatch::Idle {
            break;
        }
    }
    drop(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(sandbox.0.join("x"))
            .unwrap(),
    );
    let next = watcher.poll(2000);
    assert!(matches!(next, WatchBatch::Changes(_)), "{next:?}");
    let start = Instant::now();
    drop(watcher);
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn native_cloud_scan_does_not_hydrate_any_placeholder() {
    use crate::cfapi::{
        prototype,
        prototype_layout::{PROBE_DIRECTORY, PROBE_FILES},
    };
    let sandbox = Sandbox::new();
    prototype::create(&sandbox.0).unwrap();
    struct Registration<'a>(&'a Path);
    impl Drop for Registration<'_> {
        fn drop(&mut self) {
            prototype::cleanup(self.0).unwrap();
        }
    }
    let _registration = Registration(&sandbox.0);
    // A disconnected partially populated directory can reject metadata opens on current Windows.
    // Such evidence must block deletion; reconnect before requiring complete enumeration.
    let offline = VaultHandle::open(&sandbox.0).unwrap().scan();
    if !offline.permits_missing_comparison() {
        assert!(
            offline.entries.iter().any(|e| e.issue.is_some())
                || offline.health == crate::files::ScanHealth::VaultUnavailable
        );
    }
    let session =
        crate::cfapi::provider::ProviderSession::open(&sandbox.0, Default::default()).unwrap();
    for (name, _) in PROBE_FILES {
        assert_eq!(
            prototype::on_disk_data_size(&sandbox.0.join(PROBE_DIRECTORY).join(name)).unwrap(),
            0
        );
    }
    {
        let vault = VaultHandle::open(&sandbox.0).unwrap();
        let report = vault.scan();
        assert!(
            report.permits_missing_comparison(),
            "{:?}: {:?}",
            report.health,
            report.entries
        );
        assert_eq!(report.entries.len(), 4);
        assert_eq!(session.context().stats().bytes_served, 0);
        for (name, size) in PROBE_FILES {
            let meta = vault.inspect(&format!("{PROBE_DIRECTORY}/{name}")).unwrap();
            assert!(meta.placeholder());
            assert_eq!(meta.size, size);
        }
    }
    for (name, _) in PROBE_FILES {
        assert_eq!(
            prototype::on_disk_data_size(&sandbox.0.join(PROBE_DIRECTORY).join(name)).unwrap(),
            0
        );
    }
    session.close().unwrap();
}
