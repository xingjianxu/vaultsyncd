//! 受限目录句柄：所有文件操作都在一个已打开的目录句柄之下按相对名称进行。
//!
//! design §16.1 要求“运行时依然以根句柄为边界逐层安全访问”，且注册期的 `canonicalize` 不能消除
//! 之后的 TOCTOU。因此这里的每个操作都：
//!
//! - 只接受**相对**名称，拒绝空段、`.`、`..`、绝对路径、NUL 与超长段（纯逻辑，先于系统调用）；
//! - 优先用 `openat2(2)` + `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS`：逃出根目录
//!   返回 `EXDEV`、遇到符号链接返回 `ELOOP`，两种情况都 fail-closed；
//! - 内核早于 5.6（`openat2` 返回 `ENOSYS`）时回退到逐段 `openat(2)` + `O_NOFOLLOW`，并在结果里
//!   标明实际使用的机制。其它错误（例如被 seccomp 拦截的 `EPERM`）一律原样上报，不用回退掩盖；
//! - 提供 `stat`（`O_PATH` 句柄 + `statx`）、创建独占新文件、`renameat2`、`unlinkat`、`fsync`，
//!   不提供“改当前工作目录再按绝对路径访问”的入口；
//! - 目录列举也走已打开目录句柄的 `getdents64(2)`，不用 `read_dir(路径)`：路径在列举瞬间被换掉
//!   也带不到别处，且权限失败必须报错而不是返回空目录。

use crate::files::identity::FileIdentity;
use crate::files::raw::{self, errno};
use crate::mounts::raw::{O_DIRECTORY, O_NOFOLLOW, O_PATH};
use std::ffi::{CString, OsStr, OsString};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// 单个名称分量的字节上限（`NAME_MAX`）。
const NAME_MAX_BYTES: usize = 255;
/// 相对路径整体字节上限（`PATH_MAX` 留一字节给 NUL）。
const PATH_MAX_BYTES: usize = 4096;
/// `getdents64(2)` 每次读取的缓冲区（内核不会拆开单条记录）。
const DIRENT_BUFFER_BYTES: usize = 32 * 1024;

/// 使用哪种解析机制。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveMode {
    /// 优先 `openat2`，仅当内核没有该系统调用时回退逐段打开。
    Auto,
    /// 强制 `openat2`；不可用时失败，不回退（用于证明内核能力）。
    Openat2,
    /// 强制逐段 `openat(2)` + `O_NOFOLLOW`。
    ComponentWalk,
}

impl ResolveMode {
    /// 从命令行短名解析。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(ResolveMode::Auto),
            "openat2" => Some(ResolveMode::Openat2),
            "walk" => Some(ResolveMode::ComponentWalk),
            _ => None,
        }
    }
}

/// 实际生效的解析机制（写进输出，便于区分“真的走了 openat2”与“回退了”）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveMechanism {
    /// `openat2(2)` + `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS`。
    Openat2,
    /// 逐段 `openat(2)` + `O_NOFOLLOW`。
    ComponentWalk,
}

impl ResolveMechanism {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ResolveMechanism::Openat2 => "openat2",
            ResolveMechanism::ComponentWalk => "component_walk",
        }
    }
}

impl fmt::Display for ResolveMechanism {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 相对名称校验失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidRelativePath {
    /// 原始路径。
    pub path: PathBuf,
    /// 稳定原因：`empty`/`absolute`/`empty_component`/`dot_component`/`dotdot_component`/
    /// `nul_byte`/`component_too_long`/`path_too_long`。
    pub detail: &'static str,
}

impl fmt::Display for InvalidRelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "非法相对路径 {}（{}）", self.path.display(), self.detail)
    }
}

