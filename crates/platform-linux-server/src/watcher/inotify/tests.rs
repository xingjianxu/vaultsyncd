use std::fs::{self, File, FileTimes};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::*;
use crate::files::restricted::ResolveMode;

struct Lab {
    dir: PathBuf,
}

impl Lab {
    fn new(name: &str) -> Self {
        // `VAULTSYNCD_TEST_DIR` 指向（btrfs 测试卷上的）基目录时用它，否则用系统临时目录。
        let parent =
            std::env::var_os("VAULTSYNCD_TEST_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let dir = parent.join(format!("vaultsyncd-s050-w-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建目录");
        Lab { dir }
    }

    fn vault(&self) -> VaultHandle {
        VaultHandle::open(&self.dir, ResolveMode::Auto).expect("打开 Vault")
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.join(relative)
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        restore(&self.dir);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn restore(path: &Path) {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.is_dir() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
            if let Ok(read) = fs::read_dir(path) {
                for entry in read.flatten() {
                    restore(&entry.path());
                }
            }
        }
    }
}

fn collect(watcher: &mut DirWatcher, vault: &VaultHandle) -> Vec<WatchSignal> {
    watcher
        .poll(vault, Duration::from_millis(500))
        .expect("读取事件")
}

fn changed(signals: &[WatchSignal], path: &str) -> bool {
    signals
        .iter()
        .any(|signal| matches!(signal, WatchSignal::Changed { path: p } if p == path))
}

fn subtree(signals: &[WatchSignal], path: &str) -> bool {
    signals
        .iter()
        .any(|signal| matches!(signal, WatchSignal::Subtree { path: p } if p == path))
}

fn gap_of<'a>(signals: &'a [WatchSignal], path: &str) -> Option<&'a GapReason> {
    signals.iter().find_map(|signal| match signal {
        WatchSignal::Gap {
            path: p, reason, ..
        } if p == path => Some(reason),
        _ => None,
    })
}

#[test]
fn file_creation_and_rename_over_are_reported_as_hints_for_the_final_path() {
    let lab = Lab::new("rename-over");
    fs::write(lab.path("a.txt"), b"old").expect("写");
    let vault = lab.vault();
    let (mut watcher, gaps) = DirWatcher::open(&vault, WatchConfig::DEFAULT).expect("监视");
    assert!(gaps.is_empty());

    fs::write(lab.path(".a.txt.tmp"), b"new").expect("写临时文件");
    fs::rename(lab.path(".a.txt.tmp"), lab.path("a.txt")).expect("rename-over");
    let signals = collect(&mut watcher, &vault);
    assert!(
        changed(&signals, "a.txt"),
        "rename-over 的最终路径必须有线索：{signals:?}"
    );
}

#[test]
fn new_directory_is_watched_before_its_subtree_hint_so_nothing_falls_in_a_gap() {
    let lab = Lab::new("new-dir");
    let vault = lab.vault();
    let (mut watcher, _) = DirWatcher::open(&vault, WatchConfig::DEFAULT).expect("监视");
    assert_eq!(watcher.watch_count(), 1);

    fs::create_dir_all(lab.path("x/y")).expect("建目录树");
    fs::write(lab.path("x/y/early.txt"), b"early").expect("写");
    let signals = collect(&mut watcher, &vault);
    assert!(
        subtree(&signals, "x"),
        "新目录必须要求补扫子树：{signals:?}"
    );
    assert!(watcher.is_watching("x") && watcher.is_watching("x/y"));

    fs::write(lab.path("x/y/late.txt"), b"late").expect("写");
    let later = collect(&mut watcher, &vault);
    assert!(changed(&later, "x/y/late.txt"), "{later:?}");
}

#[test]
fn moved_away_directory_stops_reporting_under_its_old_path() {
    let lab = Lab::new("move-dir");
    fs::create_dir_all(lab.path("d")).expect("建");
    let vault = lab.vault();
    let (mut watcher, _) = DirWatcher::open(&vault, WatchConfig::DEFAULT).expect("监视");
    fs::rename(lab.path("d"), lab.path("e")).expect("改名");
    let signals = collect(&mut watcher, &vault);
    assert!(changed(&signals, "d"), "{signals:?}");
    assert!(subtree(&signals, "e"), "{signals:?}");
    assert!(!watcher.is_watching("d") && watcher.is_watching("e"));
}

