use super::*;
use crate::observer::fixtures::{FakeWorkTree, shared_fs};

fn limits(max: usize) -> ScanLimits {
    ScanLimits::new(max).expect("预算合法")
}

fn paths(report: &ScanReport) -> Vec<&str> {
    report.files.iter().map(|file| file.path.as_str()).collect()
}

#[test]
fn zero_budget_and_zero_window_are_rejected() {
    assert_eq!(ScanLimits::new(0), Err(InvalidScanLimits));
    assert_eq!(StabilityWindow::from_millis(0), Err(InvalidStabilityWindow));
    assert_eq!(StabilityWindow::DEFAULT.millis(), 1000);
}

#[test]
fn complete_scan_lists_regular_files_breadth_first_in_byte_order() {
    let fs = shared_fs();
    {
        let mut state = fs.borrow_mut();
        state.write("b.txt", b"b");
        state.write("a.txt", b"a");
        state.write("sub/z.txt", b"z");
        state.write("sub/deep/y.txt", b"y");
        state.symlinks.insert("link".to_owned());
        state.unrepresentable_in.insert("sub".to_owned(), 2);
    }
    let mut work = FakeWorkTree::new(&fs);
    let report = scan_tree(&mut work, limits(100), ScanCursor::from_root());
    assert!(report.completion.is_complete());
    assert_eq!(
        paths(&report),
        ["a.txt", "b.txt", "sub/z.txt", "sub/deep/y.txt"]
    );
    assert_eq!(report.entries_examined, 7);
    // 符号链接与不可表示的名字必须可见，不能静默消失。
    assert!(report.skipped.iter().any(|skipped| skipped.path == "link"
        && skipped.reason == SkipReason::UnsupportedKind(WorkEntryKind::Symlink)));
    assert!(
        report.skipped.iter().any(|skipped| skipped.path == "sub"
            && skipped.reason == SkipReason::UnrepresentableNames(2))
    );
}

#[test]
fn truncated_scan_is_inconclusive_and_resumes_without_repeating_or_losing_files() {
    let fs = shared_fs();
    {
        let mut state = fs.borrow_mut();
        for name in ["a", "b", "c", "d", "e"] {
            state.write(&format!("{name}.txt"), name.as_bytes());
        }
        state.write("sub/x.txt", b"x");
        state.write("sub/y.txt", b"y");
    }
    let mut work = FakeWorkTree::new(&fs);
    let mut seen: Vec<String> = Vec::new();
    let mut cursor = ScanCursor::from_root();
    let mut passes = 0;
    loop {
        passes += 1;
        assert!(passes < 20, "续扫必须推进");
        let report = scan_tree(&mut work, limits(3), cursor);
        assert!(report.entries_examined <= 3, "预算是硬上限");
        seen.extend(report.files.iter().map(|file| file.path.clone()));
        match report.completion {
            ScanCompletion::Complete => break,
            ScanCompletion::Inconclusive { resume, reasons } => {
                assert!(
                    reasons
                        .iter()
                        .any(|reason| matches!(reason, InconclusiveReason::BudgetExhausted { .. })),
                    "被截断必须明说，不能当成完整快照"
                );
                assert!(resume.pending_dirs() > 0);
                cursor = resume;
            }
        }
    }
    assert!(passes > 1, "预算 3 不可能一次扫完 8 个项");
    seen.sort();
    assert_eq!(
        seen,
        [
            "a.txt",
            "b.txt",
            "c.txt",
            "d.txt",
            "e.txt",
            "sub/x.txt",
            "sub/y.txt"
        ]
    );
}

#[test]
fn unreadable_directory_is_inconclusive_not_empty_and_resumes_after_fix() {
    let fs = shared_fs();
    {
        let mut state = fs.borrow_mut();
        state.write("ok/one.txt", b"1");
        state.write("locked/two.txt", b"2");
        state.unreadable_dirs.insert("locked".to_owned());
    }
    let mut work = FakeWorkTree::new(&fs);
    let report = scan_tree(&mut work, limits(100), ScanCursor::from_root());
    assert_eq!(paths(&report), ["ok/one.txt"]);
    let ScanCompletion::Inconclusive { resume, reasons } = report.completion else {
        panic!("读不了的目录不能得到完整结论");
    };
    assert!(reasons.iter().any(|reason| matches!(
        reason,
        InconclusiveReason::DirectoryUnreadable { path, .. } if path == "locked"
    )));
    assert_eq!(
        resume
            .dirs()
            .map(|dir| dir.path.as_str())
            .collect::<Vec<_>>(),
        ["locked"]
    );

    fs.borrow_mut().unreadable_dirs.clear();
    let second = scan_tree(&mut work, limits(100), resume);
    assert!(second.completion.is_complete());
    assert_eq!(paths(&second), ["locked/two.txt"]);
}