/// 校验一个相对路径：只允许普通的相对分量，不允许任何逃逸形式（design §16.1）。
///
/// # Errors
///
/// 见 [`InvalidRelativePath::detail`] 的稳定原因码。
pub fn validate_relative_path(path: &Path) -> Result<(), InvalidRelativePath> {
    let bytes = path.as_os_str().as_bytes();
    let fail = |detail: &'static str| {
        Err(InvalidRelativePath {
            path: path.to_path_buf(),
            detail,
        })
    };
    if bytes.is_empty() {
        return fail("empty");
    }
    if bytes[0] == b'/' {
        return fail("absolute");
    }
    if bytes.len() >= PATH_MAX_BYTES {
        return fail("path_too_long");
    }
    if bytes.contains(&0) {
        return fail("nul_byte");
    }
    for component in bytes.split(|byte| *byte == b'/') {
        if component.is_empty() {
            return fail("empty_component");
        }
        if component == b"." {
            return fail("dot_component");
        }
        if component == b".." {
            return fail("dotdot_component");
        }
        if component.len() > NAME_MAX_BYTES {
            return fail("component_too_long");
        }
    }
    Ok(())
}

fn to_c_string(path: &Path) -> Result<CString, InvalidRelativePath> {
    validate_relative_path(path)?;
    CString::new(path.as_os_str().as_bytes()).map_err(|_| InvalidRelativePath {
        path: path.to_path_buf(),
        detail: "nul_byte",
    })
}

/// 目录项的类型（来自 `getdents64` 的 `d_type`，不需要额外 `statx`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// 普通文件。
    File,
    /// 目录。
    Directory,
    /// 符号链接（按契约不跟随，只上报）。
    Symlink,
    /// 其它类型（设备、FIFO、套接字等）。
    Other,
    /// 文件系统未提供类型（`DT_UNKNOWN`）：调用方需自行 `statx` 才能分类。
    Unknown,
}

impl EntryKind {
    fn from_dtype(kind: u8) -> Self {
        match kind {
            raw::dtype::REG => EntryKind::File,
            raw::dtype::DIR => EntryKind::Directory,
            raw::dtype::LNK => EntryKind::Symlink,
            raw::dtype::UNKNOWN => EntryKind::Unknown,
            _ => EntryKind::Other,
        }
    }

    /// 稳定短名（给输出与测试断言用）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::File => "file",
            EntryKind::Directory => "dir",
            EntryKind::Symlink => "symlink",
            EntryKind::Other => "other",
            EntryKind::Unknown => "unknown",
        }
    }
}

/// 一个目录项：名称、类型、inode（`d_ino`）。
///
/// inode 只用于报告与快速比对，不能单独当身份（跨子卷会重复，也可能被复用）；身份判定
/// 一律用 [`FileIdentity::same_object`]。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// 入口名称（不含 `.` 与 `..`）。
    pub name: OsString,
    /// 类型。
    pub kind: EntryKind,
    /// `d_ino`
    pub inode: u64,
}

