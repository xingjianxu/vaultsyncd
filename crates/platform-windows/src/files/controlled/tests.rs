use super::content::*;
use super::*;
use crate::cfapi::{
    adapter::{Adapter, MetadataOnly},
    catalog::Catalog,
};
use crate::files::RootHandle;
use std::{os::windows::fs::OpenOptionsExt, path::PathBuf, process::Command, sync::Arc};
use vaultsyncd_domain::ids::{BindingId, RootId};

#[test]
fn native_application_namespace_keeps_content_and_refuses_collisions() {
    let lab = Lab::new();
    let root = RootHandle::open(&lab.base.join("root")).unwrap();
    assert!(root.ensure_directory("child").unwrap().directory);
    assert!(root.ensure_directory("child").unwrap().directory);
    std::fs::write(lab.path(), b"local-only").unwrap();
    assert!(root.ensure_directory("a.txt").is_err());
    let source = lab.observe();
    std::fs::write(lab.base.join("root/occupied"), b"user").unwrap();
    assert!(
        lab.files
            .preserve("a.txt", "occupied", source.evidence())
            .is_err()
    );
    assert_eq!(std::fs::read(lab.path()).unwrap(), b"local-only");
    let moved = lab
        .files
        .preserve("a.txt", "child/kept", source.evidence())
        .unwrap();
    assert_eq!(moved, *source.evidence());
    assert!(!lab.path().exists());
    assert_eq!(
        std::fs::read(lab.base.join("root/child/kept")).unwrap(),
        b"local-only"
    );
    assert_eq!(
        std::fs::read(lab.base.join("root/occupied")).unwrap(),
        b"user"
    );
    assert!(root.ensure_directory("../escape").is_err());
}

struct Held(Option<WindowsFiles>);
impl std::ops::Deref for Held {
    type Target = WindowsFiles;
    fn deref(&self) -> &WindowsFiles {
        self.0.as_ref().unwrap()
    }
}
struct Lab {
    base: PathBuf,
    files: Held,
}
impl Lab {
    fn new() -> Self {
        let base = PathBuf::from(format!(
            "C:/vaultsyncd-lab/s068/run-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Self::at(base)
    }
    fn at(base: PathBuf) -> Self {
        std::fs::create_dir_all(base.join("root")).unwrap();
        std::fs::create_dir_all(base.join("state")).unwrap();
        let files = WindowsFiles::open(&base.join("root"), &base.join("state")).unwrap();
        Self {
            base,
            files: Held(Some(files)),
        }
    }
    fn path(&self) -> PathBuf {
        self.base.join("root/a.txt")
    }
    fn stage(&self) -> PathBuf {
        let p = self.base.join("incoming");
        std::fs::write(&p, b"remote-new").unwrap();
        p
    }
    fn observe(&self) -> Arc<FixedSnapshot> {
        match self.files.observe("a.txt").unwrap() {
            ObservedContent::File(s) => s,
            _ => panic!("file missing"),
        }
    }
}
impl Drop for Lab {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.files.0.take();
            std::fs::remove_dir_all(&self.base).unwrap();
        }
    }
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    vaultsyncd_transfer::hash::digest_of(bytes)
}
const OP: &str = "10101010101010101010101010101010";

