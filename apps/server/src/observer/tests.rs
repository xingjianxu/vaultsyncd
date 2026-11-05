use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use vaultsyncd_domain::ids::{EntryId, ID_LEN, RootId, TxId, VersionId};
use vaultsyncd_domain::version::{Revision, RootSeq, UtcTimestamp};
use vaultsyncd_platform_linux_server::files::{self, ResolveMode, RootHandle};
use vaultsyncd_server_core::commit::put::RootCommitGate;
use vaultsyncd_server_core::objects::{ContentDigest, ExpectedContent, ObjectLayout};
use vaultsyncd_server_core::observer::{
    ExternalObserver, FileState, InconclusiveReason, ObserverPorts, PassClock, PassLimits,
    PassReport, PendingReason, ScanLimits, StabilityWindow,
};
use vaultsyncd_server_core::ports::external_store::{
    AdoptKind, ExternalAdoptStore, ExternalRecord, ExternalStoreError, ExternalWrite, KnownEntry,
};
use vaultsyncd_server_core::ports::object_store::CopyMethod;
use vaultsyncd_server_core::ports::put_store::EntryKind;

use super::{LinuxObjectPort, LinuxWorkTree};

const WINDOW_MS: u64 = 500;

fn marker_id(marker: u64) -> [u8; ID_LEN] {
    let mut bytes = [0u8; ID_LEN];
    bytes[..8].copy_from_slice(&marker.to_be_bytes());
    bytes
}

struct StoredEntry {
    entry: EntryId,
    revision: Revision,
    content: ExpectedContent,
}

/// 内存采纳存储：版本与事件按事务一起追加。
#[derive(Default)]
struct MemStore {
    entries: BTreeMap<String, StoredEntry>,
    versions: Vec<(String, ContentDigest, String)>,
    events: Vec<String>,
    counter: u64,
}

impl ExternalAdoptStore for MemStore {
    async fn lookup_path(
        &mut self,
        _root: RootId,
        path: &str,
    ) -> Result<Option<KnownEntry>, ExternalStoreError> {
        Ok(self.entries.get(path).map(|stored| KnownEntry {
            entry: stored.entry,
            revision: stored.revision,
            kind: EntryKind::File,
            content: Some(stored.content),
        }))
    }

    async fn apply_external(
        &mut self,
        write: &ExternalWrite,
    ) -> Result<ExternalRecord, ExternalStoreError> {
        self.counter += 1;
        let (entry, revision) = match write.kind {
            AdoptKind::Create => (
                EntryId::from_bytes(marker_id(100 + self.counter)),
                Revision::new(1).expect("合法"),
            ),
            AdoptKind::Update {
                entry,
                expected_revision,
            } => (entry, expected_revision.next().expect("可推进")),
        };
        self.entries.insert(
            write.path.clone(),
            StoredEntry {
                entry,
                revision,
                content: write.content,
            },
        );
        self.versions.push((
            write.path.clone(),
            write.content.digest,
            write.storage_path.clone(),
        ));
        self.events.push(write.path.clone());
        Ok(ExternalRecord {
            entry,
            version: VersionId::from_bytes(marker_id(500 + self.counter)),
            revision,
            seq: RootSeq::new(self.counter).expect("合法"),
            tx: TxId::from_bytes(marker_id(700 + self.counter)),
            digest: write.content.digest,
            size: write.content.bytes,
        })
    }
}

/// 真实目录夹具：`VAULTSYNCD_TEST_DIR` 指向（btrfs 测试卷上的）基目录时用它，否则用系统临时目录。
struct Lab {
    base: PathBuf,
    root_dir: PathBuf,
    objects: PathBuf,
}