/// 文件操作的错误：全部保留路径与原始 `io::Error`，不吞错。
#[derive(Debug)]
pub enum RestrictError {
    /// 相对路径不符合 §16.1 的规则。
    InvalidRelativePath(InvalidRelativePath),
    /// 打开根目录句柄失败。
    OpenRoot {
        /// 调用方给出的路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 路径不是目录（根句柄或子目录句柄）。
    NotDirectory {
        /// 路径。
        path: PathBuf,
        /// `statx` 观察到的身份。
        observed: String,
    },
    /// 解析相对路径失败（`openat2` 或逐段打开）。
    Resolve {
        /// 相对路径。
        path: PathBuf,
        /// 实际使用的机制。
        mechanism: ResolveMechanism,
        /// 原始错误。
        source: io::Error,
    },
    /// 路径解析结果逃出了根目录（`RESOLVE_BENEATH` 的 `EXDEV`）。
    EscapesRoot {
        /// 相对路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 路径中含有符号链接，按契约拒绝跟随。
    SymlinkRejected {
        /// 相对路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 路径不存在。
    NotFound {
        /// 相对路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 操作对象不是普通文件。
    NotRegularFile {
        /// 相对路径。
        path: PathBuf,
        /// `statx` 观察到的身份。
        observed: String,
    },
    /// 目标已存在（`O_EXCL` 或 `RENAME_NOREPLACE`）。
    AlreadyExists {
        /// 相对路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 跨文件系统重命名（`EXDEV`）：不能靠一次 `rename` 完成。
    CrossDeviceRename {
        /// 源相对路径。
        from: PathBuf,
        /// 目标相对路径。
        to: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 重命名失败。
    Rename {
        /// 源相对路径。
        from: PathBuf,
        /// 目标相对路径。
        to: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 删除失败。
    Unlink {
        /// 相对路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// `fsync` 失败。
    Fsync {
        /// 被刷盘的目录。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 读取身份（`statx`）失败。
    Stat {
        /// 相对路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 权限不足（`EACCES`/`EPERM`）：不是“空目录”，也不当不存在。
    PermissionDenied {
        /// 路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 读目录项失败（`getdents64(2)`）。
    ReadDir {
        /// 被列举的目录。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 解析结果进入了另一个挂载（bind mount、子挂载）：超出 Root 的存储边界。
    MountBoundaryCrossed {
        /// 相对路径。
        path: PathBuf,
        /// Root 句柄所在的挂载 ID。
        root_mount_id: u64,
        /// 实际观察到的挂载 ID。
        observed_mount_id: u64,
    },
    /// 内核未提供挂载 ID，无法确认这是否仍在 Root 的挂载内。
    MountBoundaryUnverified {
        /// 相对路径。
        path: PathBuf,
        /// 原因。
        detail: &'static str,
    },
}

impl RestrictError {
    /// 稳定原因码（测试与输出按它断言）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            RestrictError::InvalidRelativePath(_) => "invalid_relative_path",
            RestrictError::OpenRoot { .. } => "open_root_failed",
            RestrictError::NotDirectory { .. } => "not_directory",
            RestrictError::Resolve { .. } => "resolve_failed",
            RestrictError::EscapesRoot { .. } => "escape_rejected",
            RestrictError::SymlinkRejected { .. } => "symlink_rejected",
            RestrictError::NotFound { .. } => "not_found",
            RestrictError::NotRegularFile { .. } => "not_regular_file",
            RestrictError::AlreadyExists { .. } => "already_exists",
            RestrictError::CrossDeviceRename { .. } => "cross_device_rename",
            RestrictError::Rename { .. } => "rename_failed",
            RestrictError::Unlink { .. } => "unlink_failed",
            RestrictError::Fsync { .. } => "fsync_failed",
            RestrictError::Stat { .. } => "stat_failed",
            RestrictError::PermissionDenied { .. } => "permission_denied",
            RestrictError::ReadDir { .. } => "read_dir_failed",
            RestrictError::MountBoundaryCrossed { .. } => "mount_boundary_crossed",
            RestrictError::MountBoundaryUnverified { .. } => "mount_boundary_unverified",
        }
    }
}

impl fmt::Display for RestrictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestrictError::InvalidRelativePath(error) => write!(f, "{error}"),
            RestrictError::OpenRoot { path, source } => {
                write!(f, "无法打开根目录句柄 {}：{source}", path.display())
            }
            RestrictError::NotDirectory { path, observed } => {
                write!(f, "{} 不是目录（{observed}）", path.display())
            }
            RestrictError::Resolve {
                path,
                mechanism,
                source,
            } => write!(f, "按 {mechanism} 解析 {} 失败：{source}", path.display()),
            RestrictError::EscapesRoot { path, source } => write!(
                f,
                "{} 的解析结果逃出了根目录，拒绝继续：{source}",
                path.display()
            ),
            RestrictError::SymlinkRejected { path, source } => {
                write!(f, "{} 含符号链接，按契约不跟随：{source}", path.display())
            }
            RestrictError::NotFound { path, source } => {
                write!(f, "{} 不存在：{source}", path.display())
            }
            RestrictError::NotRegularFile { path, observed } => {
                write!(f, "{} 不是普通文件（{observed}）", path.display())
            }
            RestrictError::AlreadyExists { path, source } => {
                write!(f, "{} 已存在，拒绝覆盖：{source}", path.display())
            }
            RestrictError::CrossDeviceRename { from, to, source } => write!(
                f,
                "{} → {} 跨文件系统，无法一次重命名完成：{source}",
                from.display(),
                to.display()
            ),
            RestrictError::Rename { from, to, source } => {
                write!(
                    f,
                    "{} → {} 重命名失败：{source}",
                    from.display(),
                    to.display()
                )
            }
            RestrictError::Unlink { path, source } => {
                write!(f, "删除 {} 失败：{source}", path.display())
            }
            RestrictError::Fsync { path, source } => {
                write!(f, "刷盘 {} 失败：{source}", path.display())
            }
            RestrictError::Stat { path, source } => {
                write!(f, "读取 {} 的身份失败：{source}", path.display())
            }
            RestrictError::PermissionDenied { path, source } => {
                write!(f, "没有权限访问 {}：{source}", path.display())
            }
            RestrictError::ReadDir { path, source } => {
                write!(f, "读取目录项 {} 失败：{source}", path.display())
            }
            RestrictError::MountBoundaryCrossed {
                path,
                root_mount_id,
                observed_mount_id,
            } => write!(
                f,
                "{} 进入了另一个挂载（Root 挂载 {root_mount_id}，实际 {observed_mount_id}），拒绝继续",
                path.display()
            ),
            RestrictError::MountBoundaryUnverified { path, detail } => write!(
                f,
                "无法确认 {} 是否仍在 Root 的挂载内（{detail}），拒绝继续",
                path.display()
            ),
        }
    }
}

impl std::error::Error for RestrictError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RestrictError::InvalidRelativePath(_) => None,
            RestrictError::OpenRoot { source, .. }
            | RestrictError::Resolve { source, .. }
            | RestrictError::EscapesRoot { source, .. }
            | RestrictError::SymlinkRejected { source, .. }
            | RestrictError::NotFound { source, .. }
            | RestrictError::AlreadyExists { source, .. }
            | RestrictError::CrossDeviceRename { source, .. }
            | RestrictError::Rename { source, .. }
            | RestrictError::Unlink { source, .. }
            | RestrictError::Fsync { source, .. }
            | RestrictError::Stat { source, .. }
            | RestrictError::PermissionDenied { source, .. }
            | RestrictError::ReadDir { source, .. } => Some(source),
            RestrictError::NotDirectory { .. }
            | RestrictError::NotRegularFile { .. }
            | RestrictError::MountBoundaryCrossed { .. }
            | RestrictError::MountBoundaryUnverified { .. } => None,
        }
    }
}

/// 打开的对象：句柄 + 打开时观察到的身份。
pub struct OpenedObject {
    /// 已打开的句柄（只读或 `O_PATH`）。
    pub file: File,
    /// 打开后立刻用 `statx` 读到的身份。
    pub identity: FileIdentity,
}

impl OpenedObject {
    fn new(fd: OwnedFd, path: &Path) -> Result<Self, RestrictError> {
        let identity =
            FileIdentity::of_fd(fd.as_raw_fd()).map_err(|source| RestrictError::Stat {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(Self {
            file: File::from(fd),
            identity,
        })
    }
}

/// 一个已打开的目录句柄；所有相对操作都以它为边界。
pub struct RestrictedDir {
    path: PathBuf,
    handle: OwnedFd,
    mechanism: ResolveMechanism,
}

impl RestrictedDir {
    /// 打开一个目录作为边界句柄。
    ///
    /// 根路径本身不跟随符号链接（`O_NOFOLLOW|O_DIRECTORY`）：注册期的规范化由调用方完成，
    /// 运行期不再接受会变形的根路径。
    ///
    /// # Errors
    ///
    /// 见 [`RestrictError`]：路径不可打开、不是目录，或强制 `openat2` 而内核不支持时返回错误。
    pub fn open(path: &Path, mode: ResolveMode) -> Result<Self, RestrictError> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW | raw::O_CLOEXEC)
            .open(path)
            .map_err(|source| match source.raw_os_error() {
                Some(errno::EACCES | errno::EPERM) => RestrictError::PermissionDenied {
                    path: path.to_path_buf(),
                    source,
                },
                _ => RestrictError::OpenRoot {
                    path: path.to_path_buf(),
                    source,
                },
            })?;
        let identity =
            FileIdentity::of_fd(file.as_raw_fd()).map_err(|source| RestrictError::Stat {
                path: path.to_path_buf(),
                source,
            })?;
        if !identity.is_directory() {
            return Err(RestrictError::NotDirectory {
                path: path.to_path_buf(),
                observed: identity.to_string(),
            });
        }
        let handle = OwnedFd::from(file);
        let mechanism = match mode {
            ResolveMode::Openat2 => ResolveMechanism::Openat2,
            ResolveMode::ComponentWalk => ResolveMechanism::ComponentWalk,
            ResolveMode::Auto => {
                let probe = raw::openat2(handle.as_raw_fd(), c".", O_PATH, 0, raw::RESOLVE_STRICT);
                match probe {
                    Ok(_) => ResolveMechanism::Openat2,
                    Err(error) if raw::is_openat2_missing(&error) => {
                        ResolveMechanism::ComponentWalk
                    }
                    Err(source) => {
                        return Err(RestrictError::Resolve {
                            path: path.to_path_buf(),
                            mechanism: ResolveMechanism::Openat2,
                            source,
                        });
                    }
                }
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            handle,
            mechanism,
        })
    }

    /// 打开时给出的路径（只用于报告，不参与解析）。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 实际生效的解析机制。
    #[must_use]
    pub fn mechanism(&self) -> ResolveMechanism {
        self.mechanism
    }

    /// 目录自身的身份（含挂载 ID）。
    ///
    /// # Errors
    ///
    /// `statx` 失败时返回 [`RestrictError::Stat`]。
    pub fn identity(&self) -> Result<FileIdentity, RestrictError> {
        FileIdentity::of_fd(self.handle.as_raw_fd()).map_err(|source| RestrictError::Stat {
            path: self.path.clone(),
            source,
        })
    }

    /// 目录句柄所在的挂载 ID（用于确认暂存与目标在同一文件系统/挂载内）。
    ///
    /// # Errors
    ///
    /// `statx` 失败时返回 [`RestrictError::Stat`]。
    pub fn mount_id(&self) -> Result<Option<u64>, RestrictError> {
        Ok(self.identity()?.mount_id)
    }

    /// 打开根目录之下的子目录，得到一个同样受边界约束的句柄。
    ///
    /// # Errors
    ///
    /// 见 [`RestrictError`]：路径非法、逃逸、含符号链接、不存在或不是目录。
    pub fn child_dir(&self, relative: &Path) -> Result<Self, RestrictError> {
        let opened = self.open_with(relative, O_DIRECTORY, 0)?;
        if !opened.identity.is_directory() {
            return Err(RestrictError::NotDirectory {
                path: self.path.join(relative),
                observed: opened.identity.to_string(),
            });
        }
        Ok(Self {
            path: self.path.join(relative),
            handle: OwnedFd::from(opened.file),
            mechanism: self.mechanism,
        })
    }

    /// 用 `O_PATH` 打开（只读元数据，不需要读权限）。
    ///
    /// # Errors
    ///
    /// 见 [`RestrictError`]。
    pub fn open_path(&self, relative: &Path) -> Result<OpenedObject, RestrictError> {
        self.open_with(relative, O_PATH, 0)
    }

    /// 只读打开并确认是普通文件。
    ///
    /// # Errors
    ///
    /// 见 [`RestrictError`]；不是普通文件时返回 [`RestrictError::NotRegularFile`]。
    pub fn open_read(&self, relative: &Path) -> Result<OpenedObject, RestrictError> {
        let opened = self.open_with(relative, 0, 0)?;
        if !opened.identity.is_regular_file() {
            return Err(RestrictError::NotRegularFile {
                path: relative.to_path_buf(),
                observed: opened.identity.to_string(),
            });
        }
        Ok(opened)
    }

    /// 读取相对路径的当前身份；不存在时返回 `Ok(None)`（其它错误原样上报）。
    ///
    /// # Errors
    ///
    /// 见 [`RestrictError`]：合法的不存在不算错误，逃逸/符号链接/无权限都算。
    pub fn identity_at(&self, relative: &Path) -> Result<Option<FileIdentity>, RestrictError> {
        match self.open_path(relative) {
            Ok(opened) => Ok(Some(opened.identity)),
            Err(RestrictError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// 新建一个独占的普通文件（`O_CREAT|O_EXCL`，已存在即失败）。
    ///
    /// # Errors
    ///
    /// 见 [`RestrictError`]；目标已存在时返回 [`RestrictError::AlreadyExists`]。
    pub fn create_exclusive(&self, relative: &Path, mode: u32) -> Result<File, RestrictError> {
        let opened = self.open_with(relative, raw::O_RDWR | raw::O_CREAT | raw::O_EXCL, mode)?;
        Ok(opened.file)
    }

    /// 刷盘目录自身（发布重命名之后必须做）。
    ///
    /// # Errors
    ///
    /// `fsync(2)` 失败时返回 [`RestrictError::Fsync`]。
    pub fn fsync_dir(&self) -> Result<(), RestrictError> {
        raw::fsync(self.handle.as_raw_fd()).map_err(|source| RestrictError::Fsync {
            path: self.path.clone(),
            source,
        })
    }

    /// 列举本目录句柄的目录项（`getdents64(2)`），按名称字节序排序。
    ///
    /// 列举的是**这个句柄**指向的目录，不重新解析任何路径；`.b` 与 `..` 丢弃，符号链接只
    /// 报告为 [`EntryKind::Symlink`]，不跟随。无权限、句柄失效与内核报错都返回错误——
    /// **永远不会用空列表代替失败**（design §11.4：权限/扫描故障不等于删除）。
    ///
    /// 条目数不设上限：需要上限的递归扫描由上层根据自己的策略限制。
    ///
    /// # Errors
    ///
    /// 见 [`RestrictError::ReadDir`]、[`RestrictError::PermissionDenied`]。
    pub fn list_entries(&self) -> Result<Vec<DirEntry>, RestrictError> {
        // 同一个句柄可能被重复列举（Root 句柄每轮扫描都列一次）：先倒回起点，否则第二次起得到空列表。
        raw::rewind_dir(self.handle.as_raw_fd()).map_err(|source| RestrictError::ReadDir {
            path: self.path.clone(),
            source,
        })?;
        let mut buffer = vec![0u8; DIRENT_BUFFER_BYTES];
        let mut entries = Vec::new();
        loop {
            let filled = raw::getdents64(self.handle.as_raw_fd(), &mut buffer).map_err(
                |source| match source.raw_os_error() {
                    Some(errno::EACCES | errno::EPERM) => RestrictError::PermissionDenied {
                        path: self.path.clone(),
                        source,
                    },
                    _ => RestrictError::ReadDir {
                        path: self.path.clone(),
                        source,
                    },
                },
            )?;
            if filled == 0 {
                break;
            }
            let parsed =
                raw::parse_dirents(&buffer, filled).map_err(|error| RestrictError::ReadDir {
                    path: self.path.clone(),
                    source: io::Error::new(io::ErrorKind::InvalidData, error),
                })?;
            for entry in parsed {
                if entry.name == b"." || entry.name == b".." {
                    continue;
                }
                entries.push(DirEntry {
                    name: OsString::from_vec(entry.name),
                    kind: EntryKind::from_dtype(entry.kind),
                    inode: entry.inode,
                });
            }
        }
        entries.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
        Ok(entries)
    }

    /// 在目录句柄之间重命名。`noreplace` 为真时目标必须不存在（`RENAME_NOREPLACE`）。
    ///
    /// # Errors
    ///
    /// 见 [`RestrictError`]；跨文件系统返回 [`RestrictError::CrossDeviceRename`]，目标已存在返回
    /// [`RestrictError::AlreadyExists`]。
    pub fn rename(
        &self,
        from: &Path,
        to_dir: &Self,
        to: &Path,
        noreplace: bool,
    ) -> Result<(), RestrictError> {
        let from_c = to_c_string(from).map_err(RestrictError::InvalidRelativePath)?;
        let to_c = to_c_string(to).map_err(RestrictError::InvalidRelativePath)?;
        let flags = if noreplace { raw::RENAME_NOREPLACE } else { 0 };
        raw::renameat2(
            self.handle.as_raw_fd(),
            &from_c,
            to_dir.handle.as_raw_fd(),
            &to_c,
            flags,
        )
        .map_err(|source| match source.raw_os_error() {
            Some(errno::EXDEV) => RestrictError::CrossDeviceRename {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                source,
            },
            Some(errno::EEXIST) => RestrictError::AlreadyExists {
                path: to.to_path_buf(),
                source,
            },
            _ => RestrictError::Rename {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                source,
            },
        })
    }

    /// 删除目录句柄下的文件。
    ///
    /// # Errors
    ///
    /// 见 [`RestrictError::Unlink`]。
    pub fn unlink(&self, relative: &Path) -> Result<(), RestrictError> {
        let target = to_c_string(relative).map_err(RestrictError::InvalidRelativePath)?;
        raw::unlinkat(self.handle.as_raw_fd(), &target).map_err(|source| RestrictError::Unlink {
            path: relative.to_path_buf(),
            source,
        })
    }

    fn open_with(
        &self,
        relative: &Path,
        flags: i32,
        mode: u32,
    ) -> Result<OpenedObject, RestrictError> {
        let path = to_c_string(relative).map_err(RestrictError::InvalidRelativePath)?;
        let fd = match self.mechanism {
            ResolveMechanism::Openat2 => raw::openat2(
                self.handle.as_raw_fd(),
                &path,
                flags,
                mode,
                raw::RESOLVE_STRICT,
            )
            .map_err(|source| self.map_resolve_error(relative, source))?,
            ResolveMechanism::ComponentWalk => self.walk_open(relative, flags, mode)?,
        };
        OpenedObject::new(fd, relative)
    }

    /// 逐段 `openat(2)` + `O_NOFOLLOW`：中间段必须是目录，末段用调用方给的标志。
    fn walk_open(&self, relative: &Path, flags: i32, mode: u32) -> Result<OwnedFd, RestrictError> {
        let components: Vec<&OsStr> = relative.iter().collect();
        let mut current: Option<OwnedFd> = None;
        for (index, component) in components.iter().enumerate() {
            let is_last = index + 1 == components.len();
            let name = CString::new(component.as_bytes()).map_err(|_| {
                RestrictError::InvalidRelativePath(InvalidRelativePath {
                    path: relative.to_path_buf(),
                    detail: "nul_byte",
                })
            })?;
            let dirfd = current
                .as_ref()
                .map_or_else(|| self.handle.as_raw_fd(), AsRawFd::as_raw_fd);
            let step_flags = if is_last {
                flags | O_NOFOLLOW
            } else {
                O_PATH | O_NOFOLLOW | O_DIRECTORY
            };
            let step_mode = if is_last { mode } else { 0 };
            let fd = raw::openat(dirfd, &name, step_flags, step_mode)
                .map_err(|source| self.map_resolve_error(relative, source))?;
            if is_last {
                return Ok(fd);
            }
            current = Some(fd);
        }
        // 校验已拒绝空路径，这里只是把“不可能”写成显式错误。
        Err(RestrictError::InvalidRelativePath(InvalidRelativePath {
            path: relative.to_path_buf(),
            detail: "empty",
        }))
    }

    fn map_resolve_error(&self, relative: &Path, source: io::Error) -> RestrictError {
        match source.raw_os_error() {
            Some(errno::EXDEV) => RestrictError::EscapesRoot {
                path: relative.to_path_buf(),
                source,
            },
            Some(errno::ELOOP) => RestrictError::SymlinkRejected {
                path: relative.to_path_buf(),
                source,
            },
            Some(errno::ENOENT) => RestrictError::NotFound {
                path: relative.to_path_buf(),
                source,
            },
            Some(errno::EEXIST) => RestrictError::AlreadyExists {
                path: relative.to_path_buf(),
                source,
            },
            Some(errno::EACCES | errno::EPERM) => RestrictError::PermissionDenied {
                path: relative.to_path_buf(),
                source,
            },
            _ => RestrictError::Resolve {
                path: relative.to_path_buf(),
                mechanism: self.mechanism,
                source,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_relative_names() {
        for path in [
            "a",
            "a.txt",
            "dir/child",
            ".hidden",
            "dir/.hidden",
            "a-b_c.d",
        ] {
            assert!(
                validate_relative_path(Path::new(path)).is_ok(),
                "应接受 {path}"
            );
        }
    }

    #[test]
    fn rejects_escape_and_malformed_forms() {
        let cases = [
            ("", "empty"),
            ("/etc/passwd", "absolute"),
            ("/", "absolute"),
            (".", "dot_component"),
            ("..", "dotdot_component"),
            ("dir/..", "dotdot_component"),
            ("dir/../etc", "dotdot_component"),
            ("dir//child", "empty_component"),
            ("dir/", "empty_component"),
        ];
        for (path, detail) in cases {
            let error =
                validate_relative_path(Path::new(path)).expect_err(&format!("应拒绝 {path:?}"));
            assert_eq!(error.detail, detail, "path={path:?}");
        }

        let long_name = "a".repeat(NAME_MAX_BYTES + 1);
        assert_eq!(
            validate_relative_path(Path::new(&long_name))
                .unwrap_err()
                .detail,
            "component_too_long"
        );
        let long_path = format!("{}/x", "a".repeat(PATH_MAX_BYTES));
        assert_eq!(
            validate_relative_path(Path::new(&long_path))
                .unwrap_err()
                .detail,
            "path_too_long"
        );
    }

    #[test]
    fn resolve_mode_parsing_is_explicit() {
        assert_eq!(ResolveMode::parse("openat2"), Some(ResolveMode::Openat2));
        assert_eq!(ResolveMode::parse("walk"), Some(ResolveMode::ComponentWalk));
        assert_eq!(ResolveMode::parse("auto"), Some(ResolveMode::Auto));
        assert_eq!(ResolveMode::parse("force"), None);
        assert_eq!(ResolveMechanism::Openat2.as_str(), "openat2");
        assert_eq!(ResolveMechanism::ComponentWalk.as_str(), "component_walk");
    }

    #[test]
    fn error_codes_are_stable() {
        let invalid = RestrictError::InvalidRelativePath(InvalidRelativePath {
            path: PathBuf::from(".."),
            detail: "dotdot_component",
        });
        assert_eq!(invalid.code(), "invalid_relative_path");
        assert!(invalid.to_string().contains("dotdot_component"));
    }

    #[test]
    fn permission_errors_keep_their_own_code() {
        let denied = RestrictError::PermissionDenied {
            path: PathBuf::from("locked"),
            source: io::Error::from_raw_os_error(errno::EACCES),
        };
        assert_eq!(denied.code(), "permission_denied");
        assert!(
            std::error::Error::source(&denied)
                .and_then(|source| source.downcast_ref::<io::Error>())
                .is_some_and(|source| source.raw_os_error() == Some(errno::EACCES)),
            "权限错误必须保留原始 io::Error"
        );
        assert_eq!(
            RestrictError::ReadDir {
                path: PathBuf::from("locked"),
                source: io::Error::from_raw_os_error(errno::EACCES),
            }
            .code(),
            "read_dir_failed"
        );
        assert_eq!(
            RestrictError::MountBoundaryCrossed {
                path: PathBuf::from("alias"),
                root_mount_id: 40,
                observed_mount_id: 41,
            }
            .code(),
            "mount_boundary_crossed"
        );
        assert_eq!(
            RestrictError::MountBoundaryUnverified {
                path: PathBuf::from("x"),
                detail: "STATX_MNT_ID 不可用",
            }
            .code(),
            "mount_boundary_unverified"
        );
    }

    #[test]
    fn entry_kinds_map_from_dtype() {
        assert_eq!(EntryKind::File.as_str(), "file");
        assert_eq!(EntryKind::Directory.as_str(), "dir");
        assert_eq!(EntryKind::Symlink.as_str(), "symlink");
        assert_eq!(EntryKind::Other.as_str(), "other");
        assert_eq!(EntryKind::Unknown.as_str(), "unknown");
        assert_eq!(EntryKind::from_dtype(raw::dtype::REG), EntryKind::File);
        assert_eq!(EntryKind::from_dtype(raw::dtype::DIR), EntryKind::Directory);
        assert_eq!(EntryKind::from_dtype(raw::dtype::LNK), EntryKind::Symlink);
        assert_eq!(
            EntryKind::from_dtype(raw::dtype::UNKNOWN),
            EntryKind::Unknown
        );
        assert_eq!(EntryKind::from_dtype(6), EntryKind::Other);
    }

    #[test]
    fn listing_the_same_handle_twice_returns_the_same_entries() {
        // 回归（S049）：目录句柄的读取位置属于打开文件描述，不倒回会让第二次列举得到空列表。
        let base =
            std::env::temp_dir().join(format!("vaultsyncd-s049-relist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("建目录");
        std::fs::write(base.join("a.txt"), b"a").expect("写文件");
        std::fs::write(base.join("b.txt"), b"b").expect("写文件");
        let dir = RestrictedDir::open(&base, ResolveMode::Auto).expect("打开目录句柄");
        let first = dir.list_entries().expect("第一次列举");
        let second = dir.list_entries().expect("第二次列举");
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(first.len(), 2);
        assert_eq!(
            first, second,
            "重复列举同一句柄必须得到相同结果，而不是空列表"
        );
    }
}
