use super::*;
use crate::commit::put::RootCommitGate;
use crate::objects::{ExpectedContent, ObjectLayout};
use crate::observer::fixtures::{
    FakeBlobs, FakeExternalStore, FakeWorkTree, SharedFs, digest_of, shared_fs,
};
use crate::observer::import::FileState;
use crate::observer::intents::OwnWriteIntent;
use crate::observer::scan::{ScanLimits, StabilityWindow};
use crate::test_support::{root_id, user_time};

const WINDOW_MS: u64 = 500;
const DEBOUNCE_MS: u64 = 100;
const METADATA_MS: u64 = 60_000;
const RETRY_MS: u64 = 5_000;
const REVIEW_MS: u64 = 10_000;

struct Harness {
    fs: SharedFs,
    work: FakeWorkTree,
    blobs: FakeBlobs,
    store: FakeExternalStore,
    gate: RootCommitGate,
    layout: ObjectLayout,
    driver: FollowDriver,
    now_ms: u64,
    scan_budget: usize,
}

impl Harness {
    fn new() -> Self {
        let fs = shared_fs();
        let observer = ExternalObserver::new(
            root_id(0x11),
            StabilityWindow::from_millis(WINDOW_MS).expect("合法"),
            "ext-follow",
        )
        .expect("前缀合法");
        let config = FollowConfig::new(
            HintLimits::new(64, DEBOUNCE_MS).expect("合法"),
            METADATA_MS,
            RETRY_MS,
            REVIEW_MS,
            10,
        )
        .expect("合法");
        Harness {
            work: FakeWorkTree::new(&fs),
            blobs: FakeBlobs::new(&fs),
            fs,
            store: FakeExternalStore::default(),
            gate: RootCommitGate::new(),
            layout: ObjectLayout::new("/state/objects"),
            driver: FollowDriver::new(observer, config),
            now_ms: 1_000,
            scan_budget: 1_000,
        }
    }

    fn hint(&mut self, hint: WatchHint) {
        self.driver.push_hint(hint, self.now_ms);
    }

    fn changed(&mut self, path: &str) {
        self.hint(WatchHint::Changed(path.to_owned()));
    }

    /// 时间推进 `advance_ms` 后执行一步。
    async fn step(&mut self, advance_ms: u64) -> StepReport {
        self.now_ms += advance_ms;
        let limits =
            PassLimits::new(ScanLimits::new(self.scan_budget).expect("合法"), 100).expect("合法");
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
        self.driver.step(&mut ports, limits, clock).await
    }

    /// 启动补扫并让现有文件都被采纳。
    async fn adopt_existing(&mut self) {
        let first = self.step(0).await;
        assert_eq!(
            first.full_scan.as_ref().map(|run| run.cause),
            Some(FullScanCause::Startup)
        );
        self.step(WINDOW_MS).await;
    }

    /// 线索 → 去抖 → 稳定窗口，逐步推进直到采纳（最多 6 步）。
    async fn follow_hints_until_adopted(&mut self) {
        for _ in 0..6 {
            self.step(DEBOUNCE_MS).await;
            self.step(WINDOW_MS).await;
        }
    }
}

fn adopted(report: &StepReport, path: &str) -> bool {
    all_outcomes(report)
        .iter()
        .any(|outcome| outcome.path == path && matches!(outcome.state, FileState::Adopted { .. }))
}

fn all_outcomes(report: &StepReport) -> Vec<&crate::observer::import::FileOutcome> {
    let mut all = Vec::new();
    if let Some(run) = &report.full_scan {
        all.extend(run.report.outcomes.iter());
    }
    if let Some(hinted) = &report.hinted {
        all.extend(hinted.outcomes.iter());
    }
    if let Some(review) = &report.review {
        all.extend(review.outcomes.iter());
    }
    all
}

#[tokio::test]
async fn startup_always_runs_a_full_scan_before_trusting_any_event() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"before restart");
    harness.hint(WatchHint::Changed("a.txt".to_owned()));
    let first = harness.step(0).await;
    let run = first.full_scan.expect("启动必须全量补扫");
    assert_eq!(run.cause, FullScanCause::Startup);
    assert!(run.report.scan_complete);
    let second = harness.step(WINDOW_MS).await;
    assert!(adopted(&second, "a.txt"));
    assert_eq!(harness.store.versions_of("a.txt").len(), 1);
}

