//! 真实目录 + 真实 inotify 的持续观察验收（S050）：事件只是线索，溢出/保留 mtime 的改写/权限失败
//! 都靠补扫与复核最终收敛，且不推导删除。

use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use vaultsyncd_domain::version::UtcTimestamp;
use vaultsyncd_platform_linux_server::files::{ResolveMode, VaultHandle};
use vaultsyncd_platform_linux_server::watcher::WatchConfig;
use vaultsyncd_server_core::commit::put::VaultCommitGate;
use vaultsyncd_server_core::objects::{ExpectedContent, ObjectLayout};
use vaultsyncd_server_core::observer::{
    ExternalObserver, FollowConfig, FollowDriver, FullScanCause, HintLimits, InconclusiveReason,
    ObserverPorts, OwnWriteIntent, PassClock, PassLimits, ScanHealth, ScanLimits, StabilityWindow,
    StepReport, WatchHint,
};
use vaultsyncd_server_core::ports::work_tree::{WorkObservation, WorkTreePort};

use super::tests::{Lab, MemStore, WINDOW_MS, marker_id, settle_clock, sha};
use super::{LinuxObjectPort, LinuxWorkTree, VaultWatcher, hint_of};
use vaultsyncd_domain::ids::VaultId;

const DEBOUNCE_MS: u64 = 100;
const METADATA_MS: u64 = 60_000;
const RETRY_MS: u64 = 5_000;
const REVIEW_MS: u64 = 10_000;

struct FollowEnv {
    work: LinuxWorkTree,
    blobs: LinuxObjectPort,
    store: MemStore,
    gate: VaultCommitGate,
    layout: ObjectLayout,
    driver: FollowDriver,
    watcher: VaultWatcher,
    now_ms: u64,
}

impl FollowEnv {
    fn new(lab: &Lab) -> Self {
        let vault =
            Arc::new(VaultHandle::open(&lab.vault_dir, ResolveMode::Auto).expect("打开 Vault"));
        let vault_id = VaultId::from_bytes(marker_id(0x11));
        let observer = ExternalObserver::new(
            vault_id,
            StabilityWindow::from_millis(WINDOW_MS).expect("合法"),
            "ext-s050",
        )
        .expect("前缀合法");
        let config = FollowConfig::new(
            HintLimits::new(4_096, DEBOUNCE_MS).expect("合法"),
            METADATA_MS,
            RETRY_MS,
            REVIEW_MS,
            64,
        )
        .expect("合法");
        // 先建立监视，再启动补扫：监视建立之前的变化只能靠补扫发现。
        let (watcher, gaps) =
            VaultWatcher::open(vault.clone(), WatchConfig::DEFAULT).expect("建立监视");
        let mut driver = FollowDriver::new(observer, config);
        for gap in gaps {
            driver.push_hint(gap, 1_000);
        }
        FollowEnv {
            work: LinuxWorkTree::new(vault.clone()),
            blobs: LinuxObjectPort::new(vault, &lab.objects),
            store: MemStore::default(),
            gate: VaultCommitGate::new(),
            layout: ObjectLayout::new(&lab.objects),
            driver,
            watcher,
            now_ms: 1_000,
        }
    }

    /// 读取队列里已有的内核事件（等待至多 `wait_ms`）并交给驱动器。
    fn pump(&mut self, wait_ms: u64) -> usize {
        self.watcher
            .pump(
                &mut self.driver,
                self.now_ms,
                Duration::from_millis(wait_ms),
            )
            .expect("读取 inotify 事件")
    }

