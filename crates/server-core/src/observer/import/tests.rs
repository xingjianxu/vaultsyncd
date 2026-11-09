use super::*;
use crate::observer::fixtures::{
    FakeBlobs, FakeExternalStore, FakeWorkTree, SharedFs, digest_of, shared_fs,
};
use crate::ports::external_store::ExternalStoreError;
use crate::ports::object_store::{BlobError, CopyFallback, CopyMethod};
use crate::test_support::{revision, user_time, vault_id};
use vaultsyncd_domain::version::Revision;

const WINDOW_MS: u64 = 500;

struct Harness {
    fs: SharedFs,
    work: FakeWorkTree,
    blobs: FakeBlobs,
    store: FakeExternalStore,
    gate: VaultCommitGate,
    layout: ObjectLayout,
    observer: ExternalObserver,
    now_ms: u64,
    scan_budget: usize,
    max_captures: usize,
}

impl Harness {
    fn new() -> Self {
        let fs = shared_fs();
        Harness {
            work: FakeWorkTree::new(&fs),
            blobs: FakeBlobs::new(&fs),
            fs,
            store: FakeExternalStore::default(),
            gate: VaultCommitGate::new(),
            layout: ObjectLayout::for_vault("/state/objects", crate::test_support::vault_id(0x0a)),
            observer: observer(),
            now_ms: 1_000,
            scan_budget: 1_000,
            max_captures: 100,
        }
    }

    /// 时间推进 `advance_ms` 后执行一轮。
    async fn pass(&mut self, advance_ms: u64) -> PassReport {
        self.now_ms += advance_ms;
        let limits = PassLimits::new(
            ScanLimits::new(self.scan_budget).expect("预算合法"),
            self.max_captures,
        )
        .expect("预算合法");
        let clock = PassClock {
            now_ms: self.now_ms,
            at: user_time(1_700_000_000 + i64::try_from(self.now_ms / 1000).expect("小")),
        };
        let mut ports = ObserverPorts {
            work: &mut self.work,
            blobs: &mut self.blobs,
            layout: &self.layout,
            gate: &mut self.gate,
            store: &mut self.store,
        };
        self.observer.run_pass(&mut ports, limits, clock).await
    }

    /// 首次看到（等待）→ 经过一个窗口（稳定并采纳）。
    async fn settle(&mut self) -> PassReport {
        self.pass(0).await;
        self.pass(WINDOW_MS).await
    }
}

fn observer() -> ExternalObserver {
    ExternalObserver::new(
        vault_id(0x11),
        StabilityWindow::from_millis(WINDOW_MS).expect("合法"),
        "ext-test",
    )
    .expect("前缀合法")
}

fn state_of<'a>(report: &'a PassReport, path: &str) -> &'a FileState {
    &report
        .outcomes
        .iter()
        .find(|outcome| outcome.path == path)
        .unwrap_or_else(|| panic!("本轮没有 {path} 的结局：{:?}", report.outcomes))
        .state
}

fn pending_of<'a>(report: &'a PassReport, path: &str) -> &'a PendingReason {
    match state_of(report, path) {
        FileState::Pending(reason) => reason,
        other => panic!("{path} 应当是待处理，实际 {other:?}"),
    }
}