#[tokio::test]
async fn rename_over_final_content_is_captured_and_the_temp_name_is_never_adopted() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("doc.txt", b"v1");
    harness.adopt_existing().await;
    assert_eq!(harness.store.versions_of("doc.txt").len(), 1);

    // 编辑器：写临时文件 → rename-over 目标。线索里既有临时名也有最终名。
    harness
        .fs
        .borrow_mut()
        .write(".doc.txt.swp", b"final content");
    harness.changed(".doc.txt.swp");
    harness.fs.borrow_mut().files.remove(".doc.txt.swp");
    harness.fs.borrow_mut().replace("doc.txt", b"final content");
    harness.changed("doc.txt");

    harness.follow_hints_until_adopted().await;
    let versions = harness.store.versions_of("doc.txt");
    assert_eq!(versions.len(), 2, "rename-over 的最终内容必须成为新版本");
    assert_eq!(versions[1].digest, digest_of(b"final content"));
    assert!(
        harness.store.versions_of(".doc.txt.swp").is_empty(),
        "临时名从未被采纳，也没有删除结论"
    );
}

#[tokio::test]
async fn injected_overflow_triggers_an_immediate_full_scan_that_finds_unhinted_changes() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"one");
    harness.fs.borrow_mut().write("dir/b.txt", b"two");
    harness.adopt_existing().await;

    // 这些变化的事件全部丢失（没有任何线索）；随后收到溢出。
    harness.fs.borrow_mut().write("a.txt", b"one changed");
    harness.fs.borrow_mut().write("dir/new.txt", b"brand new");
    harness.hint(WatchHint::Overflow);
    let immediate = harness.step(1).await; // 远小于去抖时间：溢出不等去抖
    let run = immediate.full_scan.expect("溢出立即补扫");
    assert_eq!(run.cause, FullScanCause::QueueOverflow);
    assert!(
        harness.driver.hints().is_empty(),
        "补扫取走了溢出标记，没有残留"
    );

    harness.step(WINDOW_MS).await;
    assert_eq!(harness.store.versions_of("a.txt").len(), 2);
    assert_eq!(harness.store.versions_of("dir/new.txt").len(), 1);
}

#[tokio::test]
async fn mtime_preserving_rewrite_is_found_by_the_periodic_metadata_scan_without_any_event() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"original");
    harness.adopt_existing().await;

    harness
        .fs
        .borrow_mut()
        .write_preserving_mtime("a.txt", b"rewritten!");
    // 没有事件；周期未到时什么都不做。
    let idle = harness.step(METADATA_MS / 2).await;
    assert!(idle.full_scan.is_none() && idle.hinted.is_none());

    let periodic = harness.step(METADATA_MS / 2).await;
    assert_eq!(
        periodic.full_scan.as_ref().map(|run| run.cause),
        Some(FullScanCause::Periodic)
    );
    harness.step(WINDOW_MS).await;
    let versions = harness.store.versions_of("a.txt");
    assert_eq!(versions.len(), 2, "ctime 变化暴露了保留 mtime 的改写");
    assert_eq!(versions[1].digest, digest_of(b"rewritten!"));
}

#[tokio::test]
async fn content_review_finds_a_rewrite_whose_fingerprint_did_not_change() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"AAAA");
    harness.fs.borrow_mut().write("b.txt", b"BBBB");
    harness.adopt_existing().await;

    // 内容变了，但大小、mtime、ctime、inode 全部不变（元数据被还原或时间戳粒度吞掉了变化）。
    harness
        .fs
        .borrow_mut()
        .files
        .get_mut("a.txt")
        .expect("存在")
        .data = b"ZZZZ".to_vec();
    let meta_only = harness.step(1_000).await;
    assert!(
        meta_only.full_scan.is_none() && meta_only.review.is_none() && adopted_nothing(&meta_only),
        "元数据看不出变化"
    );
    assert_eq!(harness.store.versions_of("a.txt").len(), 1);

    let review_step = harness.step(REVIEW_MS).await;
    let review = review_step.review.expect("复核到期");
    assert_eq!(review.content_changed, vec!["a.txt".to_owned()]);
    assert!(review.reviewed >= 2);
    harness.step(WINDOW_MS).await;
    let versions = harness.store.versions_of("a.txt");
    assert_eq!(versions.len(), 2, "内容复核最终发现了变化");
    assert_eq!(versions[1].digest, digest_of(b"ZZZZ"));
    assert_eq!(
        harness.store.versions_of("b.txt").len(),
        1,
        "未变的文件不产生版本"
    );
}

fn adopted_nothing(report: &StepReport) -> bool {
    all_outcomes(report)
        .iter()
        .all(|outcome| !matches!(outcome.state, FileState::Adopted { .. }))
}

