//! Root 句柄：受限目录句柄 + 注册时身份快照 + 每次访问的挂载边界与身份复核。
//!
//! design §11.4 要求“取得受限根句柄”，§11.5 要求“准入核对 btrfs 类型、FSID、子卷 ID、mount ID
//! 和目录身份”，§16.1 要求“运行时依然以根句柄为边界逐层安全访问”。本模块把这三条落成一个
//! 接口：
//!
//! - [`RootHandle::open`] 先 [`probe_path`] 读内核事实，再用 [`RestrictedDir`] 以
//!   `O_DIRECTORY|O_NOFOLLOW` 打开根句柄，并核对句柄身份与刚探测到的事实是同一个目录——注册期
//!   路径被替换就当场失败（[`RootError::ChangedDuringOpen`]），不把“两次打开不是同一个东西”当成功。
//! - 所有访问都经由根句柄按相对名称进行（见 [`RestrictedDir`]）：`openat2` 的
//!   `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS` 保证不逃出根、不跟随符号链接；逐段回退同样带
//!   `O_NOFOLLOW`。
//! - 每次打开/列举/创建之后复核**挂载身份**：对象的 `STATX_MNT_ID` 必须等于 Root 的挂载 ID，
//!   否则说明路径穿过了 bind mount/子挂载（[`RestrictError::MountBoundaryCrossed`]）；内核不提供
//!   挂载 ID 时按 `MountBoundaryUnverified` 失败，不用“无法确认”冒充“没有变化”。
//! - [`RootSnapshot`] 记录 FSID、子卷 ID、挂载 ID、设备与目录身份，可在之后任意时刻重新探测并
//!   比较（[`RootSnapshot::verify`]），漂移原因映射到 design 的 `ROOT_UNAVAILABLE`，不推导删除。
//!
//! 边界：本模块不判定后端是否合规（那是 [`crate::mounts::admission`] 的职责，`RootHandle`
//! 接受任何目录以便在注册流程之外复用），不实现递归扫描策略、不写数据库、不做 Epoch 流程。

use crate::files::identity::FileIdentity;
use crate::files::restricted::{
    self, DirEntry, InvalidRelativePath, OpenedObject, ResolveMechanism, ResolveMode,
    RestrictError, RestrictedDir,
};
use crate::mounts::admission::{self, BackendKind};
use crate::mounts::probe::{self, ProbeError, StorageFacts};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// 一个 Root 在注册时的身份快照。
///
/// 字段全部来自打开句柄与 `/proc/self/mountinfo` 的内核事实，不用路径前缀推断；`fsid` 与
/// `subvolume_id` 只有 btrfs 上才有，比较时按 `Option` 原样对比（“从有到无”也算变化）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootSnapshot {
    /// 规范路径（`canonicalize` 之后），也是后续复核时重新探测的路径。
    pub resolved_path: PathBuf,
    /// 后端分类（[`admission::classify_backend`] 的结论）。
    pub backend: BackendKind,
    /// `fstatfs(2)` 的类型幻数。
    pub fs_magic: i64,
    /// btrfs 文件系统 UUID；非 btrfs 或读不到时为 `None`。
    pub fsid: Option<[u8; 16]>,
    /// btrfs 子卷 ID；非 btrfs 或读不到时为 `None`。
    pub subvolume_id: Option<u64>,
    /// Root 所在挂载的挂载 ID（运行时每次访问都以它核对边界）。
    pub mount_id: u64,
    /// `statx` 设备主号。
    pub dev_major: u32,
    /// `statx` 设备次号。
    pub dev_minor: u32,
    /// 目录 inode（只在所属子卷内解释）。
    pub inode: u64,
    /// 创建时间；文件系统不提供时为 `None`。
    pub btime: Option<(i64, u32)>,
}

impl RootSnapshot {
    /// 从探测事实构造快照。
    ///
    /// # Panics
    ///
    /// 不会 panic；`mount_id` 缺失（内核未提供 `STATX_MNT_ID`）时这里是 `0`，因为
    /// [`RootHandle::open`] 在此之前已经拒绝该情形。
    #[must_use]
    pub fn of_facts(facts: &StorageFacts) -> Self {
        Self {
            resolved_path: facts.resolved_path.clone(),
            backend: admission::classify_backend(facts.fs_magic, &facts.fs_type),
            fs_magic: facts.fs_magic,
            fsid: facts.btrfs.as_ref().map(|btrfs| btrfs.fsid),
            subvolume_id: facts.btrfs.as_ref().and_then(|btrfs| btrfs.subvolume_id),
            mount_id: facts.mount.mount_id,
            dev_major: facts.device.major,
            dev_minor: facts.device.minor,
            inode: facts.inode,
            btime: facts.btime,
        }
    }