#[tokio::test]
async fn ssh_created_file_becomes_a_version_and_event_after_the_stable_window() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("notes/a.txt", b"hello");

    // 第一次看到：只等待，不采集、不标已保护。
    let first = harness.pass(0).await;
    assert_eq!(
        *pending_of(&first, "notes/a.txt"),
        PendingReason::Waiting { quiet_ms: 0 }
    );
    assert!(!harness.observer.history_protected("notes/a.txt"));
    assert!(harness.store.versions.is_empty());
    assert_eq!(harness.blobs.stage_calls, 0, "窗口内不读内容");

    // 窗口未满仍然等待。
    let early = harness.pass(WINDOW_MS - 1).await;
    assert!(matches!(
        pending_of(&early, "notes/a.txt"),
        PendingReason::Waiting { quiet_ms } if *quiet_ms == WINDOW_MS - 1
    ));
    assert!(harness.store.versions.is_empty());

    // 窗口满：采集并采纳，版本与事件同时出现。
    let adopted = harness.pass(1).await;
    let FileState::Adopted {
        record,
        created,
        copy_method,
        ..
    } = state_of(&adopted, "notes/a.txt")
    else {
        panic!("窗口满应当被采纳：{:?}", adopted.outcomes);
    };
    assert!(*created);
    assert_eq!(*copy_method, CopyMethod::Reflink);
    assert_eq!(record.digest, digest_of(b"hello"));
    assert_eq!(record.size, 5);
    assert_eq!(harness.store.versions.len(), 1);
    assert_eq!(harness.store.events.len(), 1, "同一事务追加事件批次");
    assert_eq!(harness.store.events[0].1, "notes/a.txt");
    assert!(harness.observer.history_protected("notes/a.txt"));
    assert!(harness.observer.pending_reason("notes/a.txt").is_none());
    // 候选已固定成对象，字节与内容一致；工作文件没有被改写。
    let object_path = harness.store.versions[0].storage_path.clone();
    let stored = harness
        .blobs
        .objects
        .iter()
        .find(|(path, _)| path.ends_with(&object_path))
        .map(|(_, data)| data.clone())
        .expect("对象已发布");
    assert_eq!(stored, b"hello");
    assert_eq!(harness.fs.borrow().files["notes/a.txt"].data, b"hello");
    assert!(adopted.scan_complete);
    assert!(
        adopted.initial_import_complete,
        "整树采纳且无待处理才算完成"
    );
    assert!(harness.blobs.staged.is_empty(), "不留暂存副本");
}

#[tokio::test]
async fn ssh_edited_file_produces_a_new_version_with_advanced_revision() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"v1");
    harness.settle().await;
    assert_eq!(harness.store.versions_of("a.txt").len(), 1);

    harness.fs.borrow_mut().write("a.txt", b"version two");
    let waiting = harness.pass(10).await;
    assert!(matches!(
        pending_of(&waiting, "a.txt"),
        PendingReason::Waiting { quiet_ms: 0 }
    ));
    let adopted = harness.pass(WINDOW_MS).await;
    let FileState::Adopted {
        record, created, ..
    } = state_of(&adopted, "a.txt")
    else {
        panic!("改内容应产生新版本：{:?}", adopted.outcomes);
    };
    assert!(!created);
    assert_eq!(record.revision, revision(2));
    let versions = harness.store.versions_of("a.txt");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].digest, digest_of(b"v1"));
    assert_eq!(versions[1].digest, digest_of(b"version two"));
    assert_eq!(versions[0].revision, revision(1));
    assert_eq!(versions[1].revision, revision(2));
    assert!(versions[0].seq < versions[1].seq, "事件批次按提交顺序推进");
    assert_eq!(harness.store.events.len(), 2);
    // 旧版本对象仍在（不可变保留）。
    assert!(harness.blobs.objects.len() >= 2);
}

#[tokio::test]
async fn continuous_writes_keep_requeueing_and_never_protect_or_commit_partial_content() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("log.bin", b"0");
    harness.pass(0).await;
    for round in 1..=6u8 {
        // 每一轮都在窗口内又写了一次。
        harness
            .fs
            .borrow_mut()
            .write("log.bin", &vec![b'x'; usize::from(round) + 1]);
        let report = harness.pass(WINDOW_MS / 2).await;
        let PendingReason::Waiting { quiet_ms } = pending_of(&report, "log.bin") else {
            panic!("持续写入必须保持等待");
        };
        assert_eq!(*quiet_ms, 0, "每次变化都重新计时");
        assert!(!harness.observer.history_protected("log.bin"));
    }
    assert!(harness.store.versions.is_empty(), "不得传播部分片段");
    assert_eq!(harness.blobs.stage_calls, 0);
    assert!(!harness.observer.initial_import_complete());

    // 写入者停下后，才采纳最终内容。
    harness.pass(0).await;
    let done = harness.pass(WINDOW_MS).await;
    assert!(matches!(
        state_of(&done, "log.bin"),
        FileState::Adopted { .. }
    ));
    assert_eq!(harness.store.versions.len(), 1);
    assert_eq!(harness.store.versions[0].bytes, 7);
}