#[test]
fn native_snapshots_generation_and_rename_over_are_real_content_based() {
    let lab = Lab::new();
    std::fs::write(lab.path(), b"initial").unwrap();
    let first = lab.observe();
    std::fs::write(lab.path(), b"changed").unwrap();
    let second = lab.observe();
    assert_eq!(
        second.evidence().generation,
        first.evidence().generation + 1
    );
    assert_eq!(first.read_at(0, 7).unwrap(), b"initial");
    let temp = lab.base.join("root/editor.tmp");
    std::fs::write(&temp, b"atomic-save").unwrap();
    // The actual Win32 ReplaceFile API performs the editor's temporary+rename-over save.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(
            old: *const u16,
            new: *const u16,
            backup: *const u16,
            flags: u32,
            a: *const c_void,
            b: *const c_void,
        ) -> i32;
    }
    let wide = |p: &Path| {
        p.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    assert_ne!(
        unsafe {
            ReplaceFileW(
                wide(&lab.path()).as_ptr(),
                wide(&temp).as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0,
        "{:?}",
        io::Error::last_os_error()
    );
    let third = lab.observe();
    assert_ne!(third.evidence().file, second.evidence().file);
    assert_eq!(
        third.evidence().generation,
        second.evidence().generation + 1
    );
    assert!(
        lab.files
            .replace(
                OP,
                "a.txt",
                &lab.stage(),
                Some(first.evidence()),
                digest(b"remote-new")
            )
            .is_err()
    );
    assert_eq!(std::fs::read(lab.path()).unwrap(), b"atomic-save");
    // Old fixed snapshot remains stable during later saves (S056 generation boundary).
    assert_eq!(second.read_at(0, 7).unwrap(), b"changed");
}

#[test]
fn native_protection_rejects_all_shared_writers_readers_and_mapping_after_file_close() {
    let lab = Lab::new();
    std::fs::write(lab.path(), b"original").unwrap();
    let before = lab.observe();
    let logical = crate::cfapi::catalog::Identity {
        binding: BindingId::from_bytes([0x73; 16]),
        root: RootId::from_bytes([0x74; 16]),
        entry: vaultsyncd_domain::ids::EntryId::from_bytes([0x75; 16]),
        version: vaultsyncd_domain::ids::VersionId::from_bytes([0x76; 16]),
    };
    let mut adapter = Adapter::connect(
        &lab.base.join("root"),
        Catalog::new(logical.binding, logical.root),
        Arc::new(MetadataOnly),
    )
    .unwrap();
    let original_meta = adapter.inspect("a.txt").unwrap();
    for share in [0, 1, 7] {
        let writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(share)
            .open(lab.path())
            .unwrap();
        assert!(lab.files.observe("a.txt").is_err());
        assert!(
            lab.files
                .replace(
                    OP,
                    "a.txt",
                    &lab.stage(),
                    Some(before.evidence()),
                    digest(b"remote-new")
                )
                .is_err()
        );
        drop(writer);
        assert_eq!(std::fs::read(lab.path()).unwrap(), b"original");
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileMappingW(
            file: *mut c_void,
            attrs: *const c_void,
            protect: u32,
            high: u32,
            low: u32,
            name: *const u16,
        ) -> *mut c_void;
        fn MapViewOfFile(
            mapping: *mut c_void,
            access: u32,
            hi: u32,
            lo: u32,
            size: usize,
        ) -> *mut c_void;
        fn UnmapViewOfFile(view: *const c_void) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    use std::os::windows::io::AsRawHandle;
    let writer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(7)
        .open(lab.path())
        .unwrap();
    let mapping = unsafe {
        CreateFileMappingW(
            writer.as_raw_handle(),
            std::ptr::null(),
            4,
            0,
            0,
            std::ptr::null(),
        )
    };
    assert!(!mapping.is_null());
    let view = unsafe { MapViewOfFile(mapping, 2, 0, 0, 0) };
    assert!(!view.is_null());
    drop(writer);
    assert!(
        adapter
            .adopt_complete("a.txt", &original_meta, logical, digest(b"original"), 8)
            .is_err(),
        "live mapping must prevent adoption/in-sync even after file handle closes"
    );
    assert!(adapter.inspect("a.txt").unwrap().cloud.is_none());
    assert!(
        lab.files.observe("a.txt").is_err(),
        "live writable section must refuse even after the creating file handle closes"
    );
    unsafe {
        std::ptr::copy_nonoverlapping(b"mapped!!".as_ptr(), view.cast::<u8>(), 8);
        assert_ne!(UnmapViewOfFile(view), 0);
        assert_ne!(CloseHandle(mapping), 0);
    }
    assert_eq!(std::fs::read(lab.path()).unwrap(), b"mapped!!");
    assert!(
        lab.files
            .replace(
                OP,
                "a.txt",
                &lab.stage(),
                Some(before.evidence()),
                digest(b"remote-new")
            )
            .is_err()
    );
    adapter.unregister().unwrap();
}

#[test]
fn native_publish_preserves_old_object_and_verifies_stage() {
    let lab = Lab::new();
    std::fs::write(lab.path(), b"original").unwrap();
    let before = lab.observe();
    let staged = lab.stage();
    assert!(
        lab.files
            .replace(
                "20202020202020202020202020202020",
                "a.txt",
                &staged,
                Some(before.evidence()),
                digest(b"wrong")
            )
            .is_err()
    );
    assert_eq!(std::fs::read(lab.path()).unwrap(), b"original");
    let after = lab
        .files
        .replace(
            OP,
            "a.txt",
            &staged,
            Some(before.evidence()),
            digest(b"remote-new"),
        )
        .unwrap();
    assert!(after.generation > before.evidence().generation);
    assert_eq!(std::fs::read(lab.path()).unwrap(), b"remote-new");
    assert_eq!(
        std::fs::read(lab.base.join(format!("state/preserved-{OP}.data"))).unwrap(),
        b"original"
    );
    assert_eq!(
        lab.files.recover_publications().unwrap(),
        vec![RecoveryOutcome::Published(OP.into())]
    );
    lab.files.acknowledge_publication(OP).unwrap();
    assert!(lab.files.recover_publications().unwrap().is_empty());
}

#[test]
fn native_real_cloud_editor_atomic_save_and_guarded_remote_update() {
    use crate::cfapi::catalog::Identity;
    use vaultsyncd_domain::ids::{EntryId, VersionId};
    let lab = Lab::new();
    let binding = BindingId::from_bytes([0x68; 16]);
    let root = RootId::from_bytes([0x69; 16]);
    let mut adapter = Adapter::connect(
        &lab.base.join("root"),
        Catalog::new(binding, root),
        Arc::new(MetadataOnly),
    )
    .unwrap();
    let id = Identity {
        binding,
        root,
        entry: EntryId::from_bytes([0x70; 16]),
        version: VersionId::from_bytes([0x71; 16]),
    };
    adapter.create("a.txt", id, 0, false).unwrap();
    // A real application writes a hydrated empty placeholder, then atomically saves over it.
    std::fs::write(lab.path(), b"local-dirty").unwrap();
    let first = lab.observe();
    let temp = lab.base.join("root/editor.tmp");
    std::fs::write(&temp, b"saved-atomic").unwrap();
    // MoveFileEx performs the same replacement namespace operation used by editor atomic saves.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let wide = |p: &Path| {
        p.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    assert_ne!(
        unsafe { MoveFileExW(wide(&temp).as_ptr(), wide(&lab.path()).as_ptr(), 1 | 8) },
        0,
        "{:?}",
        io::Error::last_os_error()
    );
    let saved = lab.observe();
    assert_eq!(saved.read_at(0, 12).unwrap(), b"saved-atomic");
    assert_ne!(saved.evidence().file, first.evidence().file);
    assert!(
        lab.files
            .replace(
                OP,
                "a.txt",
                &lab.stage(),
                Some(first.evidence()),
                digest(b"remote-new")
            )
            .is_err()
    );
    assert_eq!(std::fs::read(lab.path()).unwrap(), b"saved-atomic");
    drop(first);
    drop(saved);
    std::fs::remove_file(lab.path()).unwrap();
    adapter.unregister().unwrap();
}

#[test]
fn native_child_publication_crash() {
    let Ok(base) = std::env::var("VAULTSYNCD_S068_CHILD") else {
        return;
    };
    let lab = Lab::at(PathBuf::from(base));
    let before = lab.observe();
    let staged = lab.stage();
    lab.files
        .replace(
            OP,
            "a.txt",
            &staged,
            Some(before.evidence()),
            digest(b"remote-new"),
        )
        .unwrap();
    lab.files.acknowledge_publication(OP).unwrap();
    panic!("crash checkpoint did not terminate");
}
#[test]
fn native_restart_after_each_publication_boundary_preserves_saved_bytes() {
    for (point, code) in [("preserved", 121), ("published", 122), ("before_ack", 123)] {
        let lab = Lab::new();
        std::fs::write(lab.path(), b"original").unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "files::controlled::tests::native_child_publication_crash",
                "--nocapture",
            ])
            .env("VAULTSYNCD_S068_CHILD", &lab.base)
            .env("VAULTSYNCD_S068_CRASH", point)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(code));
        let result = lab.files.recover_publications().unwrap();
        if point == "preserved" {
            assert_eq!(result, vec![RecoveryOutcome::Restored(OP.into())]);
            assert_eq!(std::fs::read(lab.path()).unwrap(), b"original");
        } else {
            assert_eq!(result, vec![RecoveryOutcome::Published(OP.into())]);
            assert_eq!(std::fs::read(lab.path()).unwrap(), b"remote-new");
            assert_eq!(
                std::fs::read(lab.base.join(format!("state/preserved-{OP}.data"))).unwrap(),
                b"original"
            );
        }
    }
}

