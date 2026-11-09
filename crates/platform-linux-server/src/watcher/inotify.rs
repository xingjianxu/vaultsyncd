//! 目录树的 inotify 监视（design §12.1）：把内核事件整理成**变化线索**。
//!
//! 事件只是线索，不是事实（design §12.1）：它们可能丢失（队列溢出）、合并、不递归、在监视建立之
//! 前就已经发生。因此本模块：
//!
//! - 只输出 [`WatchSignal`]——“这个路径值得再看一眼”，**不**输出“文件被删除/已写完”之类的结论；
//! - 递归登记监视（每个目录一个 watch），新目录出现时先登记再发 [`WatchSignal::Subtree`]，要求
//!   消费者补扫该目录（监视建立之前已创建的内容不会有事件）；
//! - 内核队列溢出（`IN_Q_OVERFLOW`）时丢弃整张监视表并重建，再发 [`WatchSignal::Overflow`]：
//!   消费者必须做全量补扫，因为被丢弃的事件里可能有新目录、改名或任何内容变化；
//! - 登记不了的目录（权限不足、监视数上限、并发消失）以 [`WatchSignal::Gap`] 报告，**不是删除**，
//!   也不静默跳过：这些目录只能靠周期扫描覆盖；
//! - 单次读取的信号数有上限，同一路径在一次读取内合并。
//!
//! 监视的是已打开的目录句柄（`/proc/self/fd/N`），不按路径字符串重新解析：路径被换成符号链接或
//! 别的目录也带不到 Vault 之外（design §16.1）。不识别“谁写的”：忽略本服务自己的写入由核心按
//! 身份与意图核对，不在这里按时间窗口过滤。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use crate::files::restricted::{EntryKind, RestrictError};
use crate::files::vault::VaultHandle;
use crate::watcher::raw::{self, RawEvent, errno};

/// 监视参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchConfig {
    /// 最多登记的目录监视数（内核另有 `max_user_watches`）。超出的目录报 [`GapReason::WatchLimit`]。
    pub max_watches: usize,
    /// 一次 [`DirWatcher::drain`] 最多返回的信号数；多出的事件留在内核队列里下次读取（队列满则溢出）。
    pub max_signals_per_poll: usize,
}

impl WatchConfig {
    /// 保守默认值：65536 个目录监视、每次最多 4096 个信号。
    pub const DEFAULT: WatchConfig = WatchConfig {
        max_watches: 65_536,
        max_signals_per_poll: 4_096,
    };
}

/// 目录没有被监视的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapReason {
    /// 没有读/进入权限：不是删除，权限恢复后父目录会有属性事件。
    PermissionDenied,
    /// 达到监视数上限（本配置或内核 `max_user_watches`）。
    WatchLimit,
    /// 登记时目录已不存在或已不是目录（并发改名/删除）。
    Vanished,
    /// 其他原因（挂载漂移、IO 错误等），见详情文本。
    Other,
}

impl GapReason {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GapReason::PermissionDenied => "permission_denied",
            GapReason::WatchLimit => "watch_limit",
            GapReason::Vanished => "vanished",
            GapReason::Other => "other",
        }
    }
}

/// 给消费者的变化线索。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchSignal {
    /// 这个路径（文件或目录，Vault 内相对路径，空串为 Vault 自身）上发生过事件，值得重新观察。
    Changed {
        /// 相对路径。
        path: String,
    },
    /// 这个目录是新出现（或权限恢复后第一次可监视）的：监视已登记，但它的内容在登记之前就可能
    /// 存在，必须补扫整个子树。
    Subtree {
        /// 相对路径。
        path: String,
    },
    /// 内核事件队列溢出，已有事件被丢弃；监视表已重建。必须立即全量补扫。
    Overflow,
    /// 该目录没有被监视：只能靠周期扫描覆盖。
    Gap {
        /// 相对路径。
        path: String,
        /// 原因分类。
        reason: GapReason,
        /// 原始错误文本。
        detail: String,
    },
}

/// 监视失败。
#[derive(Debug)]
pub enum WatchError {
    /// 系统调用失败。
    Io {
        /// 出错阶段。
        stage: &'static str,
        /// 原始错误。
        source: io::Error,
    },
    /// 内核事件字节无法解析。
    Parse(String),
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WatchError::Io { stage, source } => write!(f, "inotify {stage} 失败：{source}"),
            WatchError::Parse(detail) => write!(f, "inotify 事件无法解析：{detail}"),
        }
    }
}

impl std::error::Error for WatchError {}