#[tokio::test]
async fn write_after_clone_before_after_check_requeues_without_adopting() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"first");
    harness.pass(0).await;
    // 候选建立之后、采集后复核之前，外部程序又写了一次。
    harness.blobs.during_stage = Some(Box::new(|fs| fs.write("a.txt", b"second write")));
    let report = harness.pass(WINDOW_MS).await;
    assert!(matches!(
        pending_of(&report, "a.txt"),
        PendingReason::Requeued(RequeueReason::ChangedDuringCapture { .. })
    ));
    assert!(harness.store.versions.is_empty());
    assert!(!harness.observer.history_protected("a.txt"));
    assert!(!report.initial_import_complete);

    // 重排队后重新等待一个完整窗口，最终内容被采集。
    harness.pass(0).await;
    let done = harness.pass(WINDOW_MS).await;
    assert!(matches!(
        state_of(&done, "a.txt"),
        FileState::Adopted { .. }
    ));
    assert_eq!(harness.store.versions[0].digest, digest_of(b"second write"));
}

#[tokio::test]
async fn growing_source_during_copy_requeues_instead_of_taking_a_short_candidate() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"abc");
    harness.pass(0).await;
    harness.blobs.grow_by = 4;
    let report = harness.pass(WINDOW_MS).await;
    assert!(matches!(
        pending_of(&report, "a.txt"),
        PendingReason::Requeued(RequeueReason::SourceChangedDuringCopy)
    ));
    assert!(harness.store.versions.is_empty());
    assert!(
        harness.blobs.objects.is_empty(),
        "长度不符的候选不得成为对象"
    );
    assert!(harness.blobs.staged.is_empty());
}

#[tokio::test]
async fn rewrite_that_preserves_mtime_is_still_noticed_through_ctime() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"aaaa");
    harness.settle().await;
    harness
        .fs
        .borrow_mut()
        .write_preserving_mtime("a.txt", b"bbbb");
    let report = harness.pass(10).await;
    assert!(matches!(
        pending_of(&report, "a.txt"),
        PendingReason::Waiting { quiet_ms: 0 }
    ));
    harness.pass(WINDOW_MS).await;
    let versions = harness.store.versions_of("a.txt");
    assert_eq!(versions.len(), 2, "相同大小、相同 mtime 的改写也发现");
    assert_eq!(versions[1].digest, digest_of(b"bbbb"));
}

#[tokio::test]
async fn rename_over_replacement_is_captured_as_the_final_content() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("doc.txt", b"old");
    harness.settle().await;
    harness.fs.borrow_mut().replace("doc.txt", b"new content");
    harness.pass(10).await;
    harness.pass(WINDOW_MS).await;
    let versions = harness.store.versions_of("doc.txt");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[1].digest, digest_of(b"new content"));
}

#[tokio::test]
async fn touch_without_content_change_creates_no_new_version() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"same");
    harness.settle().await;
    harness.fs.borrow_mut().touch("a.txt");
    harness.pass(10).await;
    let report = harness.pass(WINDOW_MS).await;
    assert!(matches!(
        state_of(&report, "a.txt"),
        FileState::UnchangedContent
    ));
    assert_eq!(harness.store.versions_of("a.txt").len(), 1);
}

#[tokio::test]
async fn restart_recognizes_known_content_and_does_not_duplicate_versions() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"same");
    harness.settle().await;
    // 进程重启：账本为空，库里仍有版本。
    harness.observer = observer();
    harness.settle().await;
    assert_eq!(harness.store.versions.len(), 1);
    assert!(harness.observer.history_protected("a.txt"));
}