    /// 与句柄身份是否同一个目录（设备、inode、创建时间）。
    #[must_use]
    pub fn same_directory(&self, identity: &FileIdentity) -> bool {
        self.dev_major == identity.dev_major
            && self.dev_minor == identity.dev_minor
            && self.inode == identity.inode
            && self.btime == identity.btime
    }

    /// 渲染成稳定的 `key=value` 文本（供验收脚本保存后交给 [`RootSnapshot::parse_spec`]）。
    #[must_use]
    pub fn to_spec(&self) -> String {
        format!(
            "resolved_path={}\nbackend={}\nfs_magic={}\nfsid={}\nsubvolume_id={}\nmount_id={}\ndev_major={}\ndev_minor={}\ninode={}\nbtime={}\n",
            self.resolved_path.display(),
            self.backend.as_str(),
            self.fs_magic,
            self.fsid
                .map_or_else(|| "none".to_owned(), |fsid| hex_bytes(&fsid)),
            self.subvolume_id
                .map_or_else(|| "none".to_owned(), |id| id.to_string()),
            self.mount_id,
            self.dev_major,
            self.dev_minor,
            self.inode,
            self.btime.map_or_else(
                || "none".to_owned(),
                |(seconds, nanos)| format!("{seconds}.{nanos:09}")
            ),
        )
    }

    /// 解析 [`RootSnapshot::to_spec`] 产生的文本（未知字段忽略，缺失字段报错）。
    ///
    /// # Errors
    ///
    /// 缺字段、字段重复或取值无法解析时返回 [`RootSpecError`]，调用方必须当作用法错误。
    pub fn parse_spec(text: &str) -> Result<Self, RootSpecError> {
        const KEYS: [&str; 10] = [
            "resolved_path",
            "backend",
            "fs_magic",
            "fsid",
            "subvolume_id",
            "mount_id",
            "dev_major",
            "dev_minor",
            "inode",
            "btime",
        ];
        let mut seen: BTreeMap<&'static str, String> = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if let Some(known) = KEYS.iter().find(|candidate| **candidate == key) {
                if seen.insert(known, value.to_owned()).is_some() {
                    return Err(RootSpecError::DuplicateKey(known));
                }
            }
        }
        let required = |key: &'static str| -> Result<String, RootSpecError> {
            seen.get(key).cloned().ok_or(RootSpecError::MissingKey(key))
        };
        let invalid = |key: &'static str, value: &str| RootSpecError::InvalidValue {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        let parse_u32 = |key: &'static str| -> Result<u32, RootSpecError> {
            let raw = required(key)?;
            raw.parse::<u32>().map_err(|_| invalid(key, &raw))
        };
        let parse_u64 = |key: &'static str| -> Result<u64, RootSpecError> {
            let raw = required(key)?;
            raw.parse::<u64>().map_err(|_| invalid(key, &raw))
        };
        let resolved_path = PathBuf::from(required("resolved_path")?);
        let backend_raw = required("backend")?;
        let backend =
            BackendKind::parse(&backend_raw).ok_or_else(|| invalid("backend", &backend_raw))?;
        let fs_magic = {
            let raw = required("fs_magic")?;
            raw.parse::<i64>().map_err(|_| invalid("fs_magic", &raw))?
        };
        let fsid_raw = required("fsid")?;
        let fsid = if fsid_raw == "none" {
            None
        } else {
            Some(parse_fsid(&fsid_raw).ok_or_else(|| invalid("fsid", &fsid_raw))?)
        };
        let subvolume_raw = required("subvolume_id")?;
        let subvolume_id = if subvolume_raw == "none" {
            None
        } else {
            Some(
                subvolume_raw
                    .parse::<u64>()
                    .map_err(|_| invalid("subvolume_id", &subvolume_raw))?,
            )
        };
        let btime_raw = required("btime")?;
        let btime = if btime_raw == "none" {
            None
        } else {
            Some(parse_timestamp(&btime_raw).ok_or_else(|| invalid("btime", &btime_raw))?)
        };
        Ok(Self {
            resolved_path,
            backend,
            fs_magic,
            fsid,
            subvolume_id,
            mount_id: parse_u64("mount_id")?,
            dev_major: parse_u32("dev_major")?,
            dev_minor: parse_u32("dev_minor")?,
            inode: parse_u64("inode")?,
            btime,
        })
    }

    /// 重新探测 `path` 并与快照比较；首个漂移原因返回为错误。
    ///
    /// # Errors
    ///
    /// 探测本身失败时返回 [`ProbeError`]（“读不到”不当作“没变化”）；身份漂移时返回
    /// [`RootDrift`]。
    pub fn verify(&self, path: &Path) -> Result<(), RootSpecVerifyError> {
        let facts = probe::probe_path(path).map_err(RootSpecVerifyError::Probe)?;
        self.compare(&facts).map_err(RootSpecVerifyError::Drift)
    }

    /// 与新的探测事实比较；按“后端 → FSID → 子卷 → 挂载 → 目录”的顺序返回首个漂移。
    pub fn compare(&self, facts: &StorageFacts) -> Result<(), RootDrift> {
        let current = RootSnapshot::of_facts(facts);
        if current.backend != self.backend {
            return Err(RootDrift::BackendChanged);
        }
        if current.fsid != self.fsid {
            return Err(RootDrift::FilesystemChanged);
        }
        if current.subvolume_id != self.subvolume_id {
            return Err(RootDrift::SubvolumeChanged);
        }
        if current.mount_id != self.mount_id {
            return Err(RootDrift::MountChanged);
        }
        if current.dev_major != self.dev_major
            || current.dev_minor != self.dev_minor
            || current.inode != self.inode
            || current.btime != self.btime
        {
            return Err(RootDrift::DirectoryChanged);
        }
        Ok(())
    }
}