    async fn step(&mut self, advance_ms: u64) -> StepReport {
        self.now_ms += advance_ms;
        let limits = PassLimits::new(ScanLimits::new(10_000).expect("合法"), 1_000).expect("合法");
        let clock = PassClock {
            now_ms: self.now_ms,
            at: UtcTimestamp::from_unix_seconds(1_700_000_000).expect("合法"),
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
    async fn start(&mut self) {
        let first = self.step(0).await;
        assert_eq!(
            first.full_scan.as_ref().map(|run| run.cause),
            Some(FullScanCause::Startup)
        );
        self.step(WINDOW_MS).await;
    }

    /// 读事件 → 去抖 → 稳定窗口，直到变化被采纳（最多 4 轮）。
    async fn follow(&mut self) {
        self.pump(300);
        for _ in 0..4 {
            self.step(DEBOUNCE_MS).await;
            self.step(WINDOW_MS).await;
            self.pump(0);
        }
    }

    fn versions_of(
        &self,
        path: &str,
    ) -> Vec<&(
        String,
        vaultsyncd_server_core::objects::ContentDigest,
        String,
    )> {
        self.store
            .versions
            .iter()
            .filter(|version| version.0 == path)
            .collect()
    }
}

fn running_as_root() -> bool {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("Uid:"))
                .map(|line| line.split_whitespace().nth(2) == Some("0"))
        })
        .unwrap_or(false)
}

#[tokio::test]
async fn real_rename_over_final_content_is_captured_through_inotify_hints() {
    let lab = Lab::new("f-rename-over");
    lab.write("doc.txt", b"version one");
    let mut env = FollowEnv::new(&lab);
    env.start().await;
    assert_eq!(env.versions_of("doc.txt").len(), 1);

    // 编辑器保存：写临时文件，再 rename-over 目标。
    lab.write(".doc.txt.swp", b"the final saved content");
    fs::rename(
        lab.vault_dir.join(".doc.txt.swp"),
        lab.vault_dir.join("doc.txt"),
    )
    .expect("rename-over");
    env.follow().await;

    let versions = env.versions_of("doc.txt");
    assert_eq!(versions.len(), 2, "rename-over 的最终内容必须被采集");
    assert_eq!(versions[1].1, sha(b"the final saved content"));
    let stored = fs::read(lab.objects.join(&versions[1].2)).expect("对象存在");
    assert_eq!(stored, b"the final saved content");
    assert!(
        env.versions_of(".doc.txt.swp").is_empty(),
        "临时名已不存在，没有它的版本，也没有删除结论"
    );
}

#[tokio::test]
async fn real_kernel_queue_overflow_forces_an_immediate_scan_that_finds_the_lost_change() {
    let lab = Lab::new("f-overflow");
    lab.write("one", b"1");
    lab.write("two", b"2");
    lab.write("target.txt", b"before the overflow");
    let mut env = FollowEnv::new(&lab);
    env.start().await;
    assert_eq!(env.versions_of("target.txt").len(), 1);

    let limit: usize = fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .expect("内核参数")
        .trim()
        .parse()
        .expect("数字");
    // 交替改两个文件的时间戳（相邻事件不同，内核不合并），不读取队列直到真实溢出。
    let one = File::options()
        .write(true)
        .open(lab.vault_dir.join("one"))
        .expect("开");
    let two = File::options()
        .write(true)
        .open(lab.vault_dir.join("two"))
        .expect("开");
    for round in 0..=(limit / 2 + 16) {
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + round as u64);
        one.set_times(FileTimes::new().set_modified(when))
            .expect("时间");
        two.set_times(FileTimes::new().set_modified(when))
            .expect("时间");
    }
    // 队列已满之后的改写：它的事件被内核丢弃。
    settle_clock();
    fs::write(lab.vault_dir.join("target.txt"), b"changed after overflow").expect("写");

    env.pump(300);
    // 溢出不等去抖：一步之内立即补扫。
    let immediate = env.step(1).await;
    let run = immediate.full_scan.expect("溢出必须立即补扫");
    assert_eq!(run.cause, FullScanCause::QueueOverflow);
    assert!(run.report.scan_complete);
    env.step(WINDOW_MS).await;
    let versions = env.versions_of("target.txt");
    assert_eq!(versions.len(), 2, "事件丢失的改写由补扫最终发现");
    assert_eq!(versions[1].1, sha(b"changed after overflow"));
}

