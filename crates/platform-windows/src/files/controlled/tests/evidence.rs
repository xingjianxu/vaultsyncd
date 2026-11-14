use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) fn snapshot_names(lab: &Lab) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(lab.base.join("state"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("snapshot-"))
        .collect();
    names.sort();
    names
}

fn observe(lab: &Lab) -> Evidence {
    lab.files.observe_evidence("a.txt").unwrap().unwrap()
}

#[test]
fn native_read_only_observation_reuses_generation_without_creating_snapshots() {
    let lab = Lab::new();
    let mut bytes = vec![0x51; 2 * 256 * 1024 + 17];
    bytes[256 * 1024] = 0x73;
    *bytes.last_mut().unwrap() = 0x9a;
    std::fs::write(lab.path(), &bytes).unwrap();
    let first = observe(&lab);
    assert_eq!(first.digest, digest(&bytes));
    assert_eq!(first.size, bytes.len() as u64);
    for _ in 0..3 {
        assert_eq!(observe(&lab), first);
    }
    assert!(snapshot_names(&lab).is_empty());
    assert_eq!(
        std::fs::read_dir(lab.base.join("state")).unwrap().count(),
        0
    );

    // Snapshot consumers still receive immutable bytes with the same generation history.
    let snapshot = lab.observe();
    assert_eq!(*snapshot.evidence(), first);
    let names = snapshot_names(&lab);
    assert_eq!(names.len(), 1);
    assert_eq!(observe(&lab), first);
    bytes.fill(0x82);
    std::fs::write(lab.path(), &bytes).unwrap();
    let second = observe(&lab);
    assert_eq!(second.generation, first.generation + 1);
    assert_eq!(second.digest, digest(&bytes));
    assert_eq!(snapshot.read_at(256 * 1024, 1).unwrap(), [0x73]);
    assert_eq!(snapshot_names(&lab), names);
}

#[test]
fn native_read_only_observation_hashes_same_size_and_restored_mtime() {
    let lab = Lab::new();
    std::fs::write(lab.path(), b"initial").unwrap();
    let first = observe(&lab);
    let mtime = std::fs::metadata(lab.path()).unwrap().modified().unwrap();
    std::fs::write(lab.path(), b"changed").unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(lab.path())
        .unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    drop(file);
    assert_eq!(
        std::fs::metadata(lab.path()).unwrap().modified().unwrap(),
        mtime
    );
    let restarted = WindowsFiles::open(&lab.base.join("vault"), &lab.base.join("state")).unwrap();
    restarted.seed("a.txt", first.clone()).unwrap();
    let second = restarted.observe_evidence("a.txt").unwrap().unwrap();
    assert_eq!(second.identity(), first.identity());
    assert_eq!(second.size, first.size);
    assert_eq!(second.generation, first.generation + 1);
    assert_ne!(second.digest, first.digest);
    assert_eq!(second.digest, digest(b"changed"));
    assert!(snapshot_names(&lab).is_empty());
}

#[test]
fn native_read_only_observation_detects_same_content_atomic_replacement() {
    let lab = Lab::new();
    std::fs::write(lab.path(), b"original").unwrap();
    let first = observe(&lab);
    let replacement = lab.base.join("vault/editor.tmp");
    std::fs::write(&replacement, b"original").unwrap();
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
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    assert_ne!(
        unsafe {
            ReplaceFileW(
                wide(&lab.path()).as_ptr(),
                wide(&replacement).as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0,
        "{}",
        io::Error::last_os_error()
    );
    let second = observe(&lab);
    assert_eq!(second.digest, first.digest);
    assert_ne!(second.file, first.file);
    assert_eq!(second.generation, first.generation + 1);
    assert!(
        lab.files
            .replace(
                OP,
                "a.txt",
                &lab.stage(),
                Some(&first),
                digest(b"remote-new")
            )
            .is_err()
    );
    assert_eq!(std::fs::read(lab.path()).unwrap(), b"original");
    assert!(snapshot_names(&lab).is_empty());
}

#[test]
fn native_read_only_observation_refuses_broken_oplock_without_advancing_generation() {
    let lab = Lab::new();
    let bytes = vec![0x51; 2 * 256 * 1024 + 17];
    std::fs::write(lab.path(), &bytes).unwrap();
    let first = observe(&lab);
    let path = lab.path();
    AFTER_OBSERVATION_CHUNK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            // Break the actual oplock between chunks, even without modifying file contents.
            let writer = std::fs::OpenOptions::new()
                .write(true)
                .share_mode(7)
                .open(path)
                .unwrap();
            drop(writer);
        }));
    });
    let error = lab.files.observe_evidence("a.txt").unwrap_err();
    assert!(error.to_string().contains("oplock broken"), "{error}");
    assert_eq!(observe(&lab), first);
    assert!(snapshot_names(&lab).is_empty());
}

