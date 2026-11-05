//! 目录句柄探针：把内核事实读成 [`StorageFacts`]。
//!
//! 检查顺序（每一步都用打开句柄上的内核事实，不用路径字符串推断后端）：
//!
//! 1. `canonicalize` 消除符号链接、`..` 与重复分隔符（规范化只用于打开路径；运行时仍以根
//!    句柄为边界，design §16.1）。
//! 2. 用 `O_PATH|O_DIRECTORY|O_NOFOLLOW` 打开目录句柄 → `fstatfs(2)` 取 `f_type`（后端类型
//!    的权威来源）→ `statx(2)` 取 inode、设备号与 `STATX_MNT_ID`。
//! 3. 用 `O_RDONLY|O_DIRECTORY|O_NOFOLLOW` 打开第二个句柄做 `ioctl`（`O_PATH` 句柄上 ioctl
//!    返回 `EBADF`），并核对两个句柄是同一个 inode，避免路径在两次打开之间被替换。
//! 4. 按挂载 ID 在本进程 `/proc/self/mountinfo` 里取该挂载的 fstype、选项、根与设备；内核不
//!    提供 `STATX_MNT_ID` 时退化为“同设备 + 最长挂载点前缀”，并在结果里标明依据。
//! 5. btrfs 上再取 FSID（`BTRFS_IOC_FS_INFO`）与子卷 ID（`BTRFS_IOC_INO_LOOKUP`），并对任何
//!    文件系统读 `FS_IOC_GETFLAGS`（只读）。
//!
//! 全部是只读查询：本模块没有任何设置标志、清理 NOCOW 或改变挂载的调用。

use crate::mounts::admission::BTRFS_SUPER_MAGIC;
use crate::mounts::mountinfo::{self, DeviceId, MountEntry};
use crate::mounts::paths;
use crate::mounts::raw;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// 子卷根目录的固定 inode 号（`BTRFS_FIRST_FREE_OBJECTID` 之前的保留值）。
///
/// btrfs 上任何子卷的根目录 inode 恒为 256，其内部普通 inode 从 257 起分配，因此“目录 inode
/// 等于 256”可以作为嵌套子卷的快速候选；候选再用 `BTRFS_IOC_INO_LOOKUP` 确认子卷 ID，
/// 不凭 inode 单独下结论。
pub const SUBVOLUME_ROOT_INODE: u64 = 256;

/// 打开的句柄用于哪一步（错误信息里区分路径句柄与可读句柄）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleRole {
    /// `O_PATH` 句柄，供 `fstatfs`/`statx`。
    Path,
    /// `O_RDONLY` 句柄，供 `ioctl`。
    Read,
}

impl HandleRole {
    fn as_str(self) -> &'static str {
        match self {
            HandleRole::Path => "path",
            HandleRole::Read => "read",
        }
    }
}

/// 挂载 ID 的来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountIdSource {
    /// `statx` 直接返回了 `STATX_MNT_ID`（内核 5.8 起）。
    StatxMountId,
    /// 内核未提供 `STATX_MNT_ID`，退化为“覆盖该路径的最长挂载点”。
    DeviceAndPathPrefix,
}

impl MountIdSource {
    /// 稳定输出用的短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MountIdSource::StatxMountId => "statx_mnt_id",
            MountIdSource::DeviceAndPathPrefix => "device_and_path_prefix",
        }
    }
}

/// 子卷 ID 的来源。既有内核版本差异，也有权限差异：`BTRFS_IOC_INO_LOOKUP` 需要
/// `CAP_SYS_ADMIN`，而 `statx` 的 `STATX_SUBVOL` 不需要特权。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubvolumeIdSource {
    /// `statx` 的 `STATX_SUBVOL`/`stx_subvol`（内核 6.7 起，无需特权）。
    StatxSubvol,
    /// `BTRFS_IOC_INO_LOOKUP` 的 `treeid`（需要 `CAP_SYS_ADMIN`）。
    BtrfsInoLookup,
    /// mountinfo 超级块选项里的 `subvolid=`（仅当目录本身就是挂载点）。
    MountSuperOption,
}