#[test]
fn native_retry_same_operation_and_restarted_generation_keep_new_saved_content() {
    let lab = Lab::new();
    std::fs::write(lab.path(), b"original").unwrap();
    let before = lab.observe();
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(7)
        .open(lab.path())
        .unwrap();
    assert!(
        lab.files
            .replace(
                OP,
                "a.txt",
                &lab.stage(),
                Some(before.evidence()),
                digest(b"remote-new")
            )
            .is_err()
    );
    drop(writer);
    let new = lab
        .files
        .replace(
            OP,
            "a.txt",
            &lab.stage(),
            Some(before.evidence()),
            digest(b"remote-new"),
        )
        .unwrap();
    assert_eq!(
        lab.files
            .replace(
                OP,
                "a.txt",
                &lab.stage(),
                Some(before.evidence()),
                digest(b"remote-new")
            )
            .unwrap(),
        new
    );
    std::fs::write(lab.path(), b"saved-later").unwrap();
    assert!(
        lab.files
            .replace(
                OP,
                "a.txt",
                &lab.stage(),
                Some(before.evidence()),
                digest(b"remote-new")
            )
            .is_err()
    );
    assert_eq!(std::fs::read(lab.path()).unwrap(), b"saved-later");
    let restarted = WindowsFiles::open(&lab.base.join("root"), &lab.base.join("state")).unwrap();
    restarted.seed("a.txt", new.clone()).unwrap();
    let ObservedContent::File(actual) = restarted.observe("a.txt").unwrap() else {
        panic!("missing")
    };
    assert!(actual.evidence().generation > new.generation);
    assert_eq!(
        restarted.recover_publications().unwrap(),
        vec![RecoveryOutcome::Blocked(OP.into())]
    );
}

