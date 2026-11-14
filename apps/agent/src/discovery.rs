//! Bounded local hints and content-audit batches, independent of network polling.
use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use vaultsyncd_platform_windows::{
    files::{EntryMetadata, VaultHandle},
    watcher::{DirectoryWatcher, RescanReason, WatchBatch},
};

const RECHECK: Duration = Duration::from_secs(60);
const DEBOUNCE: Duration = Duration::from_millis(300);
const SCAN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_HINTS: usize = 4096;
const URGENT_BURST: usize = 4;

#[derive(Default)]
struct Hints {
    ready: bool,
    lost: bool,
    full: bool,
    paths: HashSet<String>,
    first: Option<Instant>,
    due: Option<Instant>,
    not_before: Option<Instant>,
    retry_delay: Duration,
    attempt_generation: Option<u64>,
}
impl Hints {
    fn request_full(&mut self, now: Instant) {
        self.full = true;
        self.paths.clear();
        self.first = None;
        self.due = Some(now);
    }
    fn retry(&mut self, now: Instant) {
        self.attempt_generation = None;
        self.retry_delay = (self.retry_delay * 2).max(SCAN_INTERVAL).min(RECHECK);
        self.not_before = Some(now + self.retry_delay);
        self.request_full(now);
    }
    fn record(&mut self, batch: WatchBatch, now: Instant) {
        self.ready = true;
        match batch {
            WatchBatch::Idle => {}
            WatchBatch::Rescan(reason) => {
                self.lost |= reason == RescanReason::WatchLost;
                self.request_full(now);
            }
            WatchBatch::Changes(changes) if !self.full && !changes.is_empty() => {
                let first = *self.first.get_or_insert(now);
                self.due = Some((now + DEBOUNCE).min(first + Duration::from_secs(1)));
                for change in changes {
                    if self.paths.len() == MAX_HINTS && !self.paths.contains(&change.path) {
                        self.request_full(now);
                        break;
                    }
                    self.paths.insert(change.path);
                }
            }
            WatchBatch::Changes(_) => {}
        }
    }
}

fn resident(meta: &EntryMetadata) -> bool {
    !meta.directory
        && !meta
            .cloud
            .as_ref()
            .is_some_and(|cloud| cloud.on_disk < meta.size)
}
fn hinted(path: &str, hints: &HashSet<String>) -> bool {
    // Check only this path and its component ancestors, not all M hints per file.
    hints.contains(path)
        || path
            .match_indices('/')
            .any(|(end, _)| hints.contains(&path[..end]))
}