impl SubvolumeIdSource {
    /// 稳定输出用的短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SubvolumeIdSource::StatxSubvol => "statx_subvol",
            SubvolumeIdSource::BtrfsInoLookup => "btrfs_ino_lookup",
            SubvolumeIdSource::MountSuperOption => "mountinfo_super_option",
        }
    }
}

/// btrfs 专属身份：文件系统 UUID（FSID）与子卷 ID。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BtrfsFacts {
    /// 文件系统 UUID（`BTRFS_IOC_FS_INFO` 的 `fsid`）。
    pub fsid: [u8; 16],
    /// 句柄所在子卷的 ID；所有来源都取不到时为 `None`（此时准入必须拒绝）。
    pub subvolume_id: Option<u64>,
    /// 子卷 ID 的来源。
    pub subvolume_id_source: Option<SubvolumeIdSource>,
    /// 取子卷 ID 失败的原始原因（成功时为 `None`）；不吞错，随结果一起上报。
    pub subvolume_id_error: Option<String>,
}

/// 一个目录的存储事实（全部来自打开句柄或本进程 mountinfo）。
#[derive(Clone, Debug)]
pub struct StorageFacts {
    /// 调用方给出的路径（原样保留，便于报告）。
    pub given_path: PathBuf,
    /// 规范化后的路径，也用于路径关系比较。
    pub resolved_path: PathBuf,
    /// `fstatfs` 的类型幻数（后端类型的权威来源）。
    pub fs_magic: i64,
    /// 挂载记录里的 fstype 名。
    pub fs_type: String,
    /// `fstatfs` 自己的文件系统标识（只作辅助证据；btrfs 上随子卷变化）。
    pub statfs_fsid: [i32; 2],
    /// 该目录所在挂载的完整记录。
    pub mount: MountEntry,
    /// 挂载 ID 的取得方式。
    pub mount_id_source: MountIdSource,
    /// `statx` 报告的 inode（只在所属子卷内解释）。
    pub inode: u64,
    /// `statx` 报告的设备号（btrfs 上每个子卷有独立匿名设备号，与 mountinfo 的后端设备不同）。
    pub device: DeviceId,
    /// `statx` 报告的创建时间（文件系统不支持时为 `None`）。
    pub btime: Option<(i64, u32)>,
    /// btrfs 专属身份；非 btrfs 后端为 `None`。
    pub btrfs: Option<BtrfsFacts>,
    /// `FS_IOC_GETFLAGS` 读到的 inode 标志；后端不支持或无权读取时为 `None`。
    pub inode_flags: Option<u32>,
    /// `FS_IOC_GETFLAGS` 失败原因（`inode_flags` 为 `None` 时保留原始错误，不吞掉）。
    pub inode_flags_error: Option<String>,
}

/// 根目录内发现的嵌套子卷。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NestedSubvolume {
    /// 相对根目录的路径。
    pub relative_path: PathBuf,
    /// 实测子卷 ID。
    pub subvolume_id: u64,
}

/// 根目录内发现带 NOCOW 标志的目录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NocowDirectory {
    /// 相对根目录的路径。
    pub relative_path: PathBuf,
    /// `FS_IOC_GETFLAGS` 读到的标志。
    pub flags: u32,
}

/// 目录遍历的完整性。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanCompleteness {
    /// 遍历完整个树（未触发任何上限）。
    Complete,
    /// 达到深度上限，未继续遍历。
    DepthLimitReached {
        /// 触发上限的目录（相对根目录）。
        relative_path: PathBuf,
    },
    /// 达到条目上限，未继续遍历。
    EntryLimitReached {
        /// 停止时正在处理的目录（相对根目录）。
        relative_path: PathBuf,
    },
}