#[test]
fn native_crash_gap_new_target_and_corrupt_journal_never_overwrite_or_clear_evidence() {
    let lab = Lab::new();
    std::fs::write(lab.path(), b"original").unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "files::controlled::tests::native_child_publication_crash",
            "--nocapture",
        ])
        .env("VAULTSYNCD_S068_CHILD", &lab.base)
        .env("VAULTSYNCD_S068_CRASH", "preserved")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(121));
    std::fs::write(lab.path(), b"user-created-during-gap").unwrap();
    assert_eq!(
        lab.files.recover_publications().unwrap(),
        vec![RecoveryOutcome::Blocked(OP.into())]
    );
    assert_eq!(
        std::fs::read(lab.path()).unwrap(),
        b"user-created-during-gap"
    );
    assert_eq!(
        std::fs::read(lab.base.join(format!("state/preserved-{OP}.data"))).unwrap(),
        b"original"
    );
    assert_eq!(
        std::fs::read(lab.base.join(format!("state/publication-{OP}.data"))).unwrap(),
        b"remote-new"
    );
    let journal = lab.base.join(format!("state/publication-{OP}.json"));
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    json["target"] = serde_json::Value::String("../outside".into());
    std::fs::write(&journal, serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(lab.files.recover_publications().is_err());
    assert!(journal.exists());
    assert_eq!(
        std::fs::read(lab.path()).unwrap(),
        b"user-created-during-gap"
    );
}