/// 快照规格解析错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootSpecError {
    /// 缺少字段。
    MissingKey(&'static str),
    /// 字段取值无法解析。
    InvalidValue {
        /// 字段名。
        key: String,
        /// 原始取值。
        value: String,
    },
    /// 字段重复出现。
    DuplicateKey(&'static str),
}

impl fmt::Display for RootSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootSpecError::MissingKey(key) => write!(f, "Root 规格缺少字段 {key}"),
            RootSpecError::InvalidValue { key, value } => {
                write!(f, "Root 规格字段 {key} 的取值 '{value}' 无法解析")
            }
            RootSpecError::DuplicateKey(key) => write!(f, "Root 规格字段 {key} 重复出现"),
        }
    }
}

impl std::error::Error for RootSpecError {}

/// Root 身份漂移：启动或挂载变化后重新核对时发现的差异（design §11.4—11.5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootDrift {
    /// 普通目录被换成了别的后端（例如 tmpfs、FUSE）。
    BackendChanged,
    /// btrfs 文件系统 UUID 变化（卷被替换或回滚）。
    FilesystemChanged,
    /// 子卷 ID 变化（子卷被替换）。
    SubvolumeChanged,
    /// 挂载 ID 变化（路径被 bind mount 覆盖或挂载被移动）。
    MountChanged,
    /// 目录身份（设备/inode/创建时间）变化（目录被替换）。
    DirectoryChanged,
}

impl RootDrift {
    /// 稳定原因码。
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            RootDrift::BackendChanged => "backend_changed",
            RootDrift::FilesystemChanged => "filesystem_changed",
            RootDrift::SubvolumeChanged => "subvolume_changed",
            RootDrift::MountChanged => "mount_changed",
            RootDrift::DirectoryChanged => "directory_changed",
        }
    }

    /// 对应的 design 错误码：运行阶段发现变化一律 `ROOT_UNAVAILABLE`，不推导删除。
    #[must_use]
    pub fn design_error_code(self) -> &'static str {
        "ROOT_UNAVAILABLE"
    }
}