impl Lab {
    fn new(name: &str) -> Self {
        let parent =
            std::env::var_os("VAULTSYNCD_TEST_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let base = parent.join(format!("vaultsyncd-s049-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root_dir = base.join("root");
        let objects = base.join("objects");
        fs::create_dir_all(&root_dir).expect("建 Root 目录");
        fs::create_dir_all(&objects).expect("建对象区目录");
        Lab {
            base,
            root_dir,
            objects,
        }
    }

    fn write(&self, relative: &str, data: &[u8]) {
        let path = self.root_dir.join(relative);
        fs::create_dir_all(path.parent().expect("有父目录")).expect("建父目录");
        let mut file = File::create(path).expect("写文件");
        file.write_all(data).expect("写内容");
        file.sync_all().expect("刷盘");
        settle_clock();
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        // 测试里可能留下 000 权限的目录：先恢复，保证清理不残留。
        restore_permissions(&self.base);
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn restore_permissions(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_dir() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
            if let Ok(read) = fs::read_dir(path) {
                for entry in read.flatten() {
                    restore_permissions(&entry.path());
                }
            }
        }
    }
}

/// 内核时间戳粒度是粗的（数毫秒）：两次写入之间留出间隔，避免相邻写入得到相同的 ctime。
fn settle_clock() {
    std::thread::sleep(Duration::from_millis(25));
}

struct Env {
    work: LinuxWorkTree,
    blobs: LinuxObjectPort,
    store: MemStore,
    gate: RootCommitGate,
    layout: ObjectLayout,
    observer: ExternalObserver,
    now_ms: u64,
    root_id: RootId,
}

impl Env {
    fn new(lab: &Lab) -> Self {
        let root = Rc::new(RootHandle::open(&lab.root_dir, ResolveMode::Auto).expect("打开 Root"));
        let root_id = RootId::from_bytes(marker_id(0x11));
        Env {
            work: LinuxWorkTree::new(root.clone()),
            blobs: LinuxObjectPort::new(root, &lab.objects),
            store: MemStore::default(),
            gate: RootCommitGate::new(),
            layout: ObjectLayout::new(&lab.objects),
            observer: ExternalObserver::new(
                root_id,
                StabilityWindow::from_millis(WINDOW_MS).expect("合法"),
                "ext-s049",
            )
            .expect("前缀合法"),
            now_ms: 1_000,
            root_id,
        }
    }

    async fn pass(&mut self, advance_ms: u64, scan_budget: usize) -> PassReport {
        self.now_ms += advance_ms;
        let limits =
            PassLimits::new(ScanLimits::new(scan_budget).expect("合法"), 100).expect("合法");
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
        self.observer.run_pass(&mut ports, limits, clock).await
    }

    /// 首次看到 + 经过一个窗口。
    async fn settle(&mut self) -> PassReport {
        self.pass(0, 1_000).await;
        self.pass(WINDOW_MS, 1_000).await
    }
}

fn sha(data: &[u8]) -> ContentDigest {
    ContentDigest::from_bytes(files::hash::digest(data))
}

fn state_of<'a>(report: &'a PassReport, path: &str) -> &'a FileState {
    &report
        .outcomes
        .iter()
        .find(|outcome| outcome.path == path)
        .unwrap_or_else(|| panic!("本轮没有 {path} 的结局：{:?}", report.outcomes))
        .state
}

fn object_file_of(lab: &Lab, storage_path: &str) -> PathBuf {
    lab.objects.join(storage_path)
}

#[tokio::test]
async fn real_directory_create_and_edit_produce_versions_and_events() {
    let lab = Lab::new("create-edit");
    lab.write("docs/a.txt", b"hello world");
    let mut env = Env::new(&lab);

    let waiting = env.pass(0, 1_000).await;
    assert!(matches!(
        state_of(&waiting, "docs/a.txt"),
        FileState::Pending(PendingReason::Waiting { .. })
    ));
    assert!(env.store.versions.is_empty(), "窗口内不采纳");
    let adopted = env.pass(WINDOW_MS, 1_000).await;
    let FileState::Adopted {
        created,
        copy_method,
        copy_fallback,
        ..
    } = state_of(&adopted, "docs/a.txt")
    else {
        panic!("窗口满后应采纳：{:?}", adopted.outcomes);
    };
    assert!(*created);
    // 记录实际副本方式：btrfs 上为 reflink，其它文件系统为校验复制并暴露原因。
    println!("copy_method={copy_method} fallback={copy_fallback:?}");
    assert_eq!(env.store.versions.len(), 1);
    assert_eq!(env.store.events, ["docs/a.txt"]);
    assert!(env.observer.history_protected("docs/a.txt"));
    assert!(adopted.initial_import_complete);
    // 对象字节等于内容、权限不可写；工作文件原样保留（采纳不改写工作目录）。
    let object = object_file_of(&lab, &env.store.versions[0].2);
    assert_eq!(fs::read(&object).expect("读对象"), b"hello world");
    assert_eq!(
        fs::metadata(&object)
            .expect("对象元数据")
            .permissions()
            .mode()
            & 0o222,
        0,
        "对象不向外部写入者开放写权限"
    );
    assert_eq!(
        fs::read(lab.root_dir.join("docs/a.txt")).expect("读工作文件"),
        b"hello world"
    );
    assert_eq!(
        fs::read_dir(lab.objects.join("staging"))
            .expect("staging 目录")
            .count(),
        0,
        "不留暂存副本"
    );

    // 改内容 → 新版本。
    lab.write("docs/a.txt", b"hello world, edited");
    env.pass(10, 1_000).await;
    let edited = env.pass(WINDOW_MS, 1_000).await;
    assert!(matches!(
        state_of(&edited, "docs/a.txt"),
        FileState::Adopted { created: false, .. }
    ));
    assert_eq!(env.store.versions.len(), 2);
    assert_eq!(env.store.versions[1].1, sha(b"hello world, edited"));
    // 旧版本对象仍在且字节不变。
    assert_eq!(
        fs::read(object_file_of(&lab, &env.store.versions[0].2)).expect("旧对象"),
        b"hello world"
    );
    let _ = env.root_id;
}

