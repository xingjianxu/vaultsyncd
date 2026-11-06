use super::*;
use crate::cfapi::catalog::Identity;
use std::sync::atomic::{AtomicU64, Ordering};
use vaultsyncd_domain::ids::{BindingId, EntryId, RootId, VersionId};
struct Counting(AtomicU64);
impl Callbacks for Counting {
    fn fetch(&self, _: ReadDemand) -> Result<(), i32> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(STATUS_NOT_SUPPORTED)
    }
    fn cancel(&self, _: Reply) {}
}
fn identity() -> Identity {
    Identity {
        binding: BindingId::from_bytes([66; 16]),
        root: RootId::from_bytes([67; 16]),
        entry: EntryId::from_bytes([68; 16]),
        version: VersionId::from_bytes([69; 16]),
    }
}
#[test]
fn native_adapter_lifecycle_metadata_restart_and_rename() {
    unsafe {
        windows::Win32::System::WinRT::RoInitialize(
            windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
        )
        .unwrap();
    }
    let path = PathBuf::from(format!(
        "C:\\vaultsyncd-lab\\s066-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("keep.txt"), b"user content").unwrap();
    let id = identity();
    let mut cat = Catalog::new(id.binding, id.root);
    cat.insert(id).unwrap();
    let callbacks = Arc::new(Counting(AtomicU64::new(0)));
    crate::shell::register(&path, &cat).unwrap();
    let mut adapter = Adapter::connect(&path, cat.clone(), callbacks.clone()).unwrap();
    assert!(crate::shell::registered(id.binding).unwrap());
    let original = adapter.create("remote.bin", id, 12345, false).unwrap();
    assert_eq!(original.size, 12345);
    assert_eq!(original.cloud.as_ref().unwrap().on_disk, 0);
    adapter.catalog().write().unwrap().insert(id).unwrap();
    let moved = adapter
        .move_entry("remote.bin", "renamed.bin", &original)
        .unwrap();
    assert_eq!(moved.identity, original.identity);
    assert_eq!(
        moved.cloud.as_ref().unwrap().identity,
        original.cloud.as_ref().unwrap().identity
    );
    assert_eq!(moved.cloud.as_ref().unwrap().on_disk, 0);
    let directory = Identity {
        entry: EntryId::from_bytes([70; 16]),
        ..id
    };
    let d = adapter.create("folder", directory, 0, true).unwrap();
    adapter
        .catalog()
        .write()
        .unwrap()
        .insert(directory)
        .unwrap();
    adapter.move_entry("folder", "moved-folder", &d).unwrap();
    assert_eq!(callbacks.0.load(Ordering::SeqCst), 0);
    let new_id = Identity {
        version: VersionId::from_bytes([71; 16]),
        ..id
    };
    let updated = adapter
        .update("renamed.bin", &moved, new_id, 54321)
        .unwrap();
    assert_eq!(updated.size, 54321);
    assert_eq!(
        Identity::decode(&updated.cloud.as_ref().unwrap().identity),
        Ok(new_id)
    );
    assert!(adapter.update("renamed.bin", &moved, id, 12345).is_err());
    cat.insert(new_id).unwrap();
    cat.insert(directory).unwrap();
    adapter.disconnect().unwrap();
    drop(adapter);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cfapi::adapter::native_tests::native_adapter_restart_child",
            "--nocapture",
        ])
        .env("VAULTSYNCD_S066_RESTART_ROOT", &path)
        .env(
            "VAULTSYNCD_S066_FILE_ID",
            format!("{:?}", original.identity),
        )
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "child failed: {} {}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let mut adapter = Adapter::connect(&path, cat.clone(), callbacks.clone()).unwrap();
    assert_eq!(
        adapter.inspect("renamed.bin").unwrap().identity,
        original.identity
    );
    let scan = adapter.scan();
    assert!(scan.permits_missing_comparison(), "{:?}", scan);
    assert!(scan.entries.iter().any(|e| e.path == "renamed.bin"));
    std::fs::write(path.join("occupied"), b"do not replace").unwrap();
    let observed = adapter.inspect("renamed.bin").unwrap();
    assert!(
        adapter
            .move_entry("renamed.bin", "occupied", &observed)
            .is_err()
    );
    assert_eq!(
        std::fs::read(path.join("occupied")).unwrap(),
        b"do not replace"
    );
    assert_eq!(
        cat.resolve(&id.encode()),
        Err(super::super::catalog::IdentityError::StaleVersion)
    );
    let mut broken = new_id.encode();
    broken[20] ^= 1;
    assert!(cat.resolve(&broken).is_err());
    assert_eq!(callbacks.0.load(Ordering::SeqCst), 0);
    // A separate connection verifies callback context/root routing without global process state.
    let other = path.with_extension("other");
    std::fs::create_dir(&other).unwrap();
    let foreign = Identity {
        binding: BindingId::from_bytes([80; 16]),
        root: RootId::from_bytes([81; 16]),
        ..id
    };
    let mut foreign_cat = Catalog::new(foreign.binding, foreign.root);
    foreign_cat.insert(foreign).unwrap();
    let mut second = Adapter::connect(&other, foreign_cat, callbacks.clone()).unwrap();
    assert!(second.create("wrong", id, 1, false).is_err());
    second.unregister().unwrap();
    drop(second);
    std::fs::remove_dir(&other).unwrap();
    assert!(std::fs::read(path.join("renamed.bin")).is_err());
    let calls = callbacks.0.load(Ordering::SeqCst);
    assert!(calls > 0, "valid callback was not dispatched");
    adapter.catalog().write().unwrap().insert(id).unwrap();
    assert!(std::fs::read(path.join("renamed.bin")).is_err());
    assert_eq!(
        callbacks.0.load(Ordering::SeqCst),
        calls,
        "stale version reached fetch port"
    );
    adapter.catalog().write().unwrap().insert(new_id).unwrap();
    let mutate_blob = |bytes: &[u8]| {
        adapter
            .root
            .with_parent("renamed.bin", |parent, name| {
                let file = open_child_shared(parent, name, false, false, 0x0010_0182, false, 0)?;
                hr(unsafe {
                    CfUpdatePlaceholder(
                        file.as_raw_handle(),
                        std::ptr::null(),
                        bytes.as_ptr().cast(),
                        bytes.len() as u32,
                        std::ptr::null(),
                        0,
                        0,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                })
            })
            .unwrap();
    };
    mutate_blob(&broken);
    assert!(std::fs::read(path.join("renamed.bin")).is_err());
    assert_eq!(
        callbacks.0.load(Ordering::SeqCst),
        calls,
        "corrupt identity reached fetch port"
    );
    mutate_blob(&new_id.encode());
    assert!(adapter.unregister().is_err());
    assert!(crate::shell::unregister(id.binding).is_err());
    assert_eq!(
        adapter.inspect("renamed.bin").unwrap().identity,
        original.identity
    );
    // Only this run's synthetic placeholders are explicitly removed by the test owner.
    std::fs::remove_file(path.join("renamed.bin")).unwrap();
    std::fs::remove_dir(path.join("moved-folder")).unwrap();
    adapter.unregister().unwrap();
    assert_eq!(
        std::fs::read(path.join("keep.txt")).unwrap(),
        b"user content"
    );
    crate::shell::unregister(id.binding).unwrap();
    assert!(!crate::shell::registered(id.binding).unwrap());
    drop(adapter);
    std::fs::remove_dir_all(path).unwrap();
    unsafe {
        windows::Win32::System::WinRT::RoUninitialize();
    }
}

#[test]
fn native_adapter_restart_child() {
    let Some(path) = std::env::var_os("VAULTSYNCD_S066_RESTART_ROOT") else {
        return;
    };
    let id = identity();
    let current = Identity {
        version: VersionId::from_bytes([71; 16]),
        ..id
    };
    let mut cat = Catalog::new(id.binding, id.root);
    cat.insert(current).unwrap();
    let mut adapter = Adapter::connect(Path::new(&path), cat, Arc::new(MetadataOnly)).unwrap();
    let observed = adapter.inspect("renamed.bin").unwrap();
    assert_eq!(
        format!("{:?}", observed.identity),
        std::env::var("VAULTSYNCD_S066_FILE_ID").unwrap()
    );
    assert_eq!(
        Identity::decode(&observed.cloud.unwrap().identity),
        Ok(current)
    );
    adapter.disconnect().unwrap();
}