pub(crate) struct Plan {
    pub scan: HashMap<String, EntryMetadata>,
    pub snapshot: Option<Arc<crate::work::AuditSnapshot>>,
    queue: VecDeque<String>,
    urgent: VecDeque<String>,
    queued: HashSet<String>,
    urgent_streak: usize,
    audit_in_progress: bool,
    full_requested: Cell<bool>,
    next_scan: Instant,
    generation: u64,
    pub force: bool,
}
impl Plan {
    fn new() -> Self {
        Self::new_at(Instant::now())
    }
    fn new_at(now: Instant) -> Self {
        Self {
            scan: HashMap::new(),
            snapshot: None,
            queue: VecDeque::new(),
            urgent: VecDeque::new(),
            queued: HashSet::new(),
            urgent_streak: 0,
            audit_in_progress: false,
            full_requested: Cell::new(false),
            next_scan: now,
            generation: 0,
            force: true,
        }
    }
    /// Empty hints mean full audit; ordinary hints preserve the completed audit work.
    pub fn install(&mut self, scan: HashMap<String, EntryMetadata>, hints: &HashSet<String>) {
        self.install_at(scan, hints, Instant::now());
    }
    fn install_at(
        &mut self,
        scan: HashMap<String, EntryMetadata>,
        hints: &HashSet<String>,
        now: Instant,
    ) {
        // A periodic metadata sweep must not re-add the already-completed head of
        // an audit that takes longer than RECHECK. Only an explicit invalidation
        // or lost-watch reconciliation may restart such an in-progress audit.
        let full = self.full_requested.replace(false)
            || (!self.audit_in_progress && (hints.is_empty() || now >= self.next_scan));
        self.audit_in_progress |= full;
        let urgent: HashSet<_> = scan
            .iter()
            .filter(|(path, meta)| {
                resident(meta) && (hinted(path, hints) || self.scan.get(*path) != Some(*meta))
            })
            .map(|(path, _)| path.clone())
            .collect();
        // One stable partition. Repeated hints do not move an existing urgent item
        // to the front, and unrelated audit work keeps its original FIFO position.
        self.urgent
            .retain(|path| scan.get(path).is_some_and(resident));
        let mut queue = VecDeque::new();
        for path in self.queue.drain(..) {
            if !scan.get(&path).is_some_and(resident) {
                continue;
            }
            if urgent.contains(&path) {
                self.urgent.push_back(path);
            } else {
                queue.push_back(path);
            }
        }
        self.queue = queue;
        self.queued = self.queue.iter().chain(&self.urgent).cloned().collect();
        let mut additions: Vec<_> = scan
            .iter()
            .filter(|(path, meta)| {
                resident(meta) && !self.queued.contains(*path) && (full || urgent.contains(*path))
            })
            .map(|(path, _)| path.clone())
            .collect();
        additions.sort();
        for path in additions {
            self.queued.insert(path.clone());
            if urgent.contains(&path) {
                self.urgent.push_back(path);
            } else {
                self.queue.push_back(path);
            }
        }
        self.scan = scan;
        if full || now >= self.next_scan {
            self.next_scan = now + RECHECK;
        }
        self.finish_audit(now);
        self.generation = self
            .generation
            .checked_add(1)
            .expect("scan generation exhausted");
        self.force = false;
    }
    fn urgent_next(&self) -> bool {
        !self.urgent.is_empty() && (self.queue.is_empty() || self.urgent_streak < URGENT_BURST)
    }
    fn has_work(&self) -> bool {
        !self.queue.is_empty() || !self.urgent.is_empty()
    }
    pub fn next(&self) -> Option<&str> {
        if self.urgent_next() {
            self.urgent.front()
        } else {
            self.queue.front()
        }
        .map(String::as_str)
    }
    pub fn complete(&mut self) {
        self.complete_at(Instant::now());
    }
    fn complete_at(&mut self, now: Instant) {
        let path = if self.urgent_next() {
            self.urgent_streak = (self.urgent_streak + 1).min(URGENT_BURST);
            self.urgent.pop_front()
        } else {
            self.urgent_streak = 0;
            self.queue.pop_front()
        };
        if let Some(path) = path {
            self.queued.remove(&path);
        }
        self.finish_audit(now);
    }
    fn finish_audit(&mut self, now: Instant) {
        if self.audit_in_progress && !self.has_work() {
            self.audit_in_progress = false;
            self.next_scan = now + RECHECK;
        }
    }
}

pub(crate) struct Discovery {
    pub plan: tokio::sync::Mutex<Plan>,
    hints: Arc<Mutex<Hints>>,
    pending: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}
