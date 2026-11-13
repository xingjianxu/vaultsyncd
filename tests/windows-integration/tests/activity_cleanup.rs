//! Explicit cleanup for the isolated native activity fixture, using production safety checks.
#![cfg(windows)]

use std::path::PathBuf;
use vaultsyncd_platform_windows::cfapi::maintenance;

#[test]
#[ignore = "requires the stopped native activity fixture and an explicit run root"]
fn cleanup_only_fully_materialized_activity_fixture() {
    let root = PathBuf::from(
        std::env::var_os("VAULTSYNCD_ACTIVITY_CLEANUP_ROOT")
            .expect("set this run's explicit activity fixture root"),
    );
    assert!(root.is_absolute());
    assert!(
        root.parent()
            .is_some_and(|parent| parent == std::path::Path::new(r"C:\vaultsyncd-lab"))
    );
    assert!(
        root.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("S172-native-"))
    );
    let fixture: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("fixture-runtime.json")).expect("read this fixture's paths"),
    )
    .expect("fixture JSON");
    let paths = [root.join("同步目录"), root.join("空目录")];
    assert_eq!(fixture["localPath"], paths[0].to_str().unwrap());
    assert_eq!(fixture["emptyPath"], paths[1].to_str().unwrap());
    assert_eq!(
        std::fs::read(paths[0].join("server-seed.txt")).expect("complete downloaded seed"),
        b"S172-native-server-seed"
    );
    assert_eq!(
        std::fs::read(paths[0].join("local-only.txt")).expect("preserved local upload"),
        b"S172-native-upload"
    );
    // Preflight every root before converting any placeholder. Incomplete files,
    // unsupported reparse points and scanning failures all refuse unregistration.
    for path in &paths {
        maintenance::check_materialized(path).expect("refuse incomplete fixture cleanup");
    }
    let mut reports = Vec::new();
    for path in &paths {
        let hashes = maintenance::revert_materialized(path)
            .expect("convert verified complete files without changing their identity or bytes");
        maintenance::unregister_materialized(path)
            .expect("unregister only after all placeholders became ordinary files");
        reports.push(serde_json::json!({"path":path,"preserved_hashes":hashes}));
    }
    std::fs::write(
        root.join("provider-cleanup.json"),
        serde_json::to_vec_pretty(&reports).unwrap(),
    )
    .expect("retain cleanup evidence");
}