#[tokio::test]
async fn real_continuous_writes_keep_waiting_until_the_writer_stops() {
    let lab = Lab::new("continuous");
    lab.write("big.log", b"0");
    let mut env = Env::new(&lab);
    env.pass(0, 1_000).await;
    for round in 0..5 {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(lab.root_dir.join("big.log"))
            .expect("打开追加");
        file.write_all(format!("line {round}\n").as_bytes())
            .expect("追加");
        drop(file);
        settle_clock();
        let report = env.pass(WINDOW_MS / 2, 1_000).await;
        assert!(matches!(
            state_of(&report, "big.log"),
            FileState::Pending(PendingReason::Waiting { quiet_ms: 0 })
        ));
    }
    assert!(env.store.versions.is_empty(), "持续写入期间不传播部分片段");
    assert!(!env.observer.history_protected("big.log"));
    env.pass(0, 1_000).await;
    let done = env.pass(WINDOW_MS, 1_000).await;
    assert!(matches!(
        state_of(&done, "big.log"),
        FileState::Adopted { .. }
    ));
    let final_bytes = fs::read(lab.root_dir.join("big.log")).expect("读最终内容");
    assert_eq!(env.store.versions[0].1, sha(&final_bytes));
}

#[tokio::test]
async fn real_rewrite_preserving_mtime_and_rename_over_are_found() {
    let lab = Lab::new("mtime-rename");
    lab.write("keep.txt", b"aaaa");
    lab.write("swap.txt", b"old");
    let mut env = Env::new(&lab);
    env.settle().await;
    assert_eq!(env.store.versions.len(), 2);

    // 同样大小的改写，并把 mtime 设回原值：只有 ctime 暴露变化。
    let path = lab.root_dir.join("keep.txt");
    let old_mtime = fs::metadata(&path)
        .expect("元数据")
        .modified()
        .expect("mtime");
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("打开改写");
        file.write_all(b"bbbb").expect("改写");
        file.set_modified(old_mtime).expect("恢复 mtime");
    }
    settle_clock();
    // rename-over：同目录写新文件再替换。
    let tmp = lab.root_dir.join(".swap.tmp");
    fs::write(&tmp, b"brand new").expect("写临时文件");
    fs::rename(&tmp, lab.root_dir.join("swap.txt")).expect("rename-over");
    settle_clock();

    let waiting = env.pass(10, 1_000).await;
    assert!(matches!(
        state_of(&waiting, "keep.txt"),
        FileState::Pending(PendingReason::Waiting { quiet_ms: 0 })
    ));
    env.pass(WINDOW_MS, 1_000).await;
    let keep_versions: Vec<_> = env
        .store
        .versions
        .iter()
        .filter(|v| v.0 == "keep.txt")
        .collect();
    assert_eq!(keep_versions.len(), 2, "保留 mtime 的改写最终被发现");
    assert_eq!(keep_versions[1].1, sha(b"bbbb"));
    let swap_versions: Vec<_> = env
        .store
        .versions
        .iter()
        .filter(|v| v.0 == "swap.txt")
        .collect();
    assert_eq!(swap_versions.len(), 2);
    assert_eq!(
        swap_versions[1].1,
        sha(b"brand new"),
        "rename-over 的最终内容被采集"
    );
}