/// 遍历策略上限。达到上限时结果是“未完成”，由准入判定保守处理，不当作通过。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanPolicy {
    /// 最大检查的目录条目数。
    pub max_entries: u64,
    /// 最大递归深度（根目录为 0）。
    pub max_depth: u32,
}

impl Default for ScanPolicy {
    fn default() -> Self {
        // 原型默认值：单 Root 两万条目、32 层深。超过即报“未完成”，由部署侧显式调大，
        // 不静默当作完整扫描。
        Self {
            max_entries: 20_000,
            max_depth: 32,
        }
    }
}

/// 一次根目录遍历的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanReport {
    /// 发现的嵌套子卷。
    pub subvolumes: Vec<NestedSubvolume>,
    /// 发现的 NOCOW 目录（不含根目录自身）。
    pub nocow_directories: Vec<NocowDirectory>,
    /// 实际检查的目录条目数。
    pub entries_scanned: u64,
    /// 遍历完整性。
    pub completeness: ScanCompleteness,
}

/// 探针错误：全部保留路径与原始 `io::Error`，不吞错。
#[derive(Debug)]
pub enum ProbeError {
    /// `canonicalize` 失败（不存在、无权限等）。
    Canonicalize {
        /// 调用方给出的路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 打开目录句柄失败。
    OpenHandle {
        /// 规范化后的路径。
        path: PathBuf,
        /// 句柄用途。
        role: HandleRole,
        /// 原始错误。
        source: io::Error,
    },
    /// 路径不是目录。
    NotDirectory {
        /// 规范化后的路径。
        path: PathBuf,
        /// 原始错误（`ENOTDIR`）。
        source: io::Error,
    },
    /// `fstatfs` 失败。
    StatFs {
        /// 规范化后的路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// `statx` 失败。
    StatX {
        /// 规范化后的路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 两个句柄指向不同 inode（路径在两次打开之间被替换）。
    HandleMismatch {
        /// 规范化后的路径。
        path: PathBuf,
        /// `O_PATH` 句柄报告的 inode。
        path_inode: u64,
        /// 可读句柄报告的 inode。
        read_inode: u64,
    },
    /// `/proc/self/mountinfo` 读取失败。
    MountInfo(io::Error),
    /// mountinfo 里没有该挂载 ID 的记录。
    MountEntryMissing {
        /// 规范化后的路径。
        path: PathBuf,
        /// 期望的挂载 ID。
        mount_id: u64,
    },
    /// mountinfo 里找不到覆盖该路径的挂载记录（退化路径）。
    MountEntryUnresolved {
        /// 规范化后的路径。
        path: PathBuf,
    },
    /// `BTRFS_IOC_FS_INFO` 失败。
    BtrfsFsInfo {
        /// 规范化后的路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// `BTRFS_IOC_INO_LOOKUP` 失败。
    BtrfsInoLookup {
        /// 规范化后的路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 读目录失败。
    ReadDir {
        /// 规范化后的路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 读单个目录项失败。
    DirEntry {
        /// 所在目录。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 无法解析候选子卷的子卷 ID。
    SubvolumeIdUnavailable {
        /// 候选目录路径。
        path: PathBuf,
        /// 失败细节（原始错误或内核能力缺失说明）。
        detail: String,
    },
    /// `FS_IOC_GETFLAGS` 失败（无法判定 NOCOW）。
    InodeFlags {
        /// 目录路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
    /// 读目录项元数据失败。
    Metadata {
        /// 目录项路径。
        path: PathBuf,
        /// 原始错误。
        source: io::Error,
    },
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProbeError::Canonicalize { path, source } => {
                write!(f, "无法规范化路径 {}：{source}", path.display())
            }
            ProbeError::OpenHandle { path, role, source } => write!(
                f,
                "无法打开 {} 句柄（role={}）：{source}",
                path.display(),
                role.as_str()
            ),
            ProbeError::NotDirectory { path, source } => {
                write!(f, "{} 不是目录：{source}", path.display())
            }
            ProbeError::StatFs { path, source } => {
                write!(f, "fstatfs 失败 {}：{source}", path.display())
            }
            ProbeError::StatX { path, source } => {
                write!(f, "statx 失败 {}：{source}", path.display())
            }
            ProbeError::HandleMismatch {
                path,
                path_inode,
                read_inode,
            } => write!(
                f,
                "{} 在两次打开之间被替换（inode {path_inode} -> {read_inode}），拒绝继续",
                path.display()
            ),
            ProbeError::MountInfo(source) => write!(f, "读取 mountinfo 失败：{source}"),
            ProbeError::MountEntryMissing { path, mount_id } => write!(
                f,
                "mountinfo 缺少挂载 ID {mount_id} 的记录（路径 {}）",
                path.display()
            ),
            ProbeError::MountEntryUnresolved { path } => {
                write!(f, "mountinfo 中找不到覆盖 {} 的挂载记录", path.display())
            }
            ProbeError::BtrfsFsInfo { path, source } => {
                write!(f, "BTRFS_IOC_FS_INFO 失败 {}：{source}", path.display())
            }
            ProbeError::BtrfsInoLookup { path, source } => {
                write!(f, "BTRFS_IOC_INO_LOOKUP 失败 {}：{source}", path.display())
            }
            ProbeError::SubvolumeIdUnavailable { path, detail } => {
                write!(f, "无法确认 {} 的子卷 ID：{detail}", path.display())
            }
            ProbeError::InodeFlags { path, source } => {
                write!(f, "FS_IOC_GETFLAGS 失败 {}：{source}", path.display())
            }
            ProbeError::ReadDir { path, source } => {
                write!(f, "读取目录失败 {}：{source}", path.display())
            }
            ProbeError::DirEntry { path, source } => {
                write!(f, "读取目录项失败 {}：{source}", path.display())
            }
            ProbeError::Metadata { path, source } => {
                write!(f, "读取元数据失败 {}：{source}", path.display())
            }
        }
    }
}

impl std::error::Error for ProbeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProbeError::MountInfo(source) => Some(source),
            ProbeError::Canonicalize { source, .. }
            | ProbeError::OpenHandle { source, .. }
            | ProbeError::NotDirectory { source, .. }
            | ProbeError::StatFs { source, .. }
            | ProbeError::StatX { source, .. }
            | ProbeError::BtrfsFsInfo { source, .. }
            | ProbeError::BtrfsInoLookup { source, .. }
            | ProbeError::InodeFlags { source, .. }
            | ProbeError::ReadDir { source, .. }
            | ProbeError::DirEntry { source, .. }
            | ProbeError::Metadata { source, .. } => Some(source),
            ProbeError::HandleMismatch { .. }
            | ProbeError::MountEntryMissing { .. }
            | ProbeError::MountEntryUnresolved { .. }
            | ProbeError::SubvolumeIdUnavailable { .. } => None,
        }
    }
}

fn open_handle(path: &Path, custom_flags: i32, role: HandleRole) -> Result<File, ProbeError> {
    let mut options = OpenOptions::new();
    // `O_PATH` 会忽略访问模式位，这里统一带只读访问模式即可；`role` 仍决定句柄用途。
    options.read(true);
    options
        .custom_flags(custom_flags | raw::O_DIRECTORY | raw::O_NOFOLLOW)
        .open(path)
        .map_err(|source| open_error(path, role, source))
}

/// `ENOTDIR`：把“路径不是目录”从普通打开失败里分出来。
const ENOTDIR: i32 = 20;

fn open_error(path: &Path, role: HandleRole, source: io::Error) -> ProbeError {
    if source.raw_os_error() == Some(ENOTDIR) {
        ProbeError::NotDirectory {
            path: path.to_path_buf(),
            source,
        }
    } else {
        ProbeError::OpenHandle {
            path: path.to_path_buf(),
            role,
            source,
        }
    }
}

/// 探测一个目录的存储事实。
///
/// # Errors
///
/// 见 [`ProbeError`]：路径不可规范化、不是目录、句柄在打开过程被替换、mountinfo 记录缺失或
/// 设备不一致、btrfs ioctl 失败时都返回明确错误，不返回“看起来正常”的默认值。
pub fn probe_path(path: &Path) -> Result<StorageFacts, ProbeError> {
    let resolved_path = std::fs::canonicalize(path).map_err(|source| ProbeError::Canonicalize {
        path: path.to_path_buf(),
        source,
    })?;
    let path_handle = open_handle(&resolved_path, raw::O_PATH, HandleRole::Path)?;
    let statfs =
        raw::statfs_of_fd(path_handle.as_raw_fd()).map_err(|source| ProbeError::StatFs {
            path: resolved_path.clone(),
            source,
        })?;
    let statx = raw::statx_of_fd(path_handle.as_raw_fd()).map_err(|source| ProbeError::StatX {
        path: resolved_path.clone(),
        source,
    })?;
    let read_handle = open_handle(&resolved_path, 0, HandleRole::Read)?;
    let read_metadata = read_handle.metadata().map_err(|source| ProbeError::StatX {
        path: resolved_path.clone(),
        source,
    })?;
    if read_metadata.ino() != statx.inode {
        return Err(ProbeError::HandleMismatch {
            path: resolved_path,
            path_inode: statx.inode,
            read_inode: read_metadata.ino(),
        });
    }

    let entries = mountinfo::read_self().map_err(ProbeError::MountInfo)?;
    let device = DeviceId {
        major: statx.dev_major,
        minor: statx.dev_minor,
    };
    let (mount, mount_id_source) = resolve_mount(&entries, &statx, &resolved_path)?;

    let (inode_flags, inode_flags_error) = match raw::inode_flags(read_handle.as_raw_fd()) {
        Ok(flags) => (Some(flags), None),
        Err(error) => (None, Some(error.to_string())),
    };

    let btrfs = if statfs.f_type == BTRFS_SUPER_MAGIC {
        let fsid =
            raw::btrfs_fsid(read_handle.as_raw_fd()).map_err(|source| ProbeError::BtrfsFsInfo {
                path: resolved_path.clone(),
                source,
            })?;
        let mut resolved = resolve_subvolume_id(read_handle.as_raw_fd(), &statx);
        if resolved.id.is_none() && resolved_path == mount.mount_point {
            // 目录本身就是挂载点时，mountinfo 的 `subvolid=` 是同一子卷的权威记录。
            if let Some(id) = mount_subvolid(mount) {
                resolved = SubvolumeResolution {
                    id: Some(id),
                    source: Some(SubvolumeIdSource::MountSuperOption),
                    error: resolved.error,
                };
            }
        }
        Some(BtrfsFacts {
            fsid,
            subvolume_id: resolved.id,
            subvolume_id_source: resolved.source,
            subvolume_id_error: resolved.error,
        })
    } else {
        None
    };

    Ok(StorageFacts {
        given_path: path.to_path_buf(),
        resolved_path,
        fs_magic: statfs.f_type,
        fs_type: mount.fs_type.clone(),
        statfs_fsid: statfs.statfs_fsid,
        mount: mount.clone(),
        mount_id_source,
        inode: statx.inode,
        device,
        btime: statx.btime,
        btrfs,
        inode_flags,
        inode_flags_error,
    })
}

/// 读取任意路径（文件或目录）的 inode 标志（`FS_IOC_GETFLAGS`），只读。
///
/// # Errors
///
/// 路径不可规范化、无法只读打开或 ioctl 失败时返回 [`ProbeError`]。
pub fn inode_flags_of_path(path: &Path) -> Result<u32, ProbeError> {
    let resolved_path = std::fs::canonicalize(path).map_err(|source| ProbeError::Canonicalize {
        path: path.to_path_buf(),
        source,
    })?;
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(raw::O_NOFOLLOW)
        .open(&resolved_path)
        .map_err(|source| ProbeError::OpenHandle {
            path: resolved_path.clone(),
            role: HandleRole::Read,
            source,
        })?;
    raw::inode_flags(handle.as_raw_fd()).map_err(|source| ProbeError::InodeFlags {
        path: resolved_path,
        source,
    })
}

/// 一次子卷 ID 解析的结果。
struct SubvolumeResolution {
    id: Option<u64>,
    source: Option<SubvolumeIdSource>,
    error: Option<String>,
}

/// 优先用无特权的 `statx`（`STATX_SUBVOL`），再退回需要 `CAP_SYS_ADMIN` 的 ioctl。
fn resolve_subvolume_id(fd: std::os::fd::RawFd, statx: &raw::StatxFacts) -> SubvolumeResolution {
    if let Some(id) = statx.subvol_id {
        return SubvolumeResolution {
            id: Some(id),
            source: Some(SubvolumeIdSource::StatxSubvol),
            error: None,
        };
    }
    match raw::btrfs_subvolume_id(fd) {
        Ok(id) => SubvolumeResolution {
            id: Some(id),
            source: Some(SubvolumeIdSource::BtrfsInoLookup),
            error: None,
        },
        Err(error) => SubvolumeResolution {
            id: None,
            source: None,
            error: Some(error.to_string()),
        },
    }
}

/// 从 mountinfo 超级块选项里取 `subvolid=`。
fn mount_subvolid(mount: &MountEntry) -> Option<u64> {
    mount
        .super_options
        .iter()
        .find_map(|option| option.strip_prefix("subvolid="))
        .and_then(|value| value.parse::<u64>().ok())
}

fn resolve_mount<'a>(
    entries: &'a [MountEntry],
    statx: &raw::StatxFacts,
    resolved_path: &Path,
) -> Result<(&'a MountEntry, MountIdSource), ProbeError> {
    if let Some(mount_id) = statx.mnt_id {
        let entry = mountinfo::find_by_mount_id(entries, mount_id).ok_or_else(|| {
            ProbeError::MountEntryMissing {
                path: resolved_path.to_path_buf(),
                mount_id,
            }
        })?;
        return Ok((entry, MountIdSource::StatxMountId));
    }

    // 退化路径：内核未提供 `STATX_MNT_ID` 时按“覆盖该路径的最长挂载点”定位。不能按设备号
    // 过滤——btrfs 每个子卷使用独立的匿名设备号，`statx` 的设备与 `mountinfo` 的后端设备
    // 本来就不同（实测见 docs/environment.md §15）。
    let entry = entries
        .iter()
        .filter(|entry| paths::is_within(resolved_path, &entry.mount_point))
        .max_by_key(|entry| entry.mount_point.components().count())
        .ok_or_else(|| ProbeError::MountEntryUnresolved {
            path: resolved_path.to_path_buf(),
        })?;
    Ok((entry, MountIdSource::DeviceAndPathPrefix))
}

/// 遍历根目录，找嵌套子卷与 NOCOW 目录（只用于 btrfs Root）。
///
/// 只遍历普通目录：符号链接一律不跟随（design §16.1），进入嵌套子卷前就记录并停止下降，
/// 条目数、深度达到 [`ScanPolicy`] 上限时返回“未完成”而不假装扫完。
///
/// `root_subvolume_id` 由调用方从 Root 的探测事实传入：子卷内的普通目录继承父目录的子卷 ID，
/// 只有 inode 为 [`SUBVOLUME_ROOT_INODE`] 的候选才需要单独解析子卷 ID，解析不出来就报错，
/// 不把“无法确认”当成“同子卷”。
///
/// # Errors
///
/// 目录读取、目录项元数据、候选子卷 ID 或目录 NOCOW 标志读取失败时返回 [`ProbeError`]；
/// 权限问题不会被当成“没有嵌套子卷”。
pub fn scan_root(
    root: &Path,
    root_subvolume_id: u64,
    policy: &ScanPolicy,
) -> Result<ScanReport, ProbeError> {
    let root = std::fs::canonicalize(root).map_err(|source| ProbeError::Canonicalize {
        path: root.to_path_buf(),
        source,
    })?;

    let mut report = ScanReport {
        subvolumes: Vec::new(),
        nocow_directories: Vec::new(),
        entries_scanned: 0,
        completeness: ScanCompleteness::Complete,
    };
    let mut pending: Vec<(PathBuf, u32)> = vec![(PathBuf::new(), 0)];

    while let Some((relative_dir, depth)) = pending.pop() {
        let directory = if relative_dir.as_os_str().is_empty() {
            root.clone()
        } else {
            root.join(&relative_dir)
        };
        let read_dir = std::fs::read_dir(&directory).map_err(|source| ProbeError::ReadDir {
            path: directory.clone(),
            source,
        })?;
        for entry in read_dir {
            let entry = entry.map_err(|source| ProbeError::DirEntry {
                path: directory.clone(),
                source,
            })?;
            if report.entries_scanned >= policy.max_entries {
                report.completeness = ScanCompleteness::EntryLimitReached {
                    relative_path: relative_dir.clone(),
                };
                pending.clear();
                return Ok(report);
            }
            report.entries_scanned += 1;

            let entry_path = entry.path();
            let metadata =
                std::fs::symlink_metadata(&entry_path).map_err(|source| ProbeError::Metadata {
                    path: entry_path.clone(),
                    source,
                })?;
            if !metadata.is_dir() {
                continue;
            }
            let relative_path = paths::strip_prefix(&entry_path, &root).unwrap_or_else(|| {
                // canonicalize 之后仍出现非后代路径只可能是目录在遍历中被替换；按绝对路径报。
                entry_path.clone()
            });

            let handle = open_handle(&entry_path, 0, HandleRole::Read)?;
            let entry_statx =
                raw::statx_of_fd(handle.as_raw_fd()).map_err(|source| ProbeError::StatX {
                    path: entry_path.clone(),
                    source,
                })?;
            if metadata.ino() == SUBVOLUME_ROOT_INODE {
                // 子卷根目录的 inode 恒为 256；再用 statx/ioctl 确认子卷 ID，不凭 inode 单独结论。
                let resolved = resolve_subvolume_id(handle.as_raw_fd(), &entry_statx);
                let Some(subvolume_id) = resolved.id else {
                    return Err(ProbeError::SubvolumeIdUnavailable {
                        path: entry_path.clone(),
                        detail: resolved.error.unwrap_or_else(|| {
                            "内核未提供 STATX_SUBVOL 且 ioctl 不可用".to_owned()
                        }),
                    });
                };
                if subvolume_id != root_subvolume_id {
                    report.subvolumes.push(NestedSubvolume {
                        relative_path,
                        subvolume_id,
                    });
                    continue;
                }
            }

            match raw::inode_flags(handle.as_raw_fd()) {
                Ok(flags) => {
                    if raw::has_nocow(flags) {
                        report.nocow_directories.push(NocowDirectory {
                            relative_path: relative_path.clone(),
                            flags,
                        });
                    }
                }
                // NOCOW 检查依赖 GETFLAGS；读不到标志时不能当作“合规”，直接上报原始错误。
                Err(source) => {
                    return Err(ProbeError::InodeFlags {
                        path: entry_path,
                        source,
                    });
                }
            }

            if depth + 1 > policy.max_depth {
                report.completeness = ScanCompleteness::DepthLimitReached {
                    relative_path: relative_path.clone(),
                };
                pending.clear();
                return Ok(report);
            }
            pending.push((relative_path, depth + 1));
        }
    }

    Ok(report)
}