#[tokio::test]
async fn real_rewrite_preserving_mtime_is_found_by_periodic_scan_even_with_all_events_lost() {
    let lab = Lab::new("f-mtime");
    lab.write("a.txt", b"AAAAAAAA");
    let mut env = FollowEnv::new(&lab);
    env.start().await;
    let before = fs::metadata(lab.vault_dir.join("a.txt"))
        .expect("元数据")
        .modified()
        .expect("mtime");

    // 原地改写同样大小的内容并把 mtime 还原；之后丢掉所有事件（模拟事件丢失）。
    settle_clock();
    let file = File::options()
        .write(true)
        .open(lab.vault_dir.join("a.txt"))
        .expect("开");
    (&file).write_all(b"BBBBBBBB").expect("写");
    file.set_times(FileTimes::new().set_modified(before))
        .expect("还原 mtime");
    drop(file);
    assert_eq!(
        fs::metadata(lab.vault_dir.join("a.txt"))
            .expect("元数据")
            .modified()
            .expect("mtime"),
        before,
        "mtime 确实被保留"
    );
    // 不读取 inotify 队列：事件对驱动器而言全部丢失，只剩周期扫描。

    let idle = env.step(METADATA_MS / 2).await;
    assert!(idle.full_scan.is_none());
    let periodic = env.step(METADATA_MS / 2).await;
    assert_eq!(
        periodic.full_scan.as_ref().map(|run| run.cause),
        Some(FullScanCause::Periodic)
    );
    env.step(WINDOW_MS).await;
    let versions = env.versions_of("a.txt");
    assert_eq!(versions.len(), 2, "ctime 暴露了保留 mtime 的改写");
    assert_eq!(versions[1].1, sha(b"BBBBBBBB"));
}