#[tokio::test]
async fn real_permission_failure_never_becomes_a_deletion_and_resumes_after_fix() {
    let lab = Lab::new("perm");
    lab.write("open/one.txt", b"1");
    lab.write("locked/two.txt", b"2");
    let locked = lab.root_dir.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("收回权限");
    let mut env = Env::new(&lab);
    if File::open(&locked).is_ok() || fs::read_dir(&locked).is_ok() {
        // 以 root 或带 CAP_DAC_OVERRIDE 运行时权限位不生效，本场景无法构造：如实跳过。
        eprintln!("跳过：当前进程可绕过目录权限");
        return;
    }
    env.pass(0, 1_000).await;
    let report = env.pass(WINDOW_MS, 1_000).await;
    assert!(!report.scan_complete);
    assert!(report.inconclusive.iter().any(|reason| matches!(
        reason,
        InconclusiveReason::DirectoryUnreadable { path, .. } if path == "locked"
    )));
    assert!(matches!(
        state_of(&report, "open/one.txt"),
        FileState::Adopted { .. }
    ));
    assert!(
        !report.initial_import_complete,
        "读不了的目录不能宣布导入完成"
    );
    assert!(env.observer.resume_cursor().is_some());
    assert!(!env.observer.history_protected("locked/two.txt"));
    assert!(
        env.store.events.iter().all(|path| path == "open/one.txt"),
        "没有任何删除或误采纳事件"
    );

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("恢复权限");
    env.pass(0, 1_000).await;
    let done = env.pass(WINDOW_MS, 1_000).await;
    assert!(matches!(
        state_of(&done, "locked/two.txt"),
        FileState::Adopted { .. }
    ));
    assert!(done.scan_complete);
    assert!(done.initial_import_complete);
}

#[tokio::test]
async fn real_symlinks_and_non_utf8_names_are_reported_and_never_followed() {
    let lab = Lab::new("skips");
    lab.write("real.txt", b"data");
    std::os::unix::fs::symlink("/etc/hostname", lab.root_dir.join("link")).expect("建符号链接");
    let odd = lab
        .root_dir
        .join(std::ffi::OsStr::from_bytes(b"bad-\xff-name"));
    fs::write(&odd, b"odd").expect("写非 UTF-8 名字文件");
    let mut env = Env::new(&lab);
    let first = env.pass(0, 1_000).await;
    assert!(
        first.skipped.iter().any(|skip| skip.path == "link"),
        "符号链接可见但不跟随"
    );
    assert!(
        first.skipped.iter().any(|skip| skip.path.is_empty()),
        "不可表示的名字可见"
    );
    let adopted = env.pass(WINDOW_MS, 1_000).await;
    assert!(matches!(
        state_of(&adopted, "real.txt"),
        FileState::Adopted { .. }
    ));
    assert_eq!(env.store.versions.len(), 1, "只采纳普通文件");
}

#[tokio::test]
async fn real_truncated_scan_resumes_and_completes() {
    let lab = Lab::new("budget");
    for index in 0..6 {
        lab.write(
            &format!("d{}/f{index}.txt", index % 2),
            format!("c{index}").as_bytes(),
        );
    }
    let mut env = Env::new(&lab);
    let mut incomplete = 0;
    for _ in 0..20 {
        let report = env.pass(WINDOW_MS, 3).await;
        if !report.scan_complete {
            incomplete += 1;
            assert!(!report.initial_import_complete);
        }
        if report.initial_import_complete {
            break;
        }
    }
    assert!(incomplete > 0, "预算 3 必须触发截断");
    assert_eq!(env.store.versions.len(), 6);
    assert!(env.observer.initial_import_complete());
}

#[tokio::test]
async fn real_object_copy_method_matches_filesystem_capability() {
    // 在 btrfs 测试卷上（VAULTSYNCD_TEST_DIR 指向它）应走 reflink；其它文件系统走校验复制并给出原因。
    let lab = Lab::new("method");
    lab.write("a.bin", &vec![7u8; 64 * 1024]);
    let mut env = Env::new(&lab);
    let report = env.settle().await;
    let FileState::Adopted {
        copy_method,
        copy_fallback,
        ..
    } = state_of(&report, "a.bin")
    else {
        panic!("应采纳");
    };
    match copy_method {
        CopyMethod::Reflink => assert!(copy_fallback.is_none()),
        CopyMethod::VerifiedCopy => assert!(copy_fallback.is_some(), "退回复制必须暴露原因"),
    }
    if std::env::var_os("VAULTSYNCD_EXPECT_REFLINK").is_some() {
        assert_eq!(
            *copy_method,
            CopyMethod::Reflink,
            "btrfs 测试卷上必须是 reflink"
        );
    }
    println!("method={copy_method} fallback={copy_fallback:?}");
}