fn io_error(stage: &'static str) -> impl FnOnce(io::Error) -> WatchError {
    move |source| WatchError::Io { stage, source }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// 一个 Vault 目录树的 inotify 监视器。
pub struct DirWatcher {
    fd: OwnedFd,
    config: WatchConfig,
    by_wd: BTreeMap<i32, String>,
    by_path: BTreeMap<String, i32>,
}

impl DirWatcher {
    /// 建立实例并递归登记整个 Vault。
    ///
    /// 返回的信号只含 [`WatchSignal::Gap`]（登记不了的目录）。**调用方在监视建立之后必须做一次
    /// 全量扫描**——建立之前发生的变化没有事件。
    ///
    /// # Errors
    ///
    /// 创建 inotify 实例失败（例如 `max_user_instances` 耗尽）。
    pub fn open(
        vault: &VaultHandle,
        config: WatchConfig,
    ) -> Result<(Self, Vec<WatchSignal>), WatchError> {
        let fd = raw::init().map_err(io_error("init"))?;
        let mut watcher = DirWatcher {
            fd,
            config,
            by_wd: BTreeMap::new(),
            by_path: BTreeMap::new(),
        };
        let mut gaps = Vec::new();
        watcher.add_tree(vault, "", &mut gaps);
        Ok((watcher, gaps))
    }

    /// 当前登记的目录监视数。
    #[must_use]
    pub fn watch_count(&self) -> usize {
        self.by_wd.len()
    }

    /// 某个目录当前是否被监视。
    #[must_use]
    pub fn is_watching(&self, dir: &str) -> bool {
        self.by_path.contains_key(dir)
    }

    /// 等待最多 `timeout` 后读取事件；超时无事件返回空列表。
    ///
    /// # Errors
    ///
    /// 等待或读取失败、事件无法解析。
    pub fn poll(
        &mut self,
        vault: &VaultHandle,
        timeout: Duration,
    ) -> Result<Vec<WatchSignal>, WatchError> {
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        if !raw::wait_readable(self.fd.as_raw_fd(), millis).map_err(io_error("poll"))? {
            return Ok(Vec::new());
        }
        self.drain(vault)
    }

    /// 读取当前队列里的事件（不等待）。
    ///
    /// # Errors
    ///
    /// 读取失败、事件无法解析。
    pub fn drain(&mut self, vault: &VaultHandle) -> Result<Vec<WatchSignal>, WatchError> {
        let mut out = Vec::new();
        let mut seen: BTreeSet<(bool, String)> = BTreeSet::new();
        let mut overflow = false;
        let mut buffer = vec![0u8; 64 * 1024];
        while out.len() < self.config.max_signals_per_poll {
            let filled =
                raw::read_events(self.fd.as_raw_fd(), &mut buffer).map_err(io_error("read"))?;
            if filled == 0 {
                break;
            }
            let events =
                raw::parse_events(&buffer[..filled]).map_err(|error| WatchError::Parse(error.0))?;
            for event in events {
                if event.mask & raw::IN_Q_OVERFLOW != 0 {
                    overflow = true;
                    continue;
                }
                self.handle(vault, &event, &mut out, &mut seen);
            }
        }
        if overflow {
            // 被丢弃的事件里可能有新目录或改名：旧监视表不可信，整张重建。
            self.reset(vault, &mut out);
            out.push(WatchSignal::Overflow);
        }
        Ok(out)
    }

    fn emit(
        out: &mut Vec<WatchSignal>,
        seen: &mut BTreeSet<(bool, String)>,
        subtree: bool,
        path: String,
    ) {
        if seen.insert((subtree, path.clone())) {
            out.push(if subtree {
                WatchSignal::Subtree { path }
            } else {
                WatchSignal::Changed { path }
            });
        }
    }

    fn handle(
        &mut self,
        vault: &VaultHandle,
        event: &RawEvent,
        out: &mut Vec<WatchSignal>,
        seen: &mut BTreeSet<(bool, String)>,
    ) {
        let Some(dir) = self.by_wd.get(&event.wd).cloned() else {
            // 已被我们移除的监视的尾部事件。
            return;
        };
        if event.mask & raw::IN_IGNORED != 0 {
            self.by_wd.remove(&event.wd);
            if self.by_path.get(&dir) == Some(&event.wd) {
                self.by_path.remove(&dir);
            }
            Self::emit(out, seen, false, dir);
            return;
        }
        if event.mask & (raw::IN_DELETE_SELF | raw::IN_MOVE_SELF | raw::IN_UNMOUNT) != 0 {
            self.forget_subtree(&dir);
            Self::emit(out, seen, false, dir);
            return;
        }
        if event.name.is_empty() {
            Self::emit(out, seen, false, dir);
            return;
        }
        let Ok(name) = std::str::from_utf8(&event.name) else {
            // 非 UTF-8 名字无法表示：让消费者看一眼所在目录（会被报告为跳过项）。
            Self::emit(out, seen, false, dir);
            return;
        };
        let path = join(&dir, name);
        if event.mask & raw::IN_ISDIR == 0 {
            Self::emit(out, seen, false, path);
            return;
        }
        if event.mask & (raw::IN_MOVED_FROM | raw::IN_DELETE) != 0 {
            self.forget_subtree(&path);
            Self::emit(out, seen, false, path);
            return;
        }
        let newly = !self.by_path.contains_key(&path);
        if newly {
            let mut gaps = Vec::new();
            self.add_tree(vault, &path, &mut gaps);
            out.extend(gaps);
        }
        let created = event.mask & (raw::IN_CREATE | raw::IN_MOVED_TO) != 0;
        Self::emit(out, seen, newly || created, path);
    }

    fn reset(&mut self, vault: &VaultHandle, out: &mut Vec<WatchSignal>) {
        let fd = self.fd.as_raw_fd();
        for wd in self.by_wd.keys() {
            let _ = raw::rm_watch(fd, *wd);
        }
        self.by_wd.clear();
        self.by_path.clear();
        let mut gaps = Vec::new();
        self.add_tree(vault, "", &mut gaps);
        out.extend(gaps);
    }

    fn forget_subtree(&mut self, dir: &str) {
        let prefix = format!("{dir}/");
        let doomed: Vec<String> = self
            .by_path
            .keys()
            .filter(|path| path.as_str() == dir || path.starts_with(&prefix))
            .cloned()
            .collect();
        let fd = self.fd.as_raw_fd();
        for path in doomed {
            if let Some(wd) = self.by_path.remove(&path) {
                self.by_wd.remove(&wd);
                let _ = raw::rm_watch(fd, wd);
            }
        }
    }

    /// 递归登记 `start` 及其子目录。先登记、后列目录：登记之后新建的目录会有事件，登记之前已存在
    /// 的目录会出现在随后的列表里，两者之间不留缺口。
    fn add_tree(&mut self, vault: &VaultHandle, start: &str, gaps: &mut Vec<WatchSignal>) {
        let mut stack = vec![start.to_owned()];
        while let Some(dir) = stack.pop() {
            if let Err(gap) = self.ensure_watch(vault, &dir) {
                gaps.push(gap);
                continue;
            }
            match vault.list_dir(Path::new(&dir)) {
                Ok(entries) => {
                    for entry in entries {
                        if entry.kind != EntryKind::Directory {
                            continue;
                        }
                        if let Some(name) = entry.name.to_str() {
                            stack.push(join(&dir, name));
                        }
                    }
                }
                Err(error) => gaps.push(gap_of_restrict(&dir, &error)),
            }
        }
    }

    fn ensure_watch(&mut self, vault: &VaultHandle, dir: &str) -> Result<(), WatchSignal> {
        if self.by_path.contains_key(dir) {
            return Ok(());
        }
        let gap = |reason: GapReason, detail: String| WatchSignal::Gap {
            path: dir.to_owned(),
            reason,
            detail,
        };
        if self.by_wd.len() >= self.config.max_watches {
            return Err(gap(
                GapReason::WatchLimit,
                format!("已达到监视数上限 {}", self.config.max_watches),
            ));
        }
        // 句柄必须活到 add_watch 返回：/proc/self/fd/N 指向它。
        let (target, _held) = if dir.is_empty() {
            (vault.proc_fd_path(), None)
        } else {
            let held = vault
                .open_dir(Path::new(dir))
                .map_err(|error| gap_of_restrict(dir, &error))?;
            (held.proc_fd_path(), Some(held))
        };
        let c_path = CString::new(target.to_string_lossy().into_owned())
            .map_err(|error| gap(GapReason::Other, error.to_string()))?;
        match raw::add_watch(self.fd.as_raw_fd(), &c_path, raw::WATCH_MASK) {
            Ok(wd) => {
                if let Some(previous) = self.by_wd.insert(wd, dir.to_owned()) {
                    if previous != dir {
                        self.by_path.remove(&previous);
                    }
                }
                self.by_path.insert(dir.to_owned(), wd);
                Ok(())
            }
            Err(error) => {
                let reason = match error.raw_os_error() {
                    Some(errno::EACCES) => GapReason::PermissionDenied,
                    Some(errno::ENOSPC) => GapReason::WatchLimit,
                    Some(errno::ENOENT | errno::ENOTDIR) => GapReason::Vanished,
                    _ => GapReason::Other,
                };
                Err(gap(reason, error.to_string()))
            }
        }
    }
}

fn gap_of_restrict(dir: &str, error: &RestrictError) -> WatchSignal {
    let reason = match error {
        RestrictError::PermissionDenied { .. } => GapReason::PermissionDenied,
        RestrictError::NotFound { .. } | RestrictError::NotDirectory { .. } => GapReason::Vanished,
        _ => GapReason::Other,
    };
    WatchSignal::Gap {
        path: dir.to_owned(),
        reason,
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests;