impl fmt::Display for RootDrift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// 复核失败的三种来源：探测失败、句柄身份读取失败或身份漂移。
#[derive(Debug)]
pub enum RootSpecVerifyError {
    /// 重新探测失败（“读不到”不当作“没变化”）。
    Probe(ProbeError),
    /// 读取 Root 句柄身份失败。
    Handle(RestrictError),
    /// 身份漂移。
    Drift(RootDrift),
}

impl fmt::Display for RootSpecVerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootSpecVerifyError::Probe(error) => write!(f, "重新探测 Root 失败：{error}"),
            RootSpecVerifyError::Handle(error) => write!(f, "读取 Root 句柄身份失败：{error}"),
            RootSpecVerifyError::Drift(drift) => write!(f, "Root 身份已漂移：{}", drift.code()),
        }
    }
}

impl std::error::Error for RootSpecVerifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RootSpecVerifyError::Probe(error) => Some(error),
            RootSpecVerifyError::Handle(error) => Some(error),
            RootSpecVerifyError::Drift(_) => None,
        }
    }
}

/// 打开 Root 句柄时的失败。
#[derive(Debug)]
pub enum RootError {
    /// 探测目录事实失败。
    Probe(ProbeError),
    /// 打开受限目录句柄失败。
    Restrict(RestrictError),
    /// 内核未提供 `STATX_MNT_ID`（早于 5.8）：无法核对挂载边界，按 fail-closed 拒绝。
    MountIdUnavailable {
        /// 路径。
        path: PathBuf,
    },
    /// 探测与句柄打开之间目录被替换（注册期 TOCTOU）。
    ChangedDuringOpen {
        /// 路径。
        path: PathBuf,
        /// 句柄观察到的身份。
        handle: String,
        /// 探测事实观察到的身份。
        probed: String,
    },
}

impl RootError {
    /// 稳定原因码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            RootError::Probe(_) => "probe_failed",
            RootError::Restrict(error) => error.code(),
            RootError::MountIdUnavailable { .. } => "mount_id_unavailable",
            RootError::ChangedDuringOpen { .. } => "root_changed_during_open",
        }
    }
}

impl fmt::Display for RootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootError::Probe(error) => write!(f, "探测 Root 失败：{error}"),
            RootError::Restrict(error) => write!(f, "打开 Root 句柄失败：{error}"),
            RootError::MountIdUnavailable { path } => write!(
                f,
                "{} 的内核未提供 STATX_MNT_ID，无法核对挂载边界",
                path.display()
            ),
            RootError::ChangedDuringOpen {
                path,
                handle,
                probed,
            } => write!(
                f,
                "{} 在探测与打开之间被替换（句柄 {handle}，探测 {probed}）",
                path.display()
            ),
        }
    }
}

impl std::error::Error for RootError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RootError::Probe(error) => Some(error),
            RootError::Restrict(error) => Some(error),
            RootError::MountIdUnavailable { .. } | RootError::ChangedDuringOpen { .. } => None,
        }
    }
}

impl From<ProbeError> for RootError {
    fn from(error: ProbeError) -> Self {
        RootError::Probe(error)
    }
}

impl From<RestrictError> for RootError {
    fn from(error: RestrictError) -> Self {
        RootError::Restrict(error)
    }
}

/// 一个 Root 的正式访问接口：打开一次，之后所有访问都以句柄为边界并复核挂载身份。
pub struct RootHandle {
    dir: RestrictedDir,
    facts: StorageFacts,
    snapshot: RootSnapshot,
}

impl RootHandle {
    /// 探测并打开一个 Root。
    ///
    /// 步骤：`probe_path` 读内核事实 → 以 `O_DIRECTORY|O_NOFOLLOW` 打开规范路径 → 核对句柄身份
    /// 与探测事实是同一个目录 → 要求内核提供挂载 ID。
    ///
    /// # Errors
    ///
    /// 见 [`RootError`]：探测/打开失败、注册期目录被替换或内核缺少 `STATX_MNT_ID` 时都失败。
    pub fn open(path: &Path, mode: ResolveMode) -> Result<Self, RootError> {
        let facts = probe::probe_path(path)?;
        let dir = RestrictedDir::open(&facts.resolved_path, mode)?;
        let handle = dir.identity()?;
        let Some(handle_mount_id) = handle.mount_id else {
            return Err(RootError::MountIdUnavailable {
                path: facts.resolved_path,
            });
        };
        if handle.dev_major != facts.device.major
            || handle.dev_minor != facts.device.minor
            || handle.inode != facts.inode
            || handle.btime != facts.btime
            || handle_mount_id != facts.mount.mount_id
        {
            return Err(RootError::ChangedDuringOpen {
                path: facts.resolved_path,
                handle: handle.to_string(),
                probed: format!(
                    "dev={}:{} inode={} btime={:?} mount_id={}",
                    facts.device.major,
                    facts.device.minor,
                    facts.inode,
                    facts.btime,
                    facts.mount.mount_id
                ),
            });
        }
        let snapshot = RootSnapshot::of_facts(&facts);
        Ok(Self {
            dir,
            facts,
            snapshot,
        })
    }