#[test]
fn kernel_queue_overflow_is_reported_and_watches_keep_working_afterwards() {
    let lab = Lab::new("overflow");
    fs::write(lab.path("one"), b"1").expect("写");
    fs::write(lab.path("two"), b"2").expect("写");
    fs::write(lab.path("target.txt"), b"before").expect("写");
    let vault = lab.vault();
    let (mut watcher, _) = DirWatcher::open(&vault, WatchConfig::DEFAULT).expect("监视");
    let limit: usize = fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .expect("内核参数")
        .trim()
        .parse()
        .expect("数字");

    // 交替改两个文件的时间戳：相邻事件不同，内核不会合并；不读取队列直到溢出。
    let one = File::options()
        .write(true)
        .open(lab.path("one"))
        .expect("开");
    let two = File::options()
        .write(true)
        .open(lab.path("two"))
        .expect("开");
    for round in 0..=(limit / 2 + 16) {
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + round as u64);
        one.set_times(FileTimes::new().set_modified(when))
            .expect("改时间");
        two.set_times(FileTimes::new().set_modified(when))
            .expect("改时间");
    }
    // 队列已满之后的变化：它的事件被内核丢弃，只有补扫能发现。
    fs::write(lab.path("target.txt"), b"after!").expect("写");

    let signals = collect(&mut watcher, &vault);
    assert!(
        signals.contains(&WatchSignal::Overflow),
        "溢出必须上报：共 {} 条信号",
        signals.len()
    );
    assert!(
        !changed(&signals, "target.txt"),
        "队列溢出之后的事件应已丢失，这正是必须补扫的原因"
    );

    // 溢出后监视表已重建，新事件照常到达。
    fs::write(lab.path("fresh.txt"), b"x").expect("写");
    let later = collect(&mut watcher, &vault);
    assert!(changed(&later, "fresh.txt"), "{later:?}");
}

#[test]
fn unreadable_directory_is_a_gap_not_a_deletion_and_recovers_when_permission_returns() {
    if running_as_root() {
        return;
    }
    let lab = Lab::new("perm");
    fs::create_dir_all(lab.path("locked/inner")).expect("建");
    fs::set_permissions(lab.path("locked"), fs::Permissions::from_mode(0o000)).expect("改权限");
    let vault = lab.vault();
    let (mut watcher, gaps) = DirWatcher::open(&vault, WatchConfig::DEFAULT).expect("监视");
    assert_eq!(
        gap_of(&gaps, "locked"),
        Some(&GapReason::PermissionDenied),
        "{gaps:?}"
    );
    assert!(!watcher.is_watching("locked"));

    fs::set_permissions(lab.path("locked"), fs::Permissions::from_mode(0o755)).expect("恢复");
    let signals = collect(&mut watcher, &vault);
    assert!(
        subtree(&signals, "locked"),
        "权限恢复后要求补扫：{signals:?}"
    );
    assert!(watcher.is_watching("locked") && watcher.is_watching("locked/inner"));
}

#[test]
fn watch_limit_is_reported_as_a_gap() {
    let lab = Lab::new("limit");
    fs::create_dir_all(lab.path("a")).expect("建");
    fs::create_dir_all(lab.path("b")).expect("建");
    let vault = lab.vault();
    let config = WatchConfig {
        max_watches: 2,
        ..WatchConfig::DEFAULT
    };
    let (watcher, gaps) = DirWatcher::open(&vault, config).expect("监视");
    assert_eq!(watcher.watch_count(), 2);
    assert!(
        gaps.iter().any(|signal| matches!(
            signal,
            WatchSignal::Gap {
                reason: GapReason::WatchLimit,
                ..
            }
        )),
        "{gaps:?}"
    );
}

fn running_as_root() -> bool {
    // 目录权限测试对 vault 无意义（vault 绕过 DAC）；用 /proc 判断，避免引入 libc。
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