#[tokio::test]
async fn real_permission_failure_never_becomes_a_deletion_and_recovers() {
    if running_as_root() {
        return;
    }
    let lab = Lab::new("f-perm");
    lab.write("locked/secret.txt", b"private notes");
    lab.write("open.txt", b"open");
    let mut env = FollowEnv::new(&lab);
    env.start().await;
    let versions_before = env.store.versions.len();
    let events_before = env.store.events.len();

    fs::set_permissions(
        lab.vault_dir.join("locked"),
        fs::Permissions::from_mode(0o000),
    )
    .expect("收回权限");
    env.follow().await;
    let periodic = env.step(METADATA_MS).await;
    let run = periodic.full_scan.expect("周期扫描");
    assert!(!run.report.scan_complete, "读不了的目录不是“目录为空”");
    assert!(run.report.inconclusive.iter().any(|reason| matches!(
        reason,
        InconclusiveReason::DirectoryUnreadable { path, .. } if path == "locked"
    )));
    assert!(env.driver.observer().history_protected("locked/secret.txt"));
    assert_eq!(env.store.versions.len(), versions_before);
    assert_eq!(env.store.events.len(), events_before, "没有任何删除事件");

    // 权限恢复并改写文件：事件与重试扫描都能找到它。
    fs::set_permissions(
        lab.vault_dir.join("locked"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("恢复权限");
    settle_clock();
    fs::write(
        lab.vault_dir.join("locked/secret.txt"),
        b"edited after unlock",
    )
    .expect("写");
    env.follow().await;
    env.step(RETRY_MS).await;
    env.step(WINDOW_MS).await;
    let versions = env.versions_of("locked/secret.txt");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[1].1, sha(b"edited after unlock"));
    // 清理：测试结束时 Lab 的 Drop 会恢复权限。
}

#[tokio::test]
async fn real_own_write_is_matched_by_inode_and_content_not_by_time() {
    let lab = Lab::new("f-own");
    let mut env = FollowEnv::new(&lab);
    env.start().await;

    // 服务端发布：先写暂存文件，取其真实身份登记意图，再 rename 到目标路径。
    lab.write(".stage-1", b"content published by the service");
    let WorkObservation::File(staged) = env.work.observe(".stage-1").expect("观察") else {
        panic!("暂存文件应是普通文件");
    };
    env.driver
        .observer_mut()
        .expect_own_write(
            "svc.txt",
            OwnWriteIntent {
                device: staged.key.device,
                inode: staged.key.inode,
                content: ExpectedContent::new(
                    sha(b"content published by the service"),
                    b"content published by the service".len() as u64,
                ),
            },
        )
        .expect("登记意图");
    fs::rename(
        lab.vault_dir.join(".stage-1"),
        lab.vault_dir.join("svc.txt"),
    )
    .expect("发布");
    env.follow().await;
    assert!(
        env.versions_of("svc.txt").is_empty(),
        "inode 与内容都与意图一致：本服务自己的写入不当作外部变化采纳"
    );
    assert_eq!(env.driver.observer().own_write_intents(), 1);

    // 紧接着（没有任何时间间隔的假设）用户原地改写同一个文件：必须被采集。
    let mut file = File::options()
        .append(true)
        .open(lab.vault_dir.join("svc.txt"))
        .expect("开");
    file.write_all(b" + a user edit").expect("追加");
    drop(file);
    env.follow().await;
    let versions = env.versions_of("svc.txt");
    assert_eq!(versions.len(), 1, "同一窗口内的用户修改不能被忽略");
    assert_eq!(
        versions[0].1,
        sha(b"content published by the service + a user edit")
    );
    assert_eq!(env.driver.observer().own_write_intents(), 0, "意图已作废");
}

#[tokio::test]
async fn real_new_directory_tree_is_picked_up_through_subtree_hints() {
    let lab = Lab::new("f-newdir");
    lab.write("seed.txt", b"seed");
    let mut env = FollowEnv::new(&lab);
    env.start().await;

    fs::create_dir_all(lab.vault_dir.join("fresh/deep")).expect("建目录树");
    lab.write(
        "fresh/deep/early.txt",
        b"written before the watch could exist",
    );
    env.follow().await;
    assert_eq!(env.versions_of("fresh/deep/early.txt").len(), 1);

    lab.write("fresh/deep/late.txt", b"written after the watch");
    env.follow().await;
    assert_eq!(env.versions_of("fresh/deep/late.txt").len(), 1);
}

#[tokio::test]
async fn watcher_signals_convert_one_to_one_into_core_hints() {
    use vaultsyncd_platform_linux_server::watcher::{GapReason, WatchSignal};
    assert_eq!(
        hint_of(WatchSignal::Changed { path: "a".into() }),
        WatchHint::Changed("a".into())
    );
    assert_eq!(hint_of(WatchSignal::Overflow), WatchHint::Overflow);
    assert!(matches!(
        hint_of(WatchSignal::Gap {
            path: "d".into(),
            reason: GapReason::WatchLimit,
            detail: "x".into()
        }),
        WatchHint::Gap { path, detail } if path == "d" && detail.starts_with("watch_limit")
    ));
}

/// S051 验收 1：卸载后路径下露出空目录 = Vault 不可用，零删除事件。
///
/// 用真实目录模拟卸载的后果：内核在 `umount` 之后让路径露出挂载点下面的目录（通常是空目录，
/// 与挂载时的目录不是同一个 inode）。这里把 Vault 目录改名再在原路径建一个空目录，观察到的现场
/// 与卸载后一致；Vault 句柄仍指向原来的目录。
#[tokio::test]
async fn unmount_leaving_an_empty_directory_makes_the_vault_unavailable_with_zero_deletions() {
    let lab = Lab::new("f-unmount");
    lab.write("a.txt", b"alpha");
    lab.write("b.txt", b"beta");
    let mut env = FollowEnv::new(&lab);
    env.start().await;
    assert_eq!(env.versions_of("a.txt").len(), 1);
    assert_eq!(env.versions_of("b.txt").len(), 1);
    let versions_before = env.store.versions.len();
    let events_before = env.store.events.len();

    // 卸载：Vault 路径下露出另一个（空）目录。
    let moved = lab.vault_dir.with_extension("unmounted");
    fs::rename(&lab.vault_dir, &moved).expect("把原 Vault 目录移走");
    fs::create_dir(&lab.vault_dir).expect("在原路径留下空目录");

    // 没有事件可依赖：周期元数据补扫是触发点（真实卸载同样不产生 Vault 内的事件）。
    let report = env.step(METADATA_MS).await;
    let pass = report
        .full_scan
        .as_ref()
        .map(|run| &run.report)
        .expect("周期补扫必须发生");
    assert!(pass.vault_unavailable, "露出空目录必须是 Vault 不可用");
    assert!(matches!(
        pass.reconcile.health,
        ScanHealth::MountChanged { .. }
    ));
    assert!(!pass.reconcile.ran, "不可用时不核对缺失");
    assert!(pass.reconcile.deletions.is_empty());
    assert_eq!(env.store.events.len(), events_before, "零删除事件");
    assert_eq!(env.store.versions.len(), versions_before, "零新版本");

    // 恢复挂载（把原目录放回原路径）：文件仍是同一身份，不产生删除，也不产生新版本。
    fs::remove_dir(&lab.vault_dir).expect("删掉空目录");
    fs::rename(&moved, &lab.vault_dir).expect("放回原目录");
    let recovered = env.step(METADATA_MS).await;
    let pass = recovered
        .full_scan
        .as_ref()
        .map(|run| &run.report)
        .expect("恢复后仍由周期补扫触发");
    assert!(!pass.vault_unavailable);
    assert_eq!(pass.reconcile.health, ScanHealth::Complete);
    assert!(pass.reconcile.deletions.is_empty());
    assert_eq!(
        env.store.events.len(),
        events_before,
        "恢复后也没有删除事件"
    );
    assert_eq!(env.store.versions.len(), versions_before);
}

/// S051 验收 2（真实目录）：两个内容相同的文件互换名字，EntryId 不串。
#[tokio::test]
async fn two_identical_files_swapped_on_a_real_directory_keep_their_own_entries() {
    let lab = Lab::new("f-swap");
    lab.write("left.txt", b"identical bytes");
    lab.write("right.txt", b"identical bytes");
    let mut env = FollowEnv::new(&lab);
    env.start().await;
    assert_eq!(env.versions_of("left.txt").len(), 1);
    assert_eq!(env.versions_of("right.txt").len(), 1);

    // 互换名字：内容哈希完全相同，只有 inode 能区分。
    let left = lab.vault_dir.join("left.txt");
    let right = lab.vault_dir.join("right.txt");
    let tmp = lab.vault_dir.join(".tmp-swap");
    fs::rename(&left, &tmp).expect("left → tmp");
    fs::rename(&right, &left).expect("right → left");
    fs::rename(&tmp, &right).expect("tmp → right");

    let report = env.step(METADATA_MS).await;
    let pass = report
        .full_scan
        .as_ref()
        .map(|run| &run.report)
        .expect("周期补扫");
    assert_eq!(pass.reconcile.health, ScanHealth::Complete);
    // 互换名字不是一次单向移动：两个路径都还在、内容哈希完全相同，只有 inode 能看出它们换了位置。
    // 核心不按路径或内容把 EntryId 重新绑一次，而是保留双方基线并把现场记成待复核。
    assert!(pass.reconcile.moves.is_empty(), "互换不能用单向移动表达");
    assert!(
        pass.reconcile.deletions.is_empty(),
        "互换不是删除：{:?}",
        pass.reconcile.deletions
    );
    let held: Vec<&str> = pass
        .reconcile
        .held
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert!(
        held.contains(&"left.txt") && held.contains(&"right.txt"),
        "{held:?}"
    );
    assert!(
        pass.reconcile
            .held
            .iter()
            .all(|entry| entry.code == "identity_swap_needs_review")
    );
    // 没有按路径重绑：两个路径都还没有新版本、没有新事件。
    assert_eq!(env.versions_of("left.txt").len(), 1);
    assert_eq!(env.versions_of("right.txt").len(), 1);
    assert_eq!(env.store.events.len(), 2);
}