#[tokio::test]
async fn content_review_walks_the_adopted_set_in_batches_and_wraps() {
    let mut harness = Harness::new();
    for index in 0..25 {
        harness
            .fs
            .borrow_mut()
            .write(&format!("f{index:02}.txt"), b"x");
    }
    harness.adopt_existing().await;
    let mut reviewed = 0;
    let mut wrapped = false;
    for _ in 0..3 {
        let step = harness.step(REVIEW_MS).await;
        let review = step.review.expect("每个间隔一批");
        assert!(review.reviewed <= 10, "每批有上限");
        reviewed += review.reviewed;
        wrapped |= review.wrapped;
    }
    assert!(wrapped, "25 个文件、每批 10 个，三批内必须走完一圈");
    assert_eq!(reviewed, 25);
}

#[tokio::test]
async fn permission_failure_is_retried_and_never_becomes_a_deletion() {
    let mut harness = Harness::new();
    harness
        .fs
        .borrow_mut()
        .write("locked/secret.txt", b"keep me");
    harness.fs.borrow_mut().write("open.txt", b"open");
    harness.adopt_existing().await;
    let versions_before = harness.store.versions.len();
    let events_before = harness.store.events.len();

    harness
        .fs
        .borrow_mut()
        .unreadable_dirs
        .insert("locked".to_owned());
    harness.hint(WatchHint::Gap {
        path: "locked".to_owned(),
        detail: "permission_denied".to_owned(),
    });
    let hinted = harness.step(DEBOUNCE_MS).await;
    assert!(
        hinted.hinted.as_ref().is_some_and(|h| h.absent.is_empty()),
        "读不了的目录不是“不存在”"
    );

    // 周期扫描遇到读不了的目录：不完整，进入重试，绝不产生删除。
    let periodic = harness.step(METADATA_MS).await;
    let run = periodic.full_scan.expect("周期扫描");
    assert!(!run.report.scan_complete);
    assert!(run.report.inconclusive.iter().any(|reason| matches!(
        reason,
        InconclusiveReason::DirectoryUnreadable { path, .. } if path == "locked"
    )));
    assert!(
        harness
            .driver
            .observer()
            .history_protected("locked/secret.txt")
    );
    assert_eq!(harness.store.versions.len(), versions_before);
    assert_eq!(
        harness.store.events.len(),
        events_before,
        "没有删除事件，也没有新事件"
    );

    let retry = harness.step(RETRY_MS).await;
    assert_eq!(
        retry.full_scan.as_ref().map(|run| run.cause),
        Some(FullScanCause::Retry)
    );

    // 权限恢复并在目录里改了文件：重试扫描找到它。
    harness.fs.borrow_mut().unreadable_dirs.clear();
    harness
        .fs
        .borrow_mut()
        .write("locked/secret.txt", b"edited after unlock");
    harness.step(RETRY_MS).await;
    harness.step(WINDOW_MS).await;
    assert_eq!(harness.store.versions_of("locked/secret.txt").len(), 2);
}

#[tokio::test]
async fn unreadable_hinted_file_is_reported_not_absent_and_not_forgotten() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"one");
    harness.adopt_existing().await;
    harness
        .fs
        .borrow_mut()
        .unreadable_files
        .insert("a.txt".to_owned());
    harness.changed("a.txt");
    let step = harness.step(DEBOUNCE_MS).await;
    let hinted = step.hinted.expect("线索观察");
    assert_eq!(hinted.unreadable.len(), 1);
    assert!(hinted.absent.is_empty());
    assert!(harness.driver.observer().history_protected("a.txt"));
}

#[tokio::test]
async fn own_write_is_ignored_by_identity_and_content_but_a_user_edit_in_the_same_moment_is_not() {
    let mut harness = Harness::new();
    harness.adopt_existing().await;

    // 服务写入 own.txt：登记意图（inode + 内容），随后文件出现。
    harness.fs.borrow_mut().write("own.txt", b"service content");
    let inode = harness.fs.borrow().files["own.txt"].inode;
    harness
        .driver
        .observer_mut()
        .expect_own_write(
            "own.txt",
            OwnWriteIntent {
                device: 1,
                inode,
                content: ExpectedContent::new(digest_of(b"service content"), 15),
            },
        )
        .expect("登记");
    harness.changed("own.txt");
    harness.follow_hints_until_adopted().await;
    assert!(
        harness.store.versions_of("own.txt").is_empty(),
        "自己的写入不被当成外部采纳"
    );

    // 同一时刻（时钟没有任何推进）用户原地改了同一个文件：身份相同但内容不同 → 外部修改。
    harness
        .fs
        .borrow_mut()
        .write("own.txt", b"user edit right after");
    harness.changed("own.txt");
    harness.follow_hints_until_adopted().await;
    let versions = harness.store.versions_of("own.txt");
    assert_eq!(versions.len(), 1, "紧随其后的用户修改不能被时间窗口吞掉");
    assert_eq!(versions[0].digest, digest_of(b"user edit right after"));
    assert_eq!(
        harness.driver.observer().own_write_intents(),
        0,
        "意图已作废"
    );
}