#[test]
fn unreadable_entry_is_retried_by_name_and_never_reported_absent() {
    let fs = shared_fs();
    {
        let mut state = fs.borrow_mut();
        state.write("a.txt", b"a");
        state.write("b.txt", b"b");
        state.write("c.txt", b"c");
        state.unreadable_files.insert("b.txt".to_owned());
    }
    let mut work = FakeWorkTree::new(&fs);
    let report = scan_tree(&mut work, limits(100), ScanCursor::from_root());
    assert_eq!(paths(&report), ["a.txt", "c.txt"]);
    let ScanCompletion::Inconclusive { resume, reasons } = report.completion else {
        panic!("读不了的文件不能得到完整结论");
    };
    assert!(reasons.iter().any(|reason| matches!(
        reason,
        InconclusiveReason::EntryUnreadable { path, .. } if path == "b.txt"
    )));
    let pending: Vec<_> = resume.dirs().collect();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].retry.contains("b.txt"));

    fs.borrow_mut().unreadable_files.clear();
    let second = scan_tree(&mut work, limits(100), resume);
    assert!(second.completion.is_complete());
    assert_eq!(paths(&second), ["b.txt"], "只重试失败项，不重复已枚举项");
}

#[test]
fn root_unavailable_stops_the_scan_without_any_file_or_deletion_conclusion() {
    let fs = shared_fs();
    fs.borrow_mut().write("a.txt", b"a");
    fs.borrow_mut().root_unavailable = true;
    let mut work = FakeWorkTree::new(&fs);
    let report = scan_tree(&mut work, limits(100), ScanCursor::from_root());
    assert!(report.root_unavailable());
    assert!(report.files.is_empty());
    let ScanCompletion::Inconclusive { resume, .. } = report.completion else {
        panic!("Root 不可用不能得到完整结论");
    };
    assert_eq!(
        resume,
        ScanCursor::from_root(),
        "原样保留起点，等 Root 恢复"
    );
}

#[test]
fn directory_deleted_after_listing_is_gone_not_unreadable() {
    let fs = shared_fs();
    {
        let mut state = fs.borrow_mut();
        state.write("keep.txt", b"k");
        state.write("racy/inner.txt", b"i");
        state.gone_dirs.insert("racy".to_owned());
    }
    let mut work = FakeWorkTree::new(&fs);
    let report = scan_tree(&mut work, limits(100), ScanCursor::from_root());
    // 已确认不存在的目录不会让扫描永远无法完成，也不会产生任何删除结论。
    assert!(report.completion.is_complete());
    assert_eq!(paths(&report), ["keep.txt"]);
}

fn stamp(ctime: i64, mtime: i64, bytes: u64) -> FileStamp {
    use crate::ports::object_store::FileKey;
    FileStamp {
        key: FileKey {
            device: 1,
            inode: 5,
            links: 1,
            bytes,
        },
        btime: Some((1, 0)),
        mtime: (mtime, 0),
        ctime: (ctime, 0),
    }
}

#[test]
fn first_sighting_waits_a_full_window_even_for_an_old_file() {
    let mut tracker = StabilityTracker::new(StabilityWindow::from_millis(500).expect("合法"));
    // mtime 很旧也不能当稳定证据。
    let old = stamp(1, 1, 3);
    assert_eq!(
        tracker.observe("a", old, 10_000),
        Stability::Waiting {
            quiet_ms: 0,
            remaining_ms: 500,
            changed: Vec::new()
        }
    );
    assert!(matches!(
        tracker.observe("a", old, 10_300),
        Stability::Waiting {
            quiet_ms: 300,
            remaining_ms: 200,
            ..
        }
    ));
    assert_eq!(tracker.observe("a", old, 10_500), Stability::Stable);
}

#[test]
fn any_fingerprint_change_restarts_the_window_including_preserved_mtime() {
    let mut tracker = StabilityTracker::new(StabilityWindow::from_millis(500).expect("合法"));
    let base = stamp(1, 1, 3);
    tracker.observe("a", base, 0);
    // 大小变化。
    let Stability::Waiting { changed, .. } = tracker.observe("a", stamp(1, 1, 4), 400) else {
        panic!("指纹变化必须重新等待");
    };
    assert_eq!(changed, ["size"]);
    // mtime 被保留、只有 ctime 变化：仍然重新计时。
    let Stability::Waiting { changed, .. } = tracker.observe("a", stamp(9, 1, 4), 800) else {
        panic!("ctime 变化必须重新等待");
    };
    assert_eq!(changed, ["ctime"]);
    assert!(matches!(
        tracker.observe("a", stamp(9, 1, 4), 1_200),
        Stability::Waiting {
            remaining_ms: 100,
            ..
        }
    ));
    assert_eq!(
        tracker.observe("a", stamp(9, 1, 4), 1_300),
        Stability::Stable
    );

    tracker.forget("a");
    assert!(tracker.is_empty());
    tracker.observe("b", base, 0);
    tracker.retain(|path| path == "keep");
    assert_eq!(tracker.len(), 0);
}