impl Discovery {
    pub fn new(vault: Arc<VaultHandle>, cancelled: Arc<AtomicBool>) -> std::io::Result<Self> {
        let hints = Arc::new(Mutex::new(Hints::default()));
        let pending = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let (h, p, s) = (hints.clone(), pending.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("local-file-hints".into())
            .spawn(move || {
                while !s.load(Ordering::Acquire) && !cancelled.load(Ordering::Acquire) {
                    let mut watcher = match DirectoryWatcher::new(&vault, 65536) {
                        Ok(watcher) => {
                            let mut state = h.lock().unwrap_or_else(|e| e.into_inner());
                            state.ready = true;
                            state.lost = false;
                            state.request_full(Instant::now());
                            p.store(true, Ordering::Release);
                            watcher
                        }
                        Err(cause) => {
                            {
                                let mut state = h.lock().unwrap_or_else(|e| e.into_inner());
                                state.ready = true;
                                if !state.lost {
                                    eprintln!("event=local-watch-unavailable reason={cause}");
                                    state.request_full(Instant::now());
                                }
                                state.lost = true;
                                p.store(true, Ordering::Release);
                            }
                            if !watch_retry(&s, &cancelled) {
                                return;
                            }
                            continue;
                        }
                    };
                    while !s.load(Ordering::Acquire) && !cancelled.load(Ordering::Acquire) {
                        let batch = watcher.poll(100);
                        if matches!(batch, WatchBatch::Idle) {
                            continue;
                        }
                        // Native poll already rearms on overflow or malformed notifications.
                        let restart = matches!(batch, WatchBatch::Rescan(RescanReason::WatchLost));
                        {
                            let mut state = h.lock().unwrap_or_else(|e| e.into_inner());
                            state.record(batch, Instant::now());
                            p.store(true, Ordering::Release);
                        }
                        if restart {
                            break;
                        }
                    }
                    drop(watcher);
                    if !watch_retry(&s, &cancelled) {
                        return;
                    }
                }
            })?;
        Ok(Self {
            plan: tokio::sync::Mutex::new(Plan::new()),
            hints,
            pending,
            stop,
            thread: Mutex::new(Some(thread)),
        })
    }
    pub fn pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
    /// Invalidate cached discovery after scheduler work or an applied remote batch.
    pub async fn invalidate(&self) {
        let mut plan = self.plan.lock().await;
        plan.force = true;
        plan.snapshot = None;
        self.pending.store(true, Ordering::Release);
    }
    /// None: debounce, retry backoff or idle. Some: batch allowed; bool requests scan.
    /// Empty hints request an audit; periodic scans preserve an audit already in progress.
    pub fn due(&self, plan: &Plan) -> Option<(bool, HashSet<String>)> {
        self.due_at(plan, Instant::now())
    }
    fn due_at(&self, plan: &Plan, now: Instant) -> Option<(bool, HashSet<String>)> {
        let mut hints = self.hints.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(attempt) = hints.attempt_generation.take() {
            if attempt == plan.generation {
                // Also covers early returns before finished(), including namespace ACK wait.
                hints.retry(now);
            } else {
                hints.retry_delay = Duration::ZERO;
            }
        }
        // Do not let once-mode startup depend on when the watch thread is scheduled.
        // finished() keeps pending set until that thread has established its watch.
        if !hints.ready && !plan.force {
            return None;
        }
        let events_due = hints.due.is_some_and(|due| now >= due);
        let periodic = now >= plan.next_scan;
        let scan = plan.force || periodic || events_due;
        if scan && hints.not_before.is_some_and(|deadline| now < deadline) {
            return None;
        }
        if !scan && !plan.has_work() {
            return None;
        }
        self.pending.store(true, Ordering::Release);
        let paths = if scan {
            let full = plan.force || hints.full || (periodic && !plan.audit_in_progress);
            plan.full_requested.set(plan.force || hints.full);
            let paths = if full {
                HashSet::new()
            } else {
                std::mem::take(&mut hints.paths)
            };
            hints.paths.clear();
            hints.due = None;
            hints.first = None;
            hints.full = false;
            hints.attempt_generation = Some(plan.generation);
            hints.not_before = Some(now + SCAN_INTERVAL);
            paths
        } else {
            HashSet::new()
        };
        Some((scan, paths))
    }
    pub fn finished(&self, plan: &Plan) {
        self.finished_at(plan, Instant::now());
    }
    fn finished_at(&self, plan: &Plan, now: Instant) {
        let mut hints = self.hints.lock().unwrap_or_else(|e| e.into_inner());
        if plan.force || hints.attempt_generation == Some(plan.generation) {
            hints.retry(now);
        } else if hints.attempt_generation.take().is_some() {
            hints.retry_delay = Duration::ZERO;
        }
        self.pending.store(
            plan.force
                || plan.has_work()
                || !hints.ready
                || hints.lost
                || hints.full
                || hints.due.is_some(),
            Ordering::Release,
        );
    }
    pub async fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        let thread = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(thread) = thread {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}
fn watch_retry(stop: &AtomicBool, cancelled: &AtomicBool) -> bool {
    for _ in 0..10 {
        if stop.load(Ordering::Acquire) || cancelled.load(Ordering::Acquire) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}
impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self
            .thread
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vaultsyncd_platform_windows::{
        files::{CloudMetadata, FileIdentity},
        watcher::Change,
    };
    fn file() -> EntryMetadata {
        EntryMetadata {
            identity: FileIdentity {
                volume: 1,
                file: [1; 16],
            },
            size: 5,
            allocated: 5,
            directory: false,
            attributes: 0,
            reparse_tag: 0,
            links: 1,
            created: 1,
            modified: 1,
            cloud: None,
        }
    }
    fn scan(names: &[&str]) -> HashMap<String, EntryMetadata> {
        names.iter().map(|name| ((*name).into(), file())).collect()
    }
    fn changes(paths: &[&str]) -> WatchBatch {
        WatchBatch::Changes(
            paths
                .iter()
                .map(|path| Change {
                    path: (*path).into(),
                    action: 3,
                })
                .collect(),
        )
    }
    fn discovery(now: Instant) -> Discovery {
        Discovery {
            plan: tokio::sync::Mutex::new(Plan::new_at(now)),
            hints: Arc::new(Mutex::new(Hints::default())),
            pending: Arc::new(AtomicBool::new(true)),
            stop: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
        }
    }
    fn drain(plan: &mut Plan, now: Instant) -> Vec<String> {
        let mut result = Vec::new();
        while let Some(path) = plan.next().map(str::to_owned) {
            result.push(path);
            plan.complete_at(now);
        }
        assert!(plan.queued.is_empty());
        result
    }
    #[test]
    fn debounce_is_bounded_and_full_scan_cannot_be_delayed_by_hints() {
        let now = Instant::now();
        let mut hints = Hints::default();
        for i in 0..20 {
            hints.record(changes(&["a"]), now + Duration::from_millis(i * 100));
        }
        assert_eq!(hints.paths.len(), 1);
        assert!(hints.due.unwrap() <= now + Duration::from_secs(1));
        hints.record(WatchBatch::Rescan(RescanReason::Overflow), now);
        hints.record(changes(&["b"]), now + DEBOUNCE);
        assert!(hints.full && hints.paths.is_empty());
        assert_eq!(hints.due, Some(now));
    }
    #[test]
    fn hint_capacity_counts_unique_paths() {
        let now = Instant::now();
        let mut hints = Hints::default();
        hints.record(
            WatchBatch::Changes(
                (0..MAX_HINTS)
                    .map(|n| Change {
                        path: n.to_string(),
                        action: 1,
                    })
                    .collect(),
            ),
            now,
        );
        hints.record(changes(&["0"]), now);
        assert_eq!(hints.paths.len(), MAX_HINTS);
        assert!(!hints.full);
        hints.record(changes(&["new"]), now);
        assert!(hints.full && hints.paths.is_empty());
    }
    #[test]
    fn hints_only_requeue_affected_or_changed_files_without_postponing_audit() {
        let now = Instant::now();
        let mut plan = Plan::new_at(now);
        let mut files = scan(&["a/one", "a/two", "ab/three", "other", "changed"]);
        plan.install_at(files.clone(), &HashSet::new(), now);
        assert_eq!(drain(&mut plan, now).len(), 5);
        files.get_mut("changed").unwrap().modified += 1;
        plan.install_at(files.clone(), &HashSet::from(["a".into()]), now + DEBOUNCE);
        assert_eq!(
            drain(&mut plan, now).into_iter().collect::<HashSet<_>>(),
            HashSet::from(["a/one".into(), "a/two".into(), "changed".into()])
        );
        for second in 1..60 {
            plan.install_at(
                files.clone(),
                &HashSet::from(["other".into()]),
                now + Duration::from_secs(second),
            );
            assert_eq!(drain(&mut plan, now), vec!["other"]);
        }
        assert_eq!(plan.next_scan, now + RECHECK);
        plan.install_at(files, &HashSet::from(["other".into()]), now + RECHECK);
        assert_eq!(drain(&mut plan, now).len(), 5);
    }
    #[test]
    fn install_prunes_nonresident_deleted_and_directory_paths_from_both_queues() {
        let now = Instant::now();
        let mut plan = Plan::new_at(now);
        let mut files = scan(&["deleted", "directory", "partial", "kept"]);
        plan.install_at(files.clone(), &HashSet::new(), now);
        drain(&mut plan, now);
        plan.install_at(files.clone(), &HashSet::new(), now);
        assert!(!plan.queue.is_empty());
        plan.install_at(files.clone(), &HashSet::from(["partial".into()]), now);
        assert!(!plan.urgent.is_empty());
        files.remove("deleted");
        files.get_mut("directory").unwrap().directory = true;
        files.get_mut("partial").unwrap().cloud = Some(CloudMetadata {
            on_disk: 1,
            modified: 0,
            pin_state: 0,
            in_sync_state: 0,
            identity: vec![],
        });
        plan.install_at(files, &HashSet::new(), now);
        assert_eq!(drain(&mut plan, now), vec!["kept"]);
    }
    #[test]
    fn hot_hints_do_not_starve_the_old_audit_tail_or_duplicate_entries() {
        let now = Instant::now();
        let mut plan = Plan::new_at(now);
        let files = scan(&["hot", "old/a", "old/b", "old/c"]);
        plan.install_at(files.clone(), &HashSet::new(), now);
        drain(&mut plan, now);
        plan.install_at(files.clone(), &HashSet::new(), now);
        let mut visited = HashSet::new();
        for _ in 0..(URGENT_BURST + 1) * 3 {
            plan.install_at(files.clone(), &HashSet::from(["hot".into()]), now);
            assert_eq!(plan.queued.len(), plan.queue.len() + plan.urgent.len());
            visited.insert(plan.next().unwrap().to_owned());
            plan.complete();
        }
        assert!(visited.is_superset(&HashSet::from([
            "old/a".into(),
            "old/b".into(),
            "old/c".into()
        ])));
    }
    #[test]
    fn failed_scan_backs_off_even_without_finished_and_startup_does_not_wait_for_watcher() {
        let now = Instant::now();
        let discovery = discovery(now);
        let plan = Plan::new_at(now);
        assert!(discovery.due_at(&plan, now).unwrap().0);
        for tick in 1..=8 {
            assert!(
                discovery
                    .due_at(&plan, now + Duration::from_millis(tick * 250))
                    .is_none()
            );
        }
        assert!(discovery.pending());
        assert!(
            discovery
                .due_at(&plan, now + Duration::from_millis(2250))
                .unwrap()
                .0
        );
        assert!(
            discovery
                .due_at(&plan, now + Duration::from_millis(2500))
                .is_none()
        );
        assert_eq!(
            discovery.hints.lock().unwrap().retry_delay,
            Duration::from_secs(4)
        );
        assert!(
            discovery
                .due_at(&plan, now + Duration::from_millis(6250))
                .is_none()
        );
        assert!(
            discovery
                .due_at(&plan, now + Duration::from_millis(6500))
                .unwrap()
                .0
        );
    }
    #[test]
    fn consumed_hints_survive_failure_before_caller_sets_force() {
        let now = Instant::now();
        let discovery = discovery(now);
        let mut plan = Plan::new_at(now);
        let files = scan(&["a", "b"]);
        plan.install_at(files.clone(), &HashSet::new(), now);
        drain(&mut plan, now);
        discovery.hints.lock().unwrap().record(changes(&["a"]), now);
        let (scan_due, paths) = discovery.due_at(&plan, now + DEBOUNCE).unwrap();
        assert!(scan_due);
        assert_eq!(paths, HashSet::from(["a".into()]));
        let retry_start = now + DEBOUNCE + Duration::from_millis(250);
        assert!(discovery.due_at(&plan, retry_start).is_none());
        let (scan_due, paths) = discovery
            .due_at(&plan, retry_start + SCAN_INTERVAL)
            .unwrap();
        assert!(scan_due && paths.is_empty());
        plan.install_at(files, &paths, retry_start + SCAN_INTERVAL);
        assert_eq!(drain(&mut plan, now).len(), 2);
    }
    #[tokio::test]
    async fn invalidate_preserves_pending_and_failure_backoff() {
        let now = Instant::now();
        let discovery = discovery(now);
        {
            let plan = discovery.plan.lock().await;
            assert!(discovery.due_at(&plan, now).unwrap().0);
            discovery.finished_at(&plan, now);
        }
        discovery.invalidate().await;
        let plan = discovery.plan.lock().await;
        assert!(plan.force && discovery.pending() && plan.snapshot.is_none());
        assert!(
            discovery
                .due_at(&plan, now + Duration::from_millis(250))
                .is_none()
        );
        assert!(discovery.due_at(&plan, now + SCAN_INTERVAL).unwrap().0);
    }
    #[test]
    fn periodic_sweeps_do_not_restart_an_audit_taking_more_than_sixty_seconds() {
        let now = Instant::now();
        let discovery = discovery(now);
        discovery.hints.lock().unwrap().ready = true;
        let mut plan = Plan::new_at(now);
        let files = scan(&["a", "b", "c", "d"]);
        plan.install_at(files.clone(), &HashSet::new(), now);
        assert_eq!(plan.next(), Some("a"));
        plan.complete_at(now);
        for (index, expected) in ["b", "c", "d"].into_iter().enumerate() {
            let current = now + Duration::from_secs((index as u64 + 1) * 61);
            let (scan_due, hints) = discovery.due_at(&plan, current).unwrap();
            assert!(scan_due && hints.is_empty());
            // work marks force while executing each scan, including periodic ones.
            plan.force = true;
            plan.install_at(files.clone(), &hints, current);
            assert_eq!(plan.queued.len(), 3 - index);
            assert_eq!(plan.next(), Some(expected));
            plan.complete_at(current);
            discovery.finished_at(&plan, current);
        }
        assert!(!plan.has_work() && !plan.audit_in_progress && !discovery.pending());
        assert_eq!(plan.next_scan, now + Duration::from_secs(183) + RECHECK);
        assert!(
            discovery
                .due_at(&plan, now + Duration::from_secs(242))
                .is_none()
        );
    }

    #[test]
    fn periodic_metadata_sweep_preserves_hints_for_already_audited_files() {
        let now = Instant::now();
        let discovery = discovery(now);
        let mut plan = Plan::new_at(now);
        let files = scan(&["a", "b"]);
        plan.install_at(files.clone(), &HashSet::new(), now);
        plan.complete_at(now);
        discovery
            .hints
            .lock()
            .unwrap()
            .record(changes(&["a"]), now + RECHECK);
        let (scan_due, hints) = discovery.due_at(&plan, now + RECHECK).unwrap();
        assert!(scan_due);
        assert_eq!(hints, HashSet::from(["a".into()]));
        plan.install_at(files, &hints, now + RECHECK);
        assert_eq!(drain(&mut plan, now + RECHECK), vec!["b", "a"]);
    }

    #[test]
    fn watch_startup_and_loss_keep_successful_scans_pending() {
        let now = Instant::now();
        let discovery = discovery(now);
        let mut plan = Plan::new_at(now);
        assert!(discovery.due_at(&plan, now).unwrap().0);
        plan.install_at(HashMap::new(), &HashSet::new(), now);
        discovery.finished_at(&plan, now);
        assert!(discovery.pending());
        discovery
            .hints
            .lock()
            .unwrap()
            .record(WatchBatch::Rescan(RescanReason::WatchLost), now);
        assert!(discovery.due_at(&plan, now + SCAN_INTERVAL).unwrap().0);
        plan.install_at(HashMap::new(), &HashSet::new(), now + SCAN_INTERVAL);
        discovery.finished_at(&plan, now + SCAN_INTERVAL);
        assert!(discovery.pending());
    }
}