#[tokio::test]
async fn user_rename_over_with_identical_content_is_not_mistaken_for_the_service_write() {
    let mut harness = Harness::new();
    harness.adopt_existing().await;
    // 服务登记了一个尚未落地的发布（暂存文件的 inode 为 9999）。
    harness
        .driver
        .observer_mut()
        .expect_own_write(
            "p.txt",
            OwnWriteIntent {
                device: 1,
                inode: 9_999,
                content: ExpectedContent::new(digest_of(b"same bytes"), 10),
            },
        )
        .expect("登记");
    // 用户用 rename-over 放了内容相同、inode 不同的文件。
    harness.fs.borrow_mut().write("p.txt", b"same bytes");
    harness.changed("p.txt");
    harness.follow_hints_until_adopted().await;
    assert_eq!(
        harness.store.versions_of("p.txt").len(),
        1,
        "inode 不同就不是本服务的写入"
    );
    assert_eq!(
        harness.driver.observer().own_write_intents(),
        1,
        "服务的发布还没发生，意图保留"
    );
}

#[tokio::test]
async fn subtree_hint_scans_a_new_directory_whose_events_were_never_seen() {
    let mut harness = Harness::new();
    harness.adopt_existing().await;
    harness.fs.borrow_mut().write("newdir/inner/a.txt", b"a");
    harness.fs.borrow_mut().write("newdir/b.txt", b"b");
    harness.hint(WatchHint::Subtree("newdir".to_owned()));
    harness.follow_hints_until_adopted().await;
    assert_eq!(harness.store.versions_of("newdir/inner/a.txt").len(), 1);
    assert_eq!(harness.store.versions_of("newdir/b.txt").len(), 1);
}

#[tokio::test]
async fn truncated_subtree_scan_hands_over_to_a_full_scan() {
    let mut harness = Harness::new();
    harness.adopt_existing().await;
    for index in 0..8 {
        harness
            .fs
            .borrow_mut()
            .write(&format!("big/f{index}.txt"), b"x");
    }
    harness.scan_budget = 3;
    harness.hint(WatchHint::Subtree("big".to_owned()));
    let hinted = harness.step(DEBOUNCE_MS).await;
    assert!(hinted.hinted.expect("线索观察").subtree_truncated);
    let next = harness.step(1).await;
    assert_eq!(
        next.full_scan.as_ref().map(|run| run.cause),
        Some(FullScanCause::SubtreeTruncated)
    );
    // 预算仍小：之后继续续扫直到走完，全部文件最终被采纳。
    for _ in 0..8 {
        harness.step(WINDOW_MS).await;
    }
    for index in 0..8 {
        assert_eq!(
            harness
                .store
                .versions_of(&format!("big/f{index}.txt"))
                .len(),
            1,
            "big/f{index}.txt 最终被采纳"
        );
    }
}

#[tokio::test]
async fn root_unavailable_stops_everything_without_deletion_and_recovers_with_a_scan() {
    let mut harness = Harness::new();
    harness.fs.borrow_mut().write("a.txt", b"one");
    harness.adopt_existing().await;
    harness.fs.borrow_mut().root_unavailable = true;
    harness.hint(WatchHint::Overflow);
    let down = harness.step(1).await;
    assert!(down.full_scan.expect("补扫").report.root_unavailable);
    assert!(harness.driver.observer().history_protected("a.txt"));

    harness.fs.borrow_mut().root_unavailable = false;
    harness.fs.borrow_mut().write("a.txt", b"two");
    let back = harness.step(RETRY_MS).await;
    assert_eq!(
        back.full_scan.as_ref().map(|run| run.cause),
        Some(FullScanCause::Retry)
    );
    harness.step(WINDOW_MS).await;
    assert_eq!(harness.store.versions_of("a.txt").len(), 2);
}