#[test]
fn native_read_only_observation_cancellation_never_records_partial_evidence() {
    // Both a mid-file cancellation and cancellation after the last block must be refused.
    for size in [7, 2 * 256 * 1024 + 17] {
        let lab = Lab::new();
        std::fs::write(lab.path(), vec![0x51; size]).unwrap();
        let first = observe(&lab);
        std::fs::write(lab.path(), vec![0x72; size]).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        AFTER_OBSERVATION_CHUNK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || signal.store(true, Ordering::Relaxed)));
        });
        assert_eq!(
            lab.files
                .observe_evidence_cancellable("a.txt", &cancel)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        // A different complete observation advances from the last successful one, not the
        // cancelled hash. Replacing by the original bytes alone would hide a partial update.
        std::fs::write(lab.path(), vec![0x93; size]).unwrap();
        let next = observe(&lab);
        assert_eq!(next.generation, first.generation + 1);
        assert_eq!(next.digest, digest(&vec![0x93; size]));
        assert!(snapshot_names(&lab).is_empty());
    }
    let lab = Lab::new();
    std::fs::write(lab.path(), []).unwrap();
    let cancelled = AtomicBool::new(true);
    for path in ["a.txt", "absent.txt"] {
        assert_eq!(
            lab.files
                .observe_evidence_cancellable(path, &cancelled)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
    }
    assert_eq!(observe(&lab).generation, 1);
}

#[test]
fn native_read_only_observation_distinguishes_healthy_absence_from_unsafe_objects() {
    let lab = Lab::new();
    assert_eq!(lab.files.observe_evidence("a.txt").unwrap(), None);
    assert!(lab.files.observe_evidence("missing-parent/a.txt").is_err());
    assert!(lab.files.observe_evidence("../outside").is_err());
    std::fs::create_dir(lab.path()).unwrap();
    assert!(lab.files.observe_evidence("a.txt").is_err());
    std::fs::remove_dir(lab.path()).unwrap();
    std::fs::write(lab.path(), []).unwrap();
    let empty = observe(&lab);
    assert_eq!(empty.size, 0);
    assert_eq!(empty.digest, digest(b""));
    std::fs::hard_link(lab.path(), lab.base.join("vault/linked.txt")).unwrap();
    assert!(lab.files.observe_evidence("a.txt").is_err());
    assert!(snapshot_names(&lab).is_empty());
}

#[test]
fn native_prepared_publication_recovery_does_not_create_snapshots() {
    let lab = Lab::new();
    std::fs::write(lab.path(), b"original").unwrap();
    let before = observe(&lab);
    let staged = format!("publication-{OP}.data");
    std::fs::write(lab.base.join("state").join(&staged), b"remote-new").unwrap();
    let state = VaultHandle::open(&lab.base.join("state")).unwrap();
    let new = state.inspect(&staged).unwrap();
    let vault = VaultHandle::open(&lab.base.join("vault")).unwrap();
    let record = Publication {
        op: OP.into(),
        target: "a.txt".into(),
        before: Some(before),
        staged,
        backup: format!("preserved-{OP}.data"),
        digest: digest(b"remote-new"),
        size: new.size,
        new_volume: new.identity.volume,
        new_file: new.identity.file,
        vault_volume: vault.identity().volume,
        vault_file: vault.identity().file,
    };
    let journal = lab.base.join(format!("state/publication-{OP}.json"));
    std::fs::write(&journal, serde_json::to_vec(&record).unwrap()).unwrap();
    for _ in 0..2 {
        assert_eq!(
            lab.files.recover_publications().unwrap(),
            vec![RecoveryOutcome::Prepared(OP.into())]
        );
        assert!(snapshot_names(&lab).is_empty());
        assert_eq!(std::fs::read(lab.path()).unwrap(), b"original");
        assert_eq!(
            std::fs::read(lab.base.join("state").join(&record.staged)).unwrap(),
            b"remote-new"
        );
        assert!(journal.exists());
    }
}