#[tokio::test]
async fn unobserved_transient_content_is_not_claimed_as_preserved() {
    // 两轮观察之间 A 被 B 覆盖：A 从未被观察，只有最终稳定的 B 有版本。
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"transient A");
    harness.fs.borrow_mut().write("a.txt", b"final B");
    harness.settle().await;
    let versions = harness.store.versions_of("a.txt");
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].digest, digest_of(b"final B"));
    assert!(
        harness
            .store
            .versions
            .iter()
            .all(|version| version.digest != digest_of(b"transient A"))
    );
}

#[tokio::test]
async fn cross_device_fallback_still_adopts_verified_copy() {
    let mut harness = Harness::new();
    harness.blobs.fallback = Some(CopyFallback::CrossDevice);
    harness.fs.borrow_mut().write("a.txt", b"data");
    harness.settle().await;
    assert_eq!(harness.store.versions.len(), 1);
    // 方式与原因在端口结果里可见（不被解释成克隆）。
    let staged = harness
        .blobs
        .stage_working_copy(
            std::path::Path::new("a.txt"),
            std::path::Path::new("/state/objects/staging/probe"),
        )
        .expect("再次暂存");
    assert_eq!(staged.method, CopyMethod::VerifiedCopy);
    assert_eq!(staged.fallback, Some(CopyFallback::CrossDevice));
}

use crate::ports::object_store::ObjectBlobPort;

#[tokio::test]
async fn unreadable_file_is_a_failure_not_a_deletion_and_is_retried() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"secret");
    harness.fs.borrow_mut().write("b.txt", b"ok");
    harness.pass(0).await;
    // 稳定后权限被收回。
    harness
        .fs
        .borrow_mut()
        .unreadable_files
        .insert("a.txt".to_owned());
    let report = harness.pass(WINDOW_MS).await;
    assert!(
        matches!(state_of(&report, "b.txt"), FileState::Adopted { .. }),
        "一个文件读不了不影响别的文件"
    );
    // 扫描阶段 observe 失败：不是 Absent，也没有 Vanished 结局。
    assert!(!report.scan_complete);
    assert!(report.inconclusive.iter().any(|reason| matches!(
        reason,
        InconclusiveReason::EntryUnreadable { path, .. } if path == "a.txt"
    )));
    assert!(harness.fs.borrow().files.contains_key("a.txt"));
    assert!(!harness.observer.history_protected("a.txt"));
    assert!(!report.initial_import_complete, "读不了就不能宣布导入完成");
    assert!(harness.observer.resume_cursor().is_some());

    // 权限恢复后续扫并采纳。
    harness.fs.borrow_mut().unreadable_files.clear();
    harness.pass(0).await;
    harness.pass(WINDOW_MS).await;
    assert!(harness.observer.history_protected("a.txt"));
}

#[tokio::test]
async fn capture_time_permission_failure_is_pending_failed_never_vanished() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"x");
    harness.pass(0).await;
    harness.blobs.stage_error = Some(BlobError::SourceUnreadable {
        detail: "Permission denied".to_owned(),
    });
    let report = harness.pass(WINDOW_MS).await;
    let PendingReason::Failed { code, .. } = pending_of(&report, "a.txt") else {
        panic!("读不了应是失败待重试");
    };
    assert_eq!(*code, "source_unreadable");
    assert!(!harness.observer.history_protected("a.txt"));
    assert!(harness.store.versions.is_empty());
    // 下一轮重试成功。
    let retry = harness.pass(10).await;
    assert!(matches!(
        state_of(&retry, "a.txt"),
        FileState::Adopted { .. }
    ));
}

#[tokio::test]
async fn vault_unavailable_captures_nothing_and_never_completes_the_import() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"x");
    harness.pass(0).await;
    harness.fs.borrow_mut().vault_unavailable = true;
    let report = harness.pass(WINDOW_MS).await;
    assert!(report.vault_unavailable);
    assert!(!report.scan_complete);
    assert!(report.outcomes.is_empty());
    assert_eq!(harness.blobs.stage_calls, 0);
    assert!(harness.store.versions.is_empty());
    assert!(!report.initial_import_complete);
    assert!(
        harness.fs.borrow().files.contains_key("a.txt"),
        "不推导删除"
    );
}

