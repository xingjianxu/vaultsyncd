//! 外部移动、删除与保护队列的端到端用例（fake 工作目录 + fake 采纳存储）。

use super::*;
use crate::observer::fixtures::{FakeBlobs, FakeExternalStore, FakeWorkTree, SharedFs, shared_fs};
use crate::observer::reconcile::{ProtectionReason, SHARE_MIN_TOTAL};
use crate::ports::external_store::ExternalStoreError;
use crate::test_support::{user_time, vault_id};

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
}

impl Harness {
    fn new(limits: ReconcileLimits) -> Self {
        let fs = shared_fs();
        Harness {
            work: FakeWorkTree::new(&fs),
            blobs: FakeBlobs::new(&fs),
            fs,
            store: FakeExternalStore::default(),
            gate: VaultCommitGate::new(),
            layout: ObjectLayout::new("/state/objects"),
            observer: observer(limits),
            now_ms: 1_000,
            scan_budget: 5_000,
        }
    }

    async fn pass(&mut self, advance_ms: u64) -> PassReport {
        self.now_ms += advance_ms;
        let limits = PassLimits::new(ScanLimits::new(self.scan_budget).expect("预算合法"), 200)
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

fn observer(limits: ReconcileLimits) -> ExternalObserver {
    ExternalObserver::new(
        vault_id(0x21),
        StabilityWindow::from_millis(WINDOW_MS).expect("合法"),
        "ext-reconcile",
    )
    .expect("前缀合法")
    .with_reconcile_limits(limits)
}

fn default_limits() -> ReconcileLimits {
    ReconcileLimits::DEFAULT
}

#[tokio::test]
async fn moved_files_keep_their_entry_ids_when_contents_are_identical() {
    let mut h = Harness::new(default_limits());
    h.fs.borrow_mut().write("a.txt", b"same");
    h.fs.borrow_mut().write("b.txt", b"same");
    h.settle().await;
    let entry_a = h.store.entry_of("a.txt").expect("a 已采纳");
    let entry_b = h.store.entry_of("b.txt").expect("b 已采纳");
    assert_ne!(entry_a, entry_b, "两个同内容文件是两个条目");
    let versions_before = h.store.versions.len();
    let object_writes_before = h.store.writes.len();

    // 两份内容完全相同的文件互换名字：只有 inode 能区分它们。
    h.fs.borrow_mut().rename("a.txt", "c.txt");
    h.fs.borrow_mut().rename("b.txt", "d.txt");
    let report = h.pass(0).await;

    assert!(report.reconcile.ran);
    assert_eq!(report.reconcile.health, ScanHealth::Complete);
    assert_eq!(report.reconcile.moves.len(), 2);
    // 移动不串身份：c.txt 仍是 a.txt 的条目，d.txt 仍是 b.txt 的条目。
    assert_eq!(h.store.entry_of("c.txt"), Some(entry_a));
    assert_eq!(h.store.entry_of("d.txt"), Some(entry_b));
    assert_eq!(h.store.entry_of("a.txt"), None);
    assert_eq!(h.store.entry_of("b.txt"), None);
    // 移动只改父目录与名称：没有新版本，也没有新内容采集。
    assert_eq!(h.store.versions.len(), versions_before);
    assert_eq!(h.store.writes.len(), object_writes_before);
    assert!(report.reconcile.deletions.is_empty());
    assert!(report.reconcile.protected.is_none());

    // 移动过的条目仍在保护范围内（基线跟着路径走）。
    assert!(h.observer.history_protected("c.txt"));
    assert!(!h.observer.history_protected("a.txt"));
}

#[tokio::test]
async fn identical_content_alone_never_pairs_a_new_inode_as_a_move() {
    let mut h = Harness::new(default_limits());
    h.fs.borrow_mut().write("x.txt", b"dup");
    h.settle().await;
    let entry_x = h.store.entry_of("x.txt").expect("已采纳");

    // 内容相同但 inode 不同：这是“删除 + 新增”，不是移动。
    h.fs.borrow_mut().unlink("x.txt");
    h.fs.borrow_mut().write("z.txt", b"dup");
    let report = h.pass(0).await;

    assert!(report.reconcile.moves.is_empty(), "内容相同不能当移动证据");
    assert_eq!(report.reconcile.deletions.len(), 1);
    assert_eq!(report.reconcile.deletions[0].path, "x.txt");
    assert_eq!(report.reconcile.deletions[0].entry, entry_x);
    // z.txt 走正常新增采集：新条目身份，不与已删除条目串号。
    h.settle().await;
    let entry_z = h.store.entry_of("z.txt").expect("z 已采纳");
    assert_ne!(entry_z, entry_x);
    // 被删除条目的历史仍可恢复：回收站记录了它的版本数。
    let trashed = h
        .store
        .trashed
        .iter()
        .find(|(path, _, _)| path == "x.txt")
        .expect("回收站有 x.txt");
    assert_eq!(trashed.1, entry_x);
    assert!(trashed.2 >= 1, "旧版本仍保留");
}

#[tokio::test]
async fn missing_file_in_a_complete_healthy_scan_becomes_a_deletion_event() {
    let mut h = Harness::new(default_limits());
    h.fs.borrow_mut().write("keep.txt", b"keep");
    h.fs.borrow_mut().write("gone.txt", b"gone");
    h.settle().await;
    let entry = h.store.entry_of("gone.txt").expect("已采纳");
    let events_before = h.store.events.len();

    h.fs.borrow_mut().unlink("gone.txt");
    let report = h.pass(0).await;

    assert_eq!(report.reconcile.health, ScanHealth::Complete);
    assert_eq!(report.reconcile.deletions.len(), 1);
    assert_eq!(report.reconcile.deletions[0].path, "gone.txt");
    assert_eq!(report.reconcile.deletions[0].entry, entry);
    assert!(report.reconcile.protected.is_none());
    assert_eq!(report.reconcile.held.len(), 0);
    assert_eq!(
        h.store.events.len(),
        events_before + 1,
        "追加了一次事件批次"
    );
    assert_eq!(h.store.entries.get("gone.txt"), None);
    assert!(h.observer.history_protected("keep.txt"));
    assert!(!h.observer.history_protected("gone.txt"));

    // 删除结论只做一次：下一轮不再重复提交。
    let again = h.pass(WINDOW_MS).await;
    assert!(again.reconcile.deletions.is_empty());
}

#[tokio::test]
async fn permission_failure_scan_never_deletes() {
    let mut h = Harness::new(default_limits());
    h.fs.borrow_mut().write("d/one.txt", b"1");
    h.fs.borrow_mut().write("d/two.txt", b"2");
    h.settle().await;
    assert_eq!(h.observer.adopted_count(), 2);

    h.fs.borrow_mut().unlink("d/one.txt");
    h.fs.borrow_mut().unreadable_dirs.insert("d".to_owned());
    let report = h.pass(0).await;

    assert_eq!(
        report.reconcile.health,
        ScanHealth::PermissionDenied {
            paths: vec!["d".to_owned()]
        }
    );
    assert!(!report.reconcile.ran, "不健康扫描不做核对");
    assert!(report.reconcile.deletions.is_empty());
    assert!(h.store.deletes.is_empty());
    assert!(h.observer.history_protected("d/one.txt"), "基线保持不动");

    // 权限恢复后照旧核对（这一轮才允许推导删除）。
    h.fs.borrow_mut().unreadable_dirs.clear();
    let recovered = h.pass(0).await;
    assert_eq!(recovered.reconcile.health, ScanHealth::Complete);
    assert_eq!(recovered.reconcile.deletions.len(), 1);
}

#[tokio::test]
async fn io_error_scan_never_deletes() {
    let mut h = Harness::new(default_limits());
    h.fs.borrow_mut().write("d/one.txt", b"1");
    h.settle().await;
    h.fs.borrow_mut().unlink("d/one.txt");
    h.fs.borrow_mut().io_error_dirs.insert("d".to_owned());
    let report = h.pass(0).await;
    assert!(
        matches!(report.reconcile.health, ScanHealth::IoError { .. }),
        "IO 失败与权限失败分开：{:?}",
        report.reconcile.health
    );
    assert!(!report.reconcile.ran);
    assert!(h.store.deletes.is_empty());
}

#[tokio::test]
async fn mount_changed_scan_is_unavailable_and_deletes_nothing() {
    let mut h = Harness::new(default_limits());
    h.fs.borrow_mut().write("one.txt", b"1");
    h.fs.borrow_mut().write("two.txt", b"2");
    h.settle().await;
    let events_before = h.store.events.len();

    // 卸载后路径下露出空目录：Vault 身份变了，扫描报 Vault 不可用。
    h.fs.borrow_mut().vault_unavailable = true;
    h.fs.borrow_mut().files.clear();
    let report = h.pass(0).await;

    assert!(report.vault_unavailable);
    assert!(matches!(
        report.reconcile.health,
        ScanHealth::MountChanged { .. }
    ));
    assert!(!report.reconcile.ran);
    assert!(report.reconcile.deletions.is_empty());
    assert!(h.store.deletes.is_empty());
    assert_eq!(h.store.events.len(), events_before, "零删除事件");
    assert_eq!(h.observer.adopted_count(), 2, "基线一条都不动");
}

#[tokio::test]
async fn truncated_scan_is_not_deletion_evidence() {
    let mut h = Harness::new(default_limits());
    h.fs.borrow_mut().write("d/one.txt", b"1");
    h.settle().await;
    h.fs.borrow_mut().unlink("d/one.txt");
    // 预算只够列 Vault 这一层：子目录 d 根本没被检查到。
    h.scan_budget = 1;
    let report = h.pass(0).await;
    assert!(matches!(
        report.reconcile.health,
        ScanHealth::Incomplete { .. }
    ));
    assert!(!report.reconcile.ran);
    assert!(h.store.deletes.is_empty());

    // 游标走完后的完整扫描才允许推导删除。
    h.scan_budget = 5_000;
    let mut report = h.pass(0).await;
    while !report.scan_complete {
        report = h.pass(0).await;
    }
    assert_eq!(report.reconcile.deletions.len(), 1);
}

#[tokio::test]
async fn mass_deletion_is_protected_with_baseline_directory_identity_and_counts() {
    let limits = ReconcileLimits::new(2, 10, 4).expect("合法");
    let mut h = Harness::new(limits);
    for name in ["a", "b", "c", "d"] {
        h.fs.borrow_mut()
            .write(&format!("{name}.txt"), name.as_bytes());
    }
    h.settle().await;
    let before = h.store.events.len();
    let inode_a = h.fs.borrow().files.get("a.txt").expect("存在").inode;
    let entry_a = h.store.entry_of("a.txt").expect("已采纳");

    for name in ["a", "b", "c"] {
        h.fs.borrow_mut().unlink(&format!("{name}.txt"));
    }
    let report = h.pass(0).await;

    let batch = report
        .reconcile
        .protected
        .expect("超过单轮上限应进保护队列");
    assert_eq!(
        batch.reason,
        ProtectionReason::TooManyDeletes { count: 3, limit: 2 }
    );
    assert_eq!(batch.len(), 3);
    assert_eq!(batch.protected_total, 4);
    // 原基线：路径、条目身份与内容摘要都在。
    let first = batch
        .entries
        .iter()
        .find(|entry| entry.path == "a.txt")
        .expect("a.txt 在批次里");
    assert_eq!(first.identity.inode, inode_a);
    assert_eq!(first.content.bytes, 1);
    assert_eq!(first.entry, entry_a);
    // 目录身份与计数：三个缺失都在 Vault 根目录下。
    assert_eq!(batch.directories.len(), 1);
    assert_eq!(batch.directories[0].path, "");
    assert_eq!(batch.directories[0].missing_count, 3);
    assert!(batch.directories[0].identity.is_some(), "实测到根目录身份");
    // 没有删除、没有事件，条目仍在保护范围内。
    assert!(report.reconcile.deletions.is_empty());
    assert_eq!(h.store.events.len(), before);
    assert_eq!(h.observer.protected_batches().len(), 1);
    assert!(h.observer.history_protected("a.txt"));

    // 人工复核后放行：下一轮允许按同一路径与条目身份传播这三个删除。
    let released = h.observer.release_protected(batch.id).expect("批次存在");
    assert_eq!(released.len(), 3);
    assert!(h.observer.protected_batches().is_empty());
    let applied = h.pass(0).await;
    assert_eq!(applied.reconcile.deletions.len(), 3);
    assert!(applied.reconcile.protected.is_none());
    assert_eq!(h.store.deletes.len(), 3);
}

#[tokio::test]
async fn protection_batch_shows_a_changed_directory_identity() {
    let limits = ReconcileLimits::new(2, 10, 4).expect("合法");
    let mut h = Harness::new(limits);
    for name in ["a", "b", "c"] {
        h.fs.borrow_mut().write(&format!("{name}.txt"), b"x");
    }
    h.settle().await;
    for name in ["a", "b", "c"] {
        h.fs.borrow_mut().unlink(&format!("{name}.txt"));
    }
    let first = h.pass(0).await;
    let first_identity = first.reconcile.protected.expect("批次 1").directories[0]
        .identity
        .expect("根目录身份");

    // 目录被换掉（例如卸载后露出另一个目录）：保护批次里的目录身份随之改变，供复核者判断。
    h.fs.borrow_mut().replace_dir("");
    let second = h.pass(0).await;
    let second_identity = second.reconcile.protected.expect("批次 2").directories[0]
        .identity
        .expect("根目录身份");
    assert_ne!(first_identity, second_identity);
    assert!(!first_identity.same_directory(&second_identity));
    assert!(h.store.deletes.is_empty(), "目录身份变了更不该自动删");
}

#[tokio::test]
async fn large_share_of_a_big_vault_is_protected_even_below_the_count_limit() {
    let limits = ReconcileLimits::DEFAULT;
    let mut h = Harness::new(limits);
    let total = SHARE_MIN_TOTAL + 4;
    for index in 0..total {
        h.fs.borrow_mut().write(&format!("f{index:02}.txt"), b"x");
    }
    h.settle().await;
    assert_eq!(h.observer.adopted_count(), total);

    // 10% of 20 = 2：删 3 个就超过占比（远低于单轮上限 32）。
    for index in 0..3 {
        h.fs.borrow_mut().unlink(&format!("f{index:02}.txt"));
    }
    let report = h.pass(0).await;
    let batch = report.reconcile.protected.expect("占比异常要保护");
    assert!(matches!(
        batch.reason,
        ProtectionReason::TooLargeShare { count: 3, .. }
    ));
    assert!(h.store.deletes.is_empty());
}

#[tokio::test]
async fn protection_queue_full_is_reported_instead_of_dropped() {
    let limits = ReconcileLimits::new(2, 10, 1).expect("合法");
    let mut h = Harness::new(limits);
    for name in ["a", "b", "c", "d", "e", "f"] {
        h.fs.borrow_mut().write(&format!("{name}.txt"), b"x");
    }
    h.settle().await;

    // 第一批 3 个缺失超过单轮上限 2 → 进保护队列（容量 1）。
    for name in ["a", "b", "c"] {
        h.fs.borrow_mut().unlink(&format!("{name}.txt"));
    }
    let first = h.pass(0).await;
    assert_eq!(
        first.reconcile.protected.as_ref().map(ProtectedBatch::len),
        Some(3)
    );

    // 又少了 3 个：队列已满。既不丢基线也不误删，逐个记成 held。
    for name in ["d", "e", "f"] {
        h.fs.borrow_mut().unlink(&format!("{name}.txt"));
    }
    let second = h.pass(0).await;
    assert!(second.reconcile.protected.is_none());
    assert!(second.reconcile.deletions.is_empty());
    assert_eq!(second.reconcile.held.len(), 6);
    assert!(
        second
            .reconcile
            .held
            .iter()
            .all(|held| held.code == "protection_queue_full")
    );
    assert_eq!(h.store.deletes.len(), 0);
    assert_eq!(h.observer.protected_batches().len(), 1);
}

#[tokio::test]
async fn store_failures_hold_the_candidate_without_losing_the_baseline() {
    let mut h = Harness::new(default_limits());
    h.fs.borrow_mut().write("one.txt", b"1");
    h.settle().await;
    h.fs.borrow_mut().unlink("one.txt");
    h.store.delete_error = Some(ExternalStoreError::Storage {
        detail: "数据库忙".to_owned(),
    });
    let report = h.pass(0).await;
    assert!(report.reconcile.deletions.is_empty());
    assert_eq!(report.reconcile.held.len(), 1);
    assert_eq!(report.reconcile.held[0].code, "external_store_failed");
    assert!(h.observer.history_protected("one.txt"));

    // 存储恢复后同一候选被正常提交。
    let after = h.pass(0).await;
    assert_eq!(after.reconcile.deletions.len(), 1);
    assert_eq!(after.reconcile.held.len(), 0);
}

#[tokio::test]
async fn a_stale_revision_holds_the_move_instead_of_forcing_it() {
    let mut h = Harness::new(default_limits());
    h.fs.borrow_mut().write("a.txt", b"same");
    h.settle().await;
    h.fs.borrow_mut().rename("a.txt", "b.txt");
    h.store.move_error = Some(ExternalStoreError::RevisionChanged {
        expected: vaultsyncd_domain::version::Revision::new(1).expect("合法"),
        actual: vaultsyncd_domain::version::Revision::new(4).expect("合法"),
    });
    let report = h.pass(0).await;
    assert!(report.reconcile.moves.is_empty());
    assert_eq!(report.reconcile.held[0].code, "revision_changed");
    assert!(h.observer.history_protected("a.txt"), "基线还在原路径");

    let after = h.pass(0).await;
    assert_eq!(after.reconcile.moves.len(), 1);
    assert_eq!(after.reconcile.moves[0].from, "a.txt");
    assert_eq!(after.reconcile.moves[0].to, "b.txt");
}

#[tokio::test]
async fn swapped_identical_files_are_held_instead_of_rebinding_entries() {
    let mut h = Harness::new(default_limits());
    h.fs.borrow_mut().write("left.txt", b"same bytes");
    h.fs.borrow_mut().write("right.txt", b"same bytes");
    h.settle().await;
    let left_entry = h.store.entry_of("left.txt").expect("已采纳");
    let right_entry = h.store.entry_of("right.txt").expect("已采纳");
    let versions_before = h.store.versions.len();

    // 互换名字：两条路径都还在，但里面的文件对象换了位置。
    h.fs.borrow_mut().rename("left.txt", ".swap");
    h.fs.borrow_mut().rename("right.txt", "left.txt");
    h.fs.borrow_mut().rename(".swap", "right.txt");
    let report = h.pass(0).await;

    assert!(report.reconcile.moves.is_empty(), "互换不能用单向移动表达");
    assert!(report.reconcile.deletions.is_empty(), "互换不是删除");
    assert_eq!(report.reconcile.held.len(), 2);
    assert!(
        report
            .reconcile
            .held
            .iter()
            .all(|held| held.code == "identity_swap_needs_review")
    );
    // 不按路径重绑：两边条目身份都没有被改写到对方的文件上。
    assert_eq!(h.store.entry_of("left.txt"), Some(left_entry));
    assert_eq!(h.store.entry_of("right.txt"), Some(right_entry));
    assert_eq!(h.store.versions.len(), versions_before);
    assert_eq!(h.store.writes.len(), 2, "没有重新采集内容");
}

#[tokio::test]
async fn busy_vault_defers_reconciliation_to_the_next_round() {
    let mut h = Harness::new(default_limits());
    h.fs.borrow_mut().write("a.txt", b"1");
    h.settle().await;
    h.fs.borrow_mut().unlink("a.txt");

    let ticket = h.gate.acquire(vault_id(0x21)).expect("拿到票据");
    let report = h.pass(0).await;
    assert!(
        report.reconcile.deferred,
        "Vault 忙时整轮核对推迟，不阻塞其他写入"
    );
    assert!(report.reconcile.deletions.is_empty());
    assert!(report.reconcile.held.is_empty(), "推迟不是失败，不记 held");
    assert!(h.store.deletes.is_empty());
    h.gate.release(ticket);

    let after = h.pass(0).await;
    assert_eq!(after.reconcile.deletions.len(), 1);
}

#[tokio::test]
async fn hinted_absence_alone_is_not_a_deletion() {
    let mut h = Harness::new(default_limits());
    h.fs.borrow_mut().write("a.txt", b"1");
    h.settle().await;
    h.fs.borrow_mut().unlink("a.txt");
    let limits = PassLimits::new(ScanLimits::new(5_000).expect("合法"), 200).expect("合法");
    let clock = PassClock {
        now_ms: h.now_ms + 1,
        at: user_time(1_700_000_100),
    };
    let mut ports = ObserverPorts {
        work: &mut h.work,
        blobs: &mut h.blobs,
        layout: &h.layout,
        gate: &mut h.gate,
        store: &mut h.store,
    };
    let report = h
        .observer
        .run_hinted_pass(&mut ports, &["a.txt".to_owned()], &[], limits, clock)
        .await;
    assert_eq!(report.absent, vec!["a.txt".to_owned()]);
    assert!(h.store.deletes.is_empty(), "线索不是完整扫描，不推导删除");
    assert!(h.observer.history_protected("a.txt"));
}

#[tokio::test]
async fn committed_api_moves_replace_old_observation_paths_without_losing_new_content() {
    for modified_during_move in [false, true] {
        let mut h = Harness::new(default_limits());
        h.fs.borrow_mut()
            .write("parent/child/note.txt", b"original");
        h.settle().await;
        let entry = h.store.entry_of("parent/child/note.txt").expect("初始身份");
        let versions = h.store.versions.len();
        let mut previous = "parent/child/note.txt";
        // 模拟 API 先提交父目录、再提交子目录改名：后代 EntryId/revision 均不变。
        for next in ["renamed/child/note.txt", "renamed/child-renamed/note.txt"] {
            h.fs.borrow_mut().rename(previous, next);
            let metadata = h.store.entries.remove(previous).expect("API 旧路径");
            h.store.entries.insert(next.to_owned(), metadata);
            if modified_during_move {
                h.fs.borrow_mut()
                    .write_preserving_mtime(next, next.as_bytes());
            }
            for _ in 0..3 {
                let report = h.settle().await;
                assert!(
                    report.reconcile.deletions.is_empty(),
                    "API 路径变化不等于删除"
                );
                assert!(
                    report.reconcile.moves.is_empty(),
                    "API 已提交，不追加重复移动事件"
                );
            }
            assert_eq!(h.store.entry_of(next), Some(entry));
            assert!(!h.observer.history_protected(previous));
            assert!(h.observer.history_protected(next));
            assert_eq!(h.observer.adopted_count(), 1, "同一身份只保留一个观察路径");
            assert_eq!(h.observer.adopted_paths.len(), 1);
            previous = next;
        }
        assert_eq!(
            h.store.versions.len(),
            versions + if modified_during_move { 2 } else { 0 }
        );
        if modified_during_move {
            assert_eq!(
                h.store.versions.last().unwrap().digest,
                crate::observer::fixtures::digest_of(previous.as_bytes())
            );
        }
        h.fs.borrow_mut().unlink(previous);
        let deleted = h.pass(0).await;
        assert_eq!(
            deleted.reconcile.deletions.len(),
            1,
            "新路径上的真实外部删除仍被观察"
        );
        assert_eq!(deleted.reconcile.deletions[0].entry, entry);
        assert_eq!(deleted.reconcile.deletions[0].path, previous);
        assert!(h.observer.adopted_paths.is_empty());
    }
}