    /// 规范路径（注册时探测到的，不重新解析）。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.snapshot.resolved_path
    }

    /// 探测到的完整内核事实。
    #[must_use]
    pub fn facts(&self) -> &StorageFacts {
        &self.facts
    }

    /// 注册时记录的身份快照。
    #[must_use]
    pub fn snapshot(&self) -> &RootSnapshot {
        &self.snapshot
    }

    /// 实际生效的解析机制。
    #[must_use]
    pub fn mechanism(&self) -> ResolveMechanism {
        self.dir.mechanism()
    }

    /// 读取根句柄自身的当前身份。
    ///
    /// # Errors
    ///
    /// `statx(2)` 失败时返回 [`RestrictError::Stat`]。
    pub fn identity(&self) -> Result<FileIdentity, RestrictError> {
        self.dir.identity()
    }

    /// 列举 Root 内相对目录的条目（空相对路径表示 Root 自身），并核对挂载边界。
    ///
    /// 权限失败返回错误而不是空列表（design §11.4：权限/扫描故障不等于删除）。
    ///
    /// # Errors
    ///
    /// 见 [`RestrictError`]：路径非法、逃逸、符号链接、跨挂载、权限不足或读目录失败。
    pub fn list_dir(&self, relative: &Path) -> Result<Vec<DirEntry>, RestrictError> {
        self.with_verified_dir(relative, RestrictedDir::list_entries)
    }

    /// 只读打开 Root 内的普通文件，并核对它仍在 Root 的挂载内。
    ///
    /// # Errors
    ///
    /// 见 [`RestrictError`]；不是普通文件时为 `NotRegularFile`，跨挂载为
    /// `MountBoundaryCrossed`。
    pub fn open_read(&self, relative: &Path) -> Result<OpenedObject, RestrictError> {
        let opened = self.dir.open_read(relative)?;
        self.check_mount(relative, opened.identity.mount_id)?;
        Ok(opened)
    }

    /// 在 Root 内新建独占文件，并核对新文件仍在 Root 的挂载内。
    ///
    /// 父目录先被打开并核对挂载，再用**那个句柄**建文件：不能先照路径建再回头发现跨了挂载，
    /// 否则 Bind mount 里已经被写进了外部样本。
    ///
    /// # Errors
    ///
    /// 见 [`RestrictError`]；目标已存在时为 `AlreadyExists`，跨挂载为 `MountBoundaryCrossed`。
    pub fn create_exclusive(&self, relative: &Path, mode: u32) -> Result<File, RestrictError> {
        restricted::validate_relative_path(relative).map_err(RestrictError::InvalidRelativePath)?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let name = relative.file_name().ok_or_else(|| {
            RestrictError::InvalidRelativePath(InvalidRelativePath {
                path: relative.to_path_buf(),
                detail: "empty",
            })
        })?;
        let file =
            self.with_verified_dir(parent, |dir| dir.create_exclusive(Path::new(name), mode))?;
        let identity =
            FileIdentity::of_fd(file.as_raw_fd()).map_err(|source| RestrictError::Stat {
                path: relative.to_path_buf(),
                source,
            })?;
        self.check_mount(relative, identity.mount_id)?;
        Ok(file)
    }

    /// 重新核对 Root 身份：句柄自身仍是注册时那个目录，且路径仍指向同一存储单元与挂载。
    ///
    /// # Errors
    ///
    /// 句柄身份漂移时返回 [`RootDrift`]；路径探测失败时返回 [`RootSpecVerifyError::Probe`]。
    pub fn verify(&self) -> Result<(), RootSpecVerifyError> {
        let handle = self.identity().map_err(RootSpecVerifyError::Handle)?;
        if !self.snapshot.same_directory(&handle) {
            return Err(RootSpecVerifyError::Drift(RootDrift::DirectoryChanged));
        }
        if handle.mount_id != Some(self.snapshot.mount_id) {
            return Err(RootSpecVerifyError::Drift(RootDrift::MountChanged));
        }
        self.snapshot.verify(&self.snapshot.resolved_path)
    }

    /// 打开（或复用根句柄）一个子目录，核对挂载边界后执行 `action`。
    fn with_verified_dir<T>(
        &self,
        relative: &Path,
        action: impl FnOnce(&RestrictedDir) -> Result<T, RestrictError>,
    ) -> Result<T, RestrictError> {
        if relative.as_os_str().is_empty() {
            return action(&self.dir);
        }
        let child = self.dir.child_dir(relative)?;
        let identity = child.identity()?;
        self.check_mount(relative, identity.mount_id)?;
        action(&child)
    }

    /// 对象的挂载 ID 必须是 Root 的挂载 ID；缺失时同样 fail-closed。
    fn check_mount(&self, relative: &Path, observed: Option<u64>) -> Result<(), RestrictError> {
        match observed {
            Some(observed) if observed == self.snapshot.mount_id => Ok(()),
            Some(observed) => Err(RestrictError::MountBoundaryCrossed {
                path: relative.to_path_buf(),
                root_mount_id: self.snapshot.mount_id,
                observed_mount_id: observed,
            }),
            None => Err(RestrictError::MountBoundaryUnverified {
                path: relative.to_path_buf(),
                detail: "内核未返回 STATX_MNT_ID",
            }),
        }
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

fn parse_fsid(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 {
        return None;
    }
    let mut fsid = [0u8; 16];
    for (index, chunk) in text.as_bytes().chunks_exact(2).enumerate() {
        let value = std::str::from_utf8(chunk).ok()?;
        fsid[index] = u8::from_str_radix(value, 16).ok()?;
    }
    Some(fsid)
}

fn parse_timestamp(text: &str) -> Option<(i64, u32)> {
    let (seconds, nanos) = text.split_once('.')?;
    let seconds = seconds.parse::<i64>().ok()?;
    let nanos = nanos.parse::<u32>().ok()?;
    if nanos >= 1_000_000_000 {
        return None;
    }
    Some((seconds, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::mountinfo::{DeviceId, MountEntry};

    fn facts(resolved: &str, inode: u64, mount_id: u64, subvolume_id: Option<u64>) -> StorageFacts {
        StorageFacts {
            given_path: PathBuf::from(resolved),
            resolved_path: PathBuf::from(resolved),
            fs_magic: admission::BTRFS_SUPER_MAGIC,
            fs_type: "btrfs".to_owned(),
            statfs_fsid: [0, 0],
            mount: MountEntry {
                mount_id,
                parent_id: 1,
                device: DeviceId {
                    major: 0,
                    minor: 40,
                },
                root: PathBuf::from("/"),
                mount_point: PathBuf::from("/data"),
                mount_options: vec!["rw".to_owned()],
                optional_fields: Vec::new(),
                fs_type: "btrfs".to_owned(),
                source: "/dev/sdc".to_owned(),
                super_options: vec!["rw".to_owned()],
            },
            mount_id_source: probe::MountIdSource::StatxMountId,
            inode,
            device: DeviceId {
                major: 0,
                minor: 41,
            },
            btime: Some((1_700_000_000, 1)),
            btrfs: Some(probe::BtrfsFacts {
                fsid: [0x11; 16],
                subvolume_id,
                subvolume_id_source: Some(probe::SubvolumeIdSource::StatxSubvol),
                subvolume_id_error: None,
            }),
            inode_flags: Some(0),
            inode_flags_error: None,
        }
    }

    #[test]
    fn snapshot_round_trips_through_spec() {
        let snapshot = RootSnapshot::of_facts(&facts("/data/root", 257, 45, Some(256)));
        let text = snapshot.to_spec();
        let parsed = RootSnapshot::parse_spec(&text).expect("自产规格必须可解析");
        assert_eq!(parsed, snapshot);
        assert!(text.contains("backend=btrfs"));
        assert!(text.contains("fsid=11111111111111111111111111111111"));
        assert!(text.contains("subvolume_id=256"));

        let non_btrfs = RootSnapshot {
            fsid: None,
            subvolume_id: None,
            btime: None,
            ..snapshot.clone()
        };
        let text = non_btrfs.to_spec();
        assert_eq!(
            RootSnapshot::parse_spec(&text).expect("non-btrfs 规格也要可解析"),
            non_btrfs
        );
    }

    #[test]
    fn snapshot_spec_rejects_missing_and_malformed_fields() {
        let text = RootSnapshot::of_facts(&facts("/data/root", 257, 45, Some(256))).to_spec();
        let without_mount = text
            .lines()
            .filter(|line| !line.starts_with("mount_id="))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            RootSnapshot::parse_spec(&without_mount).unwrap_err(),
            RootSpecError::MissingKey("mount_id")
        );
        let bad_inode = text.replace("inode=257", "inode=abc");
        assert!(matches!(
            RootSnapshot::parse_spec(&bad_inode).unwrap_err(),
            RootSpecError::InvalidValue { key, .. } if key == "inode"
        ));
        let bad_backend = text.replace("backend=btrfs", "backend=teleporter");
        assert!(matches!(
            RootSnapshot::parse_spec(&bad_backend).unwrap_err(),
            RootSpecError::InvalidValue { key, .. } if key == "backend"
        ));
        let duplicated = format!("{text}mount_id=46\n");
        assert_eq!(
            RootSnapshot::parse_spec(&duplicated).unwrap_err(),
            RootSpecError::DuplicateKey("mount_id")
        );
        let short_fsid = text.replace("fsid=11111111111111111111111111111111", "fsid=1111");
        assert!(matches!(
            RootSnapshot::parse_spec(&short_fsid).unwrap_err(),
            RootSpecError::InvalidValue { key, .. } if key == "fsid"
        ));
    }

    #[test]
    fn compare_reports_the_first_drift_in_design_order() {
        let recorded = RootSnapshot::of_facts(&facts("/data/root", 257, 45, Some(256)));

        assert_eq!(
            recorded.compare(&facts("/data/root", 257, 45, Some(256))),
            Ok(())
        );

        // 目录被换成同挂载上的另一个目录。
        assert_eq!(
            recorded.compare(&facts("/data/root", 300, 45, Some(256))),
            Err(RootDrift::DirectoryChanged)
        );

        // bind mount 覆盖路径：同一子卷、同一 inode，只有挂载 ID 变。
        assert_eq!(
            recorded.compare(&facts("/data/root", 257, 46, Some(256))),
            Err(RootDrift::MountChanged)
        );

        // 换成另一个子卷。
        assert_eq!(
            recorded.compare(&facts("/data/root", 256, 45, Some(257))),
            Err(RootDrift::SubvolumeChanged)
        );

        // btrfs 身份消失（换成 tmpfs 之类）。
        let mut other = facts("/data/root", 257, 45, Some(256));
        other.fs_magic = 0x0102_1994;
        other.fs_type = "tmpfs".to_owned();
        assert_eq!(recorded.compare(&other), Err(RootDrift::BackendChanged));

        assert_eq!(RootDrift::DirectoryChanged.code(), "directory_changed");
        assert_eq!(
            RootDrift::MountChanged.design_error_code(),
            "ROOT_UNAVAILABLE"
        );
    }

    #[test]
    fn snapshot_identity_comparison_uses_device_inode_and_btime() {
        let snapshot = RootSnapshot::of_facts(&facts("/data/root", 257, 45, Some(256)));
        let identity = FileIdentity {
            dev_major: snapshot.dev_major,
            dev_minor: snapshot.dev_minor,
            inode: snapshot.inode,
            btime: snapshot.btime,
            size: 4096,
            mtime: (1, 0),
            ctime: (1, 0),
            nlink: 1,
            mode: 0o040_755,
            mount_id: Some(snapshot.mount_id),
        };
        assert!(snapshot.same_directory(&identity));
        let mut replaced = identity.clone();
        replaced.inode += 1;
        assert!(!snapshot.same_directory(&replaced));
        let mut remounted = identity.clone();
        remounted.btime = None;
        assert!(!snapshot.same_directory(&remounted));
    }
}
