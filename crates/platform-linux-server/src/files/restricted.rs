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
//!   不提供“改当前工作目录再按绝对路径访问”的入口。

use crate::files::identity::FileIdentity;
use crate::files::raw::{self, errno};
use crate::mounts::raw::{O_DIRECTORY, O_NOFOLLOW, O_PATH};
use std::ffi::{CString, OsStr};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// 单个名称分量的字节上限（`NAME_MAX`）。
const NAME_MAX_BYTES: usize = 255;
/// 相对路径整体字节上限（`PATH_MAX` 留一字节给 NUL）。
const PATH_MAX_BYTES: usize = 4096;

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
            | RestrictError::Stat { source, .. } => Some(source),
            RestrictError::NotDirectory { .. } | RestrictError::NotRegularFile { .. } => None,
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
            .map_err(|source| RestrictError::OpenRoot {
                path: path.to_path_buf(),
                source,
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
}