#[tokio::test]
async fn truncated_scan_continues_from_the_cursor_and_completes_only_at_the_end() {
    let mut harness = Harness::new();
    {
        let mut fs = harness.fs.borrow_mut();
        for name in ["a", "b", "c", "d"] {
            fs.write(&format!("{name}.txt"), name.as_bytes());
        }
    }
    harness.scan_budget = 2;
    let mut seen_incomplete = false;
    let mut completed = false;
    for _ in 0..12 {
        let report = harness.pass(WINDOW_MS).await;
        if !report.scan_complete {
            seen_incomplete = true;
            assert!(
                !report.initial_import_complete,
                "被截断的扫描不能宣布导入完成"
            );
            assert!(harness.observer.resume_cursor().is_some());
        }
        if report.initial_import_complete {
            completed = true;
            break;
        }
    }
    assert!(seen_incomplete && completed);
    assert_eq!(harness.store.versions.len(), 4);
}

#[tokio::test]
async fn capture_budget_defers_extra_stable_files_to_the_next_pass() {
    let mut harness = Harness::new();
    {
        let mut fs = harness.fs.borrow_mut();
        fs.write("a.txt", b"a");
        fs.write("b.txt", b"b");
    }
    harness.max_captures = 1;
    harness.pass(0).await;
    let first = harness.pass(WINDOW_MS).await;
    assert!(matches!(
        state_of(&first, "a.txt"),
        FileState::Adopted { .. }
    ));
    assert_eq!(
        *pending_of(&first, "b.txt"),
        PendingReason::Deferred(DeferReason::CaptureBudget)
    );
    assert!(!first.initial_import_complete);
    let second = harness.pass(1).await;
    assert!(matches!(
        state_of(&second, "b.txt"),
        FileState::Adopted { .. }
    ));
    assert!(second.initial_import_complete);
}

#[tokio::test]
async fn vanished_never_adopted_file_is_dropped_without_a_deletion_event() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("tmp.txt", b"short lived");
    harness.pass(0).await;
    harness.fs.borrow_mut().unlink("tmp.txt");
    let report = harness.pass(WINDOW_MS).await;
    assert!(report.outcomes.is_empty());
    assert!(harness.store.events.is_empty());
    assert_eq!(harness.observer.pending_count(), 0);
    assert!(!harness.observer.history_protected("tmp.txt"));
    assert!(report.initial_import_complete, "空树完整扫描且无待处理");
}

#[tokio::test]
async fn store_conflicts_and_failures_never_mark_history_protected() {
    // 比较并写失败 → 重排队。
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"one");
    harness.settle().await;
    harness.fs.borrow_mut().write("a.txt", b"two");
    harness.pass(10).await;
    harness.store.apply_error = Some(ExternalStoreError::RevisionChanged {
        expected: revision(1),
        actual: Revision::new(5).expect("合法"),
    });
    let report = harness.pass(WINDOW_MS).await;
    assert_eq!(
        *pending_of(&report, "a.txt"),
        PendingReason::Requeued(RequeueReason::BaselineMoved)
    );

    // 路径冲突 → 阻止。
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("d/x.txt", b"x");
    harness.store.directories.insert("d/x.txt".to_owned());
    let report = harness.settle().await;
    assert!(matches!(
        pending_of(&report, "d/x.txt"),
        PendingReason::Blocked(BlockReason::PathConflict { .. })
    ));
    assert!(!harness.observer.history_protected("d/x.txt"));

    // 存储失败 → 失败待重试。
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"x");
    harness.pass(0).await;
    harness.store.apply_error = Some(ExternalStoreError::Storage {
        detail: "database is locked".to_owned(),
    });
    let report = harness.pass(WINDOW_MS).await;
    assert!(matches!(
        pending_of(&report, "a.txt"),
        PendingReason::Failed {
            code: "external_store_failed",
            ..
        }
    ));
    assert!(!harness.observer.history_protected("a.txt"));
    assert!(!report.initial_import_complete);

    // 证据与请求不符 → 契约违反，不当成已采纳。
    let mut harness = Harness::new();
    harness.store.corrupt_record_digest = true;
    harness.fs.borrow_mut().write("a.txt", b"x");
    let report = harness.settle().await;
    assert!(matches!(
        pending_of(&report, "a.txt"),
        PendingReason::Failed {
            code: "store_contract_violation",
            ..
        }
    ));
    assert!(!harness.observer.history_protected("a.txt"));
}

#[tokio::test]
async fn busy_vault_defers_adoption_and_releases_the_ticket_afterwards() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"x");
    harness.pass(0).await;
    let ticket = harness.gate.acquire(vault_id(0x11)).expect("空闲");
    let report = harness.pass(WINDOW_MS).await;
    assert_eq!(
        *pending_of(&report, "a.txt"),
        PendingReason::Deferred(DeferReason::VaultBusy)
    );
    assert!(harness.store.versions.is_empty());
    harness.gate.release(ticket);
    let report = harness.pass(1).await;
    assert!(matches!(
        state_of(&report, "a.txt"),
        FileState::Adopted { .. }
    ));
    assert!(!harness.gate.is_held(vault_id(0x11)), "采纳后释放票据");
}

#[tokio::test]
async fn change_between_capture_and_commit_requeues_before_touching_the_store() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"before");
    harness.pass(0).await;
    // 采集后复核通过；采纳前外部又写入：由采纳前核对拦住（用 proposal 直接构造该路径）。
    let stamp = match harness.work.observe("a.txt").expect("可观察") {
        crate::ports::work_tree::WorkObservation::File(stamp) => stamp,
        other => panic!("{other:?}"),
    };
    let outcome = capture_external(
        &mut harness.work,
        &mut harness.blobs,
        &harness.layout,
        vault_id(0x11),
        "a.txt",
        &stamp,
        "direct-1",
    )
    .expect("采集成功");
    let CaptureOutcome::Proposal(proposal) = outcome else {
        panic!("应产生提案");
    };
    harness.fs.borrow_mut().write("a.txt", b"after!");
    let adopted = adopt_external(
        &mut harness.gate,
        &mut harness.store,
        &mut harness.work,
        &proposal,
        user_time(1_700_000_000),
    )
    .await
    .expect("核对不是失败");
    assert!(matches!(
        adopted,
        AdoptOutcome::Requeue(RequeueReason::ChangedBeforeCommit { .. })
    ));
    assert!(harness.store.writes.is_empty(), "指纹不符不触碰存储");
    assert!(!harness.gate.is_held(vault_id(0x11)));
}

#[test]
fn staging_prefix_and_capture_budget_are_validated() {
    assert!(matches!(
        ExternalObserver::new(vault_id(1), StabilityWindow::DEFAULT, "a/b"),
        Err(ObserverConfigError::StagingPrefix(_))
    ));
    assert_eq!(
        PassLimits::new(ScanLimits::new(1).expect("合法"), 0),
        Err(ObserverConfigError::NoCaptureBudget)
    );
}

#[tokio::test]
async fn permanently_unreadable_directory_does_not_starve_other_files() {
    let mut harness = Harness::new();
    {
        let mut fs = harness.fs.borrow_mut();
        fs.write("ok/one.txt", b"1");
        fs.write("locked/two.txt", b"2");
        fs.unreadable_dirs.insert("locked".to_owned());
    }
    harness.pass(0).await;
    // 续扫游标只剩读不了的目录，其余文件靠复查推进稳定窗口。
    let report = harness.pass(WINDOW_MS).await;
    assert!(matches!(
        state_of(&report, "ok/one.txt"),
        FileState::Adopted { .. }
    ));
    assert!(!report.scan_complete);
    assert!(!report.initial_import_complete);
    assert!(!harness.observer.history_protected("locked/two.txt"));
}
