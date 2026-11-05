//! 存储准入判定：把 [`StorageFacts`] 与挂载/扫描事实变成结构化的接受或拒绝（纯逻辑）。
//!
//! 判定顺序固定，先报最先命中的原因，便于测试与运维定位：
//!
//! 1. 后端必须是 btrfs——FUSE（含 `fuse.*` 子类型与 `fuseblk`）、NFS、CIFS/SMB、overlay、
//!    tmpfs、ext4/xfs/zfs 等一律拒绝（design §11.4、C39）。
//! 2. btrfs 的 FSID 与子卷 ID 必须取得（无法确认身份就不接受）。
//! 3. inode 标志必须可读且不含 `FS_NOCOW_FL`（`+C`）。
//! 4. 挂载选项不得含 `nodatacow`/`nodatasum`。
//! 5. 扫描必须完整（受限上限未触发），否则结论是“未完成”，不是通过。
//! 6. Root 内不得有嵌套子卷，也不得有子挂载（任何类型的挂载点都不行）。
//! 7. 内部状态目录必须与 Root 不重叠、本身也在合规 btrfs 上；与 Root 不在同一 btrfs 文件
//!    系统时拒绝（design §11.5 允许“显式记录复制成本”地跨卷，这里保守拒绝并记录双方 FSID）。
//! 8. 已注册 Root 不得是同一目录的别名，也不得与 Root 路径重叠（design §16.1）。
//!
//! 别名/重叠不能只比路径或 inode：同一子卷的两个路径（bind mount、`..`、符号链接）得到的
//! 是相同的 `(FSID, 子卷 ID)`，因此判“同一存储”；inode 只在同一子卷内比较。

use crate::mounts::mountinfo::MountEntry;
use crate::mounts::paths::{self, PathRelation};
use crate::mounts::probe::{ScanCompleteness, ScanReport, StorageFacts};
use std::fmt;
use std::path::{Path, PathBuf};

/// btrfs 超级块幻数（`BTRFS_SUPER_MAGIC`，`linux/magic.h`）。
pub const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
/// tmpfs 幻数。
pub const TMPFS_MAGIC: i64 = 0x0102_1994;
/// FUSE 幻数（`fuse` 与 `fuseblk` 共用）。
pub const FUSE_SUPER_MAGIC: i64 = 0x6573_5546;
/// overlayfs 幻数。
pub const OVERLAYFS_SUPER_MAGIC: i64 = 0x794c_7630;
/// NFS 幻数。
pub const NFS_SUPER_MAGIC: i64 = 0x0000_6969;
/// CIFS/SMB1 幻数。
pub const CIFS_MAGIC_NUMBER: i64 = 0xff53_4d42;
/// SMB2/SMB3 幻数。
pub const SMB2_MAGIC_NUMBER: i64 = 0xfe53_4d42;
/// 早期 smbfs 幻数。
pub const SMB_SUPER_MAGIC: i64 = 0x0000_517b;
/// ext2/3/4 幻数。
pub const EXT4_SUPER_MAGIC: i64 = 0x0000_ef53;
/// XFS 幻数。
pub const XFS_SUPER_MAGIC: i64 = 0x5846_5342;
/// ZFS 幻数。
pub const ZFS_SUPER_MAGIC: i64 = 0x2fc1_2fc1;

/// `FS_NOCOW_FL`：btrfs 的 NOCOW/`+C` 标志（`linux/fs.h`）。
pub const FS_NOCOW_FL: u32 = crate::mounts::raw::FS_NOCOW_FL;

/// 把 16 字节 btrfs FSID 渲染成与 `btrfs filesystem show` 一致的 UUID 文本。
#[must_use]
pub fn fsid_text(fsid: &[u8; 16]) -> String {
    crate::mounts::raw::fsid_to_string(fsid)
}

/// 标志里是否含 NOCOW（`FS_NOCOW_FL`）。
#[must_use]
pub fn has_nocow_flag(flags: u32) -> bool {
    crate::mounts::raw::has_nocow(flags)
}

/// 后端类型（只区分准入需要区分的种类，其余归为 [`BackendKind::Other`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// 本地 btrfs（唯一接受的数据后端）。
    Btrfs,
    /// FUSE（`fuse`、`fuse.sshfs`、`fuse.rclone` 等子类型）。
    Fuse,
    /// `fuseblk`（基于块设备的 FUSE，如 ntfs-3g）。
    Fuseblk,
    /// NFS（含 nfs4）。
    Nfs,
    /// CIFS/SMB。
    Cifs,
    /// overlayfs。
    Overlay,
    /// tmpfs。
    Tmpfs,
    /// ext2/3/4。
    Ext4,
    /// XFS。
    Xfs,
    /// ZFS。
    Zfs,
    /// 其它后端。
    Other,
}

impl BackendKind {
    /// 稳定输出用的短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Btrfs => "btrfs",
            BackendKind::Fuse => "fuse",
            BackendKind::Fuseblk => "fuseblk",
            BackendKind::Nfs => "nfs",
            BackendKind::Cifs => "cifs",
            BackendKind::Overlay => "overlay",
            BackendKind::Tmpfs => "tmpfs",
            BackendKind::Ext4 => "ext4",
            BackendKind::Xfs => "xfs",
            BackendKind::Zfs => "zfs",
            BackendKind::Other => "other",
        }
    }
    /// 按短名解析（与 [`BackendKind::as_str`] 互为逆运算，用于读回已记录的规格）。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let kind = match value {
            "btrfs" => BackendKind::Btrfs,
            "fuse" => BackendKind::Fuse,
            "fuseblk" => BackendKind::Fuseblk,
            "nfs" => BackendKind::Nfs,
            "cifs" => BackendKind::Cifs,
            "overlay" => BackendKind::Overlay,
            "tmpfs" => BackendKind::Tmpfs,
            "ext4" => BackendKind::Ext4,
            "xfs" => BackendKind::Xfs,
            "zfs" => BackendKind::Zfs,
            "other" => BackendKind::Other,
            _ => return None,
        };
        Some(kind)
    }
}

/// 按 `fstatfs` 幻数分类后端，幻数不认识时退回 fstype 名。
///
/// 幻数是权威来源；fstype 只用于幻数缺失（未知新后端）或与幻数无关的 FUSE 子类型区分，
/// 两者都来自内核，不来自用户输入。
#[must_use]
pub fn classify_backend(fs_magic: i64, fs_type: &str) -> BackendKind {
    match fs_magic {
        BTRFS_SUPER_MAGIC => BackendKind::Btrfs,
        FUSE_SUPER_MAGIC => {
            if fs_type == "fuseblk" {
                BackendKind::Fuseblk
            } else {
                BackendKind::Fuse
            }
        }
        NFS_SUPER_MAGIC => BackendKind::Nfs,
        CIFS_MAGIC_NUMBER | SMB2_MAGIC_NUMBER | SMB_SUPER_MAGIC => BackendKind::Cifs,
        OVERLAYFS_SUPER_MAGIC => BackendKind::Overlay,
        TMPFS_MAGIC => BackendKind::Tmpfs,
        EXT4_SUPER_MAGIC => BackendKind::Ext4,
        XFS_SUPER_MAGIC => BackendKind::Xfs,
        ZFS_SUPER_MAGIC => BackendKind::Zfs,
        _ => classify_backend_by_fs_type(fs_type),
    }
}

fn classify_backend_by_fs_type(fs_type: &str) -> BackendKind {
    if fs_type == "fuseblk" {
        BackendKind::Fuseblk
    } else if fs_type == "fuse" || fs_type.starts_with("fuse.") {
        BackendKind::Fuse
    } else if fs_type == "btrfs" {
        BackendKind::Btrfs
    } else if fs_type == "nfs" || fs_type == "nfs4" {
        BackendKind::Nfs
    } else if fs_type == "cifs" || fs_type.starts_with("smb") || fs_type == "smbfs" {
        BackendKind::Cifs
    } else if fs_type == "overlay" {
        BackendKind::Overlay
    } else if fs_type == "tmpfs" {
        BackendKind::Tmpfs
    } else if fs_type.starts_with("ext") {
        BackendKind::Ext4
    } else if fs_type == "xfs" {
        BackendKind::Xfs
    } else if fs_type == "zfs" {
        BackendKind::Zfs
    } else {
        BackendKind::Other
    }
}

/// 目录的存储身份：`(FSID, 子卷 ID)` 界定存储单元，inode 在子卷内解释。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageIdentity {
    /// btrfs 文件系统 UUID。
    pub fsid: [u8; 16],
    /// 子卷 ID。
    pub subvolume_id: u64,
    /// 该目录在所属子卷内的 inode。
    pub inode: u64,
}

/// 取目录的存储身份；非 btrfs 或身份不可读时为 `None`。
#[must_use]
pub fn storage_identity(facts: &StorageFacts) -> Option<StorageIdentity> {
    facts.btrfs.as_ref().and_then(|btrfs| {
        Some(StorageIdentity {
            fsid: btrfs.fsid,
            subvolume_id: btrfs.subvolume_id?,
            inode: facts.inode,
        })
    })
}

/// 两个目录是否属于同一存储单元（同一 btrfs 文件系统的同一子卷）。
///
/// 同一子卷的不同路径（bind mount、`..`、符号链接）在这里相等，因此不会被当成两个存储。
#[must_use]
pub fn same_storage(left: &StorageFacts, right: &StorageFacts) -> bool {
    match (storage_identity(left), storage_identity(right)) {
        (Some(a), Some(b)) => a.fsid == b.fsid && a.subvolume_id == b.subvolume_id,
        _ => false,
    }
}

/// 两个目录是否是同一个目录（同存储单元且 inode 相同）。
#[must_use]
pub fn same_directory(left: &StorageFacts, right: &StorageFacts) -> bool {
    match (storage_identity(left), storage_identity(right)) {
        (Some(a), Some(b)) => {
            a.fsid == b.fsid && a.subvolume_id == b.subvolume_id && a.inode == b.inode
        }
        _ => false,
    }
}

/// 身份漂移：启动/挂载变化后重新核对时发现的差异（design §11.5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityDrift {
    /// 文件系统 UUID 变化（卷被替换或回滚）。
    FilesystemChanged,
    /// 子卷 ID 变化（子卷被替换）。
    SubvolumeChanged,
    /// 目录 inode 变化（目录被替换）。
    DirectoryChanged,
}

impl IdentityDrift {
    /// 稳定输出用的短原因。
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            IdentityDrift::FilesystemChanged => "filesystem_changed",
            IdentityDrift::SubvolumeChanged => "subvolume_changed",
            IdentityDrift::DirectoryChanged => "directory_changed",
        }
    }

    /// 对应的设计错误码：运行阶段发现变化按 `ROOT_UNAVAILABLE` 处理，不推导删除。
    #[must_use]
    pub fn design_error_code(self) -> &'static str {
        "ROOT_UNAVAILABLE"
    }
}

/// 重新核对某个已记录目录的身份是否还是同一个存储单元里的同一目录。
///
/// # Errors
///
/// 返回首个漂移原因；身份不可读时也按漂移处理（不能确认就不当作未变化）。
pub fn verify_unchanged(
    recorded: &StorageFacts,
    current: &StorageFacts,
) -> Result<(), IdentityDrift> {
    let (Some(recorded_id), Some(current_id)) =
        (storage_identity(recorded), storage_identity(current))
    else {
        return Err(IdentityDrift::SubvolumeChanged);
    };
    if recorded_id.fsid != current_id.fsid {
        return Err(IdentityDrift::FilesystemChanged);
    }
    if recorded_id.subvolume_id != current_id.subvolume_id {
        return Err(IdentityDrift::SubvolumeChanged);
    }
    if recorded_id.inode != current_id.inode {
        return Err(IdentityDrift::DirectoryChanged);
    }
    Ok(())
}

/// 拒绝原因。每个原因都有稳定的 `code()`（供测试与运维脚本匹配）与 `detail()`（人读细节）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionRejection {
    /// 后端不是 btrfs。
    UnsupportedBackend {
        /// 后端分类。
        backend: BackendKind,
        /// mountinfo 里的 fstype。
        fs_type: String,
        /// `fstatfs` 幻数。
        fs_magic: i64,
    },
    /// btrfs 上取不到 FSID 或子卷 ID。
    SubvolumeIdentityUnavailable,
    /// `FS_IOC_GETFLAGS` 失败，无法判断 NOCOW。
    InodeFlagsUnavailable {
        /// 原始错误文本。
        detail: String,
    },
    /// 目录自身带 NOCOW 标志（`+C`）。
    NocowInodeFlags {
        /// 目录路径（相对 Root；Root 自身为空路径）。
        path: PathBuf,
        /// 实测标志。
        flags: u32,
    },
    /// 挂载选项含 `nodatacow`/`nodatasum`。
    NocowMountOption {
        /// 命中的选项名。
        option: String,
    },
    /// Root 内有嵌套子卷。
    NestedSubvolume {
        /// 相对 Root 的路径。
        relative_path: PathBuf,
        /// 实测子卷 ID。
        subvolume_id: u64,
    },
    /// Root 内有子挂载（任何文件系统类型）。
    NestedMount {
        /// 挂载点路径。
        mount_point: PathBuf,
        /// 该挂载的 fstype。
        fs_type: String,
    },
    /// 内部状态目录与 Root 路径重叠（相等或互为祖先/后代）。
    InternalStateOverlap {
        /// 内部状态目录路径。
        path: PathBuf,
    },
    /// 内部状态目录所在后端不是 btrfs。
    InternalStateUnsupportedBackend {
        /// 内部状态目录路径。
        path: PathBuf,
        /// 实际 fstype。
        fs_type: String,
    },
    /// 内部状态目录与 Root 不在同一 btrfs 文件系统。
    InternalStateOtherFilesystem {
        /// 内部状态目录路径。
        path: PathBuf,
        /// 内部状态目录的 FSID（UUID 文本）。
        internal_fsid: String,
        /// Root 的 FSID（UUID 文本）。
        root_fsid: String,
    },
    /// 已注册 Root 与候选 Root 是同一个目录（别名）。
    DuplicateRoot {
        /// 已注册的路径。
        path: PathBuf,
    },
    /// 已注册 Root 与候选 Root 的路径重叠。
    OverlappingRoot {
        /// 已注册的路径。
        path: PathBuf,
    },
}

impl AdmissionRejection {
    /// 稳定短码（snake_case，测试与运维脚本按此匹配）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            AdmissionRejection::UnsupportedBackend { .. } => "unsupported_backend",
            AdmissionRejection::SubvolumeIdentityUnavailable => "subvolume_identity_unavailable",
            AdmissionRejection::InodeFlagsUnavailable { .. } => "inode_flags_unavailable",
            AdmissionRejection::NocowInodeFlags { .. } => "nocow_inode_flags",
            AdmissionRejection::NocowMountOption { .. } => "nocow_mount_option",
            AdmissionRejection::NestedSubvolume { .. } => "nested_subvolume",
            AdmissionRejection::NestedMount { .. } => "nested_mount",
            AdmissionRejection::InternalStateOverlap { .. } => "internal_state_overlap",
            AdmissionRejection::InternalStateUnsupportedBackend { .. } => {
                "internal_state_unsupported_backend"
            }
            AdmissionRejection::InternalStateOtherFilesystem { .. } => {
                "internal_state_other_filesystem"
            }
            AdmissionRejection::DuplicateRoot { .. } => "duplicate_root",
            AdmissionRejection::OverlappingRoot { .. } => "overlapping_root",
        }
    }

    /// 对应的设计错误码（design §11.4：配置阶段一律 `UNSUPPORTED_STORAGE_BACKEND`）。
    ///
    /// `SERVER_PLACEHOLDER_UNSUPPORTED` 不由本卡产生：Linux 没有可依赖的统一占位判定，
    /// 见 `docs/architecture/interfaces.md` 与 `mounts::content`。
    #[must_use]
    pub fn design_error_code(&self) -> &'static str {
        "UNSUPPORTED_STORAGE_BACKEND"
    }

    /// 人读细节。
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            AdmissionRejection::UnsupportedBackend {
                backend,
                fs_type,
                fs_magic,
            } => format!(
                "后端 {}（fstype={fs_type}，fstype 幻数 {fs_magic:#x}）不是本地 btrfs，拒绝",
                backend.as_str()
            ),
            AdmissionRejection::SubvolumeIdentityUnavailable => {
                "btrfs 上无法读取 FSID 或子卷 ID，无法确认存储身份".to_owned()
            }
            AdmissionRejection::InodeFlagsUnavailable { detail } => {
                format!("无法读取 inode 标志（FS_IOC_GETFLAGS）：{detail}")
            }
            AdmissionRejection::NocowInodeFlags { path, flags } => format!(
                "{} 带 NOCOW 标志（FS_IOC_GETFLAGS={flags:#x}，含 FS_NOCOW_FL），破坏 CoW/校验",
                display_path(path)
            ),
            AdmissionRejection::NocowMountOption { option } => {
                format!("挂载选项含 {option}，无法保证数据 CoW 与校验")
            }
            AdmissionRejection::NestedSubvolume {
                relative_path,
                subvolume_id,
            } => format!(
                "Root 内存在嵌套子卷 {}（subvolid={subvolume_id}）",
                display_path(relative_path)
            ),
            AdmissionRejection::NestedMount {
                mount_point,
                fs_type,
            } => format!(
                "Root 内存在子挂载 {}（fstype={fs_type}）",
                mount_point.display()
            ),
            AdmissionRejection::InternalStateOverlap { path } => {
                format!("内部状态目录 {} 与 Root 重叠", path.display())
            }
            AdmissionRejection::InternalStateUnsupportedBackend { path, fs_type } => format!(
                "内部状态目录 {} 后端为 {fs_type}，要求 btrfs",
                path.display()
            ),
            AdmissionRejection::InternalStateOtherFilesystem {
                path,
                internal_fsid,
                root_fsid,
            } => format!(
                "内部状态目录 {} 与 Root 不在同一 btrfs 文件系统（{internal_fsid} vs {root_fsid}）",
                path.display()
            ),
            AdmissionRejection::DuplicateRoot { path } => format!(
                "候选 Root 与已注册 Root {} 是同一目录（别名），不能重复注册",
                path.display()
            ),
            AdmissionRejection::OverlappingRoot { path } => {
                format!("候选 Root 与已注册 Root {} 路径重叠", path.display())
            }
        }
    }
}

impl fmt::Display for AdmissionRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}（{}）", self.code(), self.detail())
    }
}

/// 结论未定（扫描不完整）：不等于通过，需要更严格的上限或分片扫描。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InconclusiveReason {
    /// 目录遍历达到深度上限。
    ScanDepthLimitReached {
        /// 相对 Root 的路径。
        relative_path: PathBuf,
    },
    /// 目录遍历达到条目上限。
    ScanEntryLimitReached {
        /// 相对 Root 的路径。
        relative_path: PathBuf,
    },
}

impl InconclusiveReason {
    /// 稳定短码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            InconclusiveReason::ScanDepthLimitReached { .. } => "scan_depth_limit_reached",
            InconclusiveReason::ScanEntryLimitReached { .. } => "scan_entry_limit_reached",
        }
    }
}

/// 判定结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// 接受。
    Accepted,
    /// 拒绝，附明确原因。
    Rejected(AdmissionRejection),
    /// 无法完成判定（当前扫描上限内不能证明合规）。
    Inconclusive(InconclusiveReason),
}

impl AdmissionDecision {
    /// 稳定短名（`accepted`/`rejected`/`inconclusive`）。
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            AdmissionDecision::Accepted => "accepted",
            AdmissionDecision::Rejected(_) => "rejected",
            AdmissionDecision::Inconclusive(_) => "inconclusive",
        }
    }
}

/// 判定输出：结论 + 结构化备注（`key=value` 形式，按发生顺序追加）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionOutcome {
    /// 结论。
    pub decision: AdmissionDecision,
    /// 备注（不影响结论，用于记录“接受但需知会”的事实）。
    pub notes: Vec<String>,
}

impl AdmissionOutcome {
    fn rejected(rejection: AdmissionRejection) -> Self {
        Self {
            decision: AdmissionDecision::Rejected(rejection),
            notes: Vec::new(),
        }
    }

    fn inconclusive(reason: InconclusiveReason) -> Self {
        Self {
            decision: AdmissionDecision::Inconclusive(reason),
            notes: Vec::new(),
        }
    }

    fn push_note(&mut self, key: &str, value: impl fmt::Display) {
        self.notes.push(format!("{key}={value}"));
    }
}

/// 判定输入。
pub struct AdmissionInput<'a> {
    /// 候选 Root 的探测事实。
    pub root: &'a StorageFacts,
    /// 内部状态目录（数据库、对象、历史、暂存、备份）的探测事实。
    pub internal_state_dirs: &'a [StorageFacts],
    /// 已经注册的 Root 的探测事实（别名与重叠检测）。
    pub registered_roots: &'a [StorageFacts],
    /// 本进程的挂载表（用于发现 Root 内的子挂载）。
    pub mounts: &'a [MountEntry],
    /// Root 目录遍历结果。
    pub scan: &'a ScanReport,
}

/// 执行准入判定。
#[must_use]
pub fn evaluate(input: &AdmissionInput<'_>) -> AdmissionOutcome {
    let root = input.root;
    let backend = classify_backend(root.fs_magic, &root.fs_type);
    if backend != BackendKind::Btrfs {
        return AdmissionOutcome::rejected(AdmissionRejection::UnsupportedBackend {
            backend,
            fs_type: root.fs_type.clone(),
            fs_magic: root.fs_magic,
        });
    }

    let Some(identity) = storage_identity(root) else {
        return AdmissionOutcome::rejected(AdmissionRejection::SubvolumeIdentityUnavailable);
    };
    let mut outcome = AdmissionOutcome {
        decision: AdmissionDecision::Accepted,
        notes: Vec::new(),
    };
    outcome.push_note("fsid", fsid_text(&identity.fsid));
    outcome.push_note("subvol_id", identity.subvolume_id);
    outcome.push_note("mount_id", root.mount.mount_id);
    outcome.push_note("mount_id_source", root.mount_id_source.as_str());

    match root.inode_flags {
        Some(flags) if has_nocow_flag(flags) => {
            return AdmissionOutcome::rejected(AdmissionRejection::NocowInodeFlags {
                path: PathBuf::new(),
                flags,
            });
        }
        Some(_) => {}
        None => {
            return AdmissionOutcome::rejected(AdmissionRejection::InodeFlagsUnavailable {
                detail: root
                    .inode_flags_error
                    .clone()
                    .unwrap_or_else(|| "未知原因".to_owned()),
            });
        }
    }

    for option in ["nodatacow", "nodatasum"] {
        if root.mount.has_option(option) {
            return AdmissionOutcome::rejected(AdmissionRejection::NocowMountOption {
                option: option.to_owned(),
            });
        }
    }

    match &input.scan.completeness {
        ScanCompleteness::Complete => {}
        ScanCompleteness::DepthLimitReached { relative_path } => {
            return AdmissionOutcome::inconclusive(InconclusiveReason::ScanDepthLimitReached {
                relative_path: relative_path.clone(),
            });
        }
        ScanCompleteness::EntryLimitReached { relative_path } => {
            return AdmissionOutcome::inconclusive(InconclusiveReason::ScanEntryLimitReached {
                relative_path: relative_path.clone(),
            });
        }
    }
    outcome.push_note("scan_entries", input.scan.entries_scanned);

    if let Some(nested) = input.scan.subvolumes.first() {
        return AdmissionOutcome::rejected(AdmissionRejection::NestedSubvolume {
            relative_path: nested.relative_path.clone(),
            subvolume_id: nested.subvolume_id,
        });
    }

    if let Some(nocow) = input.scan.nocow_directories.first() {
        return AdmissionOutcome::rejected(AdmissionRejection::NocowInodeFlags {
            path: nocow.relative_path.clone(),
            flags: nocow.flags,
        });
    }

    if let Some(mount) = nested_mounts(input.mounts, root).into_iter().next() {
        return AdmissionOutcome::rejected(AdmissionRejection::NestedMount {
            mount_point: mount.mount_point.clone(),
            fs_type: mount.fs_type.clone(),
        });
    }
    if let Some(notes) = stacked_mount_notes(input.mounts, root) {
        for note in notes {
            outcome.notes.push(note);
        }
    }

    for internal in input.internal_state_dirs {
        if paths::relation(&root.resolved_path, &internal.resolved_path) != PathRelation::Unrelated
        {
            return AdmissionOutcome::rejected(AdmissionRejection::InternalStateOverlap {
                path: internal.given_path.clone(),
            });
        }
        let internal_backend = classify_backend(internal.fs_magic, &internal.fs_type);
        if internal_backend != BackendKind::Btrfs {
            return AdmissionOutcome::rejected(
                AdmissionRejection::InternalStateUnsupportedBackend {
                    path: internal.given_path.clone(),
                    fs_type: internal.fs_type.clone(),
                },
            );
        }
        match storage_identity(internal) {
            Some(internal_identity) if internal_identity.fsid == identity.fsid => {
                if internal_identity.subvolume_id != identity.subvolume_id {
                    outcome.push_note(
                        "internal_state_separate_subvolume",
                        internal_identity.subvolume_id,
                    );
                }
                if internal.mount.mount_id != root.mount.mount_id {
                    outcome.push_note(
                        "internal_state_separate_mount",
                        internal.given_path.display(),
                    );
                }
            }
            Some(internal_identity) => {
                return AdmissionOutcome::rejected(
                    AdmissionRejection::InternalStateOtherFilesystem {
                        path: internal.given_path.clone(),
                        internal_fsid: fsid_text(&internal_identity.fsid),
                        root_fsid: fsid_text(&identity.fsid),
                    },
                );
            }
            None => {
                return AdmissionOutcome::rejected(
                    AdmissionRejection::SubvolumeIdentityUnavailable,
                );
            }
        }
    }

    for registered in input.registered_roots {
        if same_directory(root, registered) {
            return AdmissionOutcome::rejected(AdmissionRejection::DuplicateRoot {
                path: registered.given_path.clone(),
            });
        }
        if paths::relation(&root.resolved_path, &registered.resolved_path)
            != PathRelation::Unrelated
        {
            return AdmissionOutcome::rejected(AdmissionRejection::OverlappingRoot {
                path: registered.given_path.clone(),
            });
        }
    }

    outcome
}

/// Root 内的子挂载（不含 Root 自己所在的挂载）。
#[must_use]
pub fn nested_mounts<'a>(mounts: &'a [MountEntry], root: &StorageFacts) -> Vec<&'a MountEntry> {
    mounts
        .iter()
        .filter(|entry| entry.mount_id != root.mount.mount_id)
        .filter(|entry| {
            paths::relation(&entry.mount_point, &root.resolved_path) == PathRelation::Descendant
        })
        .collect()
}

/// Root 路径上是否存在叠放的其它挂载（同一路径多个 mount id，只有最上层生效）。
fn stacked_mount_notes(mounts: &[MountEntry], root: &StorageFacts) -> Option<Vec<String>> {
    let stacked: Vec<String> = mounts
        .iter()
        .filter(|entry| entry.mount_id != root.mount.mount_id)
        .filter(|entry| entry.mount_point == root.resolved_path)
        .map(|entry| format!("stacked_mount_at_root={}", entry.mount_id))
        .collect();
    if stacked.is_empty() {
        None
    } else {
        Some(stacked)
    }
}

fn display_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        "<Root 自身>".to_owned()
    } else {
        path.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::mountinfo::DeviceId;
    use crate::mounts::probe::{
        BtrfsFacts, MountIdSource, NestedSubvolume, NocowDirectory, SubvolumeIdSource,
    };

    const FSID_A: [u8; 16] = [0x11; 16];
    const FSID_B: [u8; 16] = [0x22; 16];

    fn mount_entry(
        mount_id: u64,
        mount_point: &str,
        fs_type: &str,
        options: &[&str],
    ) -> MountEntry {
        MountEntry {
            mount_id,
            parent_id: 1,
            device: DeviceId { major: 8, minor: 1 },
            root: PathBuf::from("/"),
            mount_point: PathBuf::from(mount_point),
            mount_options: options.iter().map(|option| (*option).to_owned()).collect(),
            optional_fields: Vec::new(),
            fs_type: fs_type.to_owned(),
            source: "/dev/sdc1".to_owned(),
            super_options: vec!["rw".to_owned()],
        }
    }

    fn facts(path: &str, fsid: [u8; 16], subvolume_id: u64, inode: u64) -> StorageFacts {
        StorageFacts {
            given_path: PathBuf::from(path),
            resolved_path: PathBuf::from(path),
            fs_magic: BTRFS_SUPER_MAGIC,
            fs_type: "btrfs".to_owned(),
            statfs_fsid: [1, 2],
            mount: mount_entry(22, "/data", "btrfs", &["rw", "relatime"]),
            mount_id_source: MountIdSource::StatxMountId,
            inode,
            device: DeviceId { major: 8, minor: 1 },
            btime: Some((1, 2)),
            btrfs: Some(BtrfsFacts {
                fsid,
                subvolume_id: Some(subvolume_id),
                subvolume_id_source: Some(SubvolumeIdSource::StatxSubvol),
                subvolume_id_error: None,
            }),
            inode_flags: Some(0),
            inode_flags_error: None,
        }
    }

    fn empty_scan() -> ScanReport {
        ScanReport {
            subvolumes: Vec::new(),
            nocow_directories: Vec::new(),
            entries_scanned: 0,
            completeness: ScanCompleteness::Complete,
        }
    }

    #[test]
    fn backend_classification_covers_required_rejections() {
        assert_eq!(
            classify_backend(BTRFS_SUPER_MAGIC, "btrfs"),
            BackendKind::Btrfs
        );
        assert_eq!(
            classify_backend(FUSE_SUPER_MAGIC, "fuse.rclone"),
            BackendKind::Fuse
        );
        assert_eq!(
            classify_backend(FUSE_SUPER_MAGIC, "fuseblk"),
            BackendKind::Fuseblk
        );
        assert_eq!(classify_backend(NFS_SUPER_MAGIC, "nfs4"), BackendKind::Nfs);
        assert_eq!(
            classify_backend(SMB2_MAGIC_NUMBER, "cifs"),
            BackendKind::Cifs
        );
        assert_eq!(
            classify_backend(OVERLAYFS_SUPER_MAGIC, "overlay"),
            BackendKind::Overlay
        );
        assert_eq!(classify_backend(TMPFS_MAGIC, "tmpfs"), BackendKind::Tmpfs);
        assert_eq!(
            classify_backend(EXT4_SUPER_MAGIC, "ext4"),
            BackendKind::Ext4
        );
        assert_eq!(classify_backend(XFS_SUPER_MAGIC, "xfs"), BackendKind::Xfs);
        assert_eq!(classify_backend(ZFS_SUPER_MAGIC, "zfs"), BackendKind::Zfs);
        // 幻数未知时退回 fstype 名（新后端或容器里看到的伪装挂载）。
        assert_eq!(classify_backend(0, "fuse.sshfs"), BackendKind::Fuse);
        assert_eq!(classify_backend(0, "nfs"), BackendKind::Nfs);
        assert_eq!(classify_backend(0, "smb3"), BackendKind::Cifs);
        assert_eq!(classify_backend(0, "vfat"), BackendKind::Other);
    }

    #[test]
    fn backend_short_names_round_trip_through_parse() {
        for kind in [
            BackendKind::Btrfs,
            BackendKind::Fuse,
            BackendKind::Fuseblk,
            BackendKind::Nfs,
            BackendKind::Cifs,
            BackendKind::Overlay,
            BackendKind::Tmpfs,
            BackendKind::Ext4,
            BackendKind::Xfs,
            BackendKind::Zfs,
            BackendKind::Other,
        ] {
            assert_eq!(BackendKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(BackendKind::parse("teleporter"), None);
    }

    #[test]
    fn non_btrfs_backend_is_rejected_with_reason() {
        let root = StorageFacts {
            fs_magic: TMPFS_MAGIC,
            fs_type: "tmpfs".to_owned(),
            btrfs: None,
            inode_flags: None,
            inode_flags_error: Some("ENOTTY".to_owned()),
            ..facts("/tmp", FSID_A, 5, 1)
        };
        let scan = empty_scan();
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[],
            registered_roots: &[],
            mounts: &[],
            scan: &scan,
        });
        let AdmissionDecision::Rejected(rejection) = &outcome.decision else {
            panic!("tmpfs 必须拒绝，实际 {:?}", outcome.decision);
        };
        assert_eq!(rejection.code(), "unsupported_backend");
        assert_eq!(rejection.design_error_code(), "UNSUPPORTED_STORAGE_BACKEND");
        assert!(rejection.detail().contains("tmpfs"));
    }

    #[test]
    fn compliant_root_is_accepted_and_alias_shares_identity() {
        let root = facts("/data/vaultsyncd/root", FSID_A, 2506, 300);
        let alias = facts("/data/vaultsyncd/bind-alias", FSID_A, 2506, 300);
        let scan = empty_scan();
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[],
            registered_roots: &[],
            mounts: &[],
            scan: &scan,
        });
        assert_eq!(outcome.decision, AdmissionDecision::Accepted);
        assert!(
            same_storage(&root, &alias),
            "同一子卷的不同路径属于同一存储"
        );
        assert!(same_directory(&root, &alias));
        assert!(verify_unchanged(&root, &alias).is_ok());

        let mut moved = alias.clone();
        moved.inode = 301;
        assert_eq!(
            verify_unchanged(&root, &moved),
            Err(IdentityDrift::DirectoryChanged)
        );
    }

    #[test]
    fn duplicate_and_overlapping_roots_are_rejected() {
        let root = facts("/data/vaultsyncd/root", FSID_A, 2506, 300);
        let duplicate = facts("/data/vaultsyncd/alias", FSID_A, 2506, 300);
        let parent = facts("/data/vaultsyncd", FSID_A, 2506, 257);
        let scan = empty_scan();

        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[],
            registered_roots: &[duplicate],
            mounts: &[],
            scan: &scan,
        });
        assert_eq!(
            outcome.decision,
            AdmissionDecision::Rejected(AdmissionRejection::DuplicateRoot {
                path: PathBuf::from("/data/vaultsyncd/alias")
            })
        );

        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[],
            registered_roots: &[parent],
            mounts: &[],
            scan: &scan,
        });
        assert!(matches!(
            outcome.decision,
            AdmissionDecision::Rejected(AdmissionRejection::OverlappingRoot { .. })
        ));

        // 同子卷内不同目录、路径不重叠：允许。
        let sibling = facts("/data/vaultsyncd/other", FSID_A, 2506, 700);
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[],
            registered_roots: &[sibling],
            mounts: &[],
            scan: &scan,
        });
        assert_eq!(outcome.decision, AdmissionDecision::Accepted);
    }

    #[test]
    fn nested_subvolume_and_submount_are_rejected() {
        let root = facts("/data/vaultsyncd/root", FSID_A, 2506, 256);
        let mut scan = empty_scan();
        scan.subvolumes.push(NestedSubvolume {
            relative_path: PathBuf::from("inner"),
            subvolume_id: 2510,
        });
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[],
            registered_roots: &[],
            mounts: &[],
            scan: &scan,
        });
        assert!(matches!(
            outcome.decision,
            AdmissionDecision::Rejected(AdmissionRejection::NestedSubvolume { .. })
        ));

        let scan = empty_scan();
        let mounts = vec![
            mount_entry(22, "/data", "btrfs", &["rw"]),
            mount_entry(60, "/data/vaultsyncd/root/bind", "btrfs", &["rw"]),
        ];
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[],
            registered_roots: &[],
            mounts: &mounts,
            scan: &scan,
        });
        assert_eq!(
            outcome.decision,
            AdmissionDecision::Rejected(AdmissionRejection::NestedMount {
                mount_point: PathBuf::from("/data/vaultsyncd/root/bind"),
                fs_type: "btrfs".to_owned()
            })
        );
    }

    #[test]
    fn nocow_and_mount_options_are_rejected() {
        let scan = empty_scan();

        let mut root = facts("/data/vaultsyncd/root", FSID_A, 2506, 256);
        root.inode_flags = Some(FS_NOCOW_FL);
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[],
            registered_roots: &[],
            mounts: &[],
            scan: &scan,
        });
        assert_eq!(
            outcome.decision,
            AdmissionDecision::Rejected(AdmissionRejection::NocowInodeFlags {
                path: PathBuf::new(),
                flags: FS_NOCOW_FL
            })
        );

        let mut root = facts("/data/vaultsyncd/root", FSID_A, 2506, 256);
        root.mount.super_options.push("nodatacow".to_owned());
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[],
            registered_roots: &[],
            mounts: &[],
            scan: &scan,
        });
        assert_eq!(
            outcome.decision,
            AdmissionDecision::Rejected(AdmissionRejection::NocowMountOption {
                option: "nodatacow".to_owned()
            })
        );

        let mut root = facts("/data/vaultsyncd/root", FSID_A, 2506, 256);
        root.inode_flags = None;
        root.inode_flags_error = Some("Operation not supported (os error 95)".to_owned());
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[],
            registered_roots: &[],
            mounts: &[],
            scan: &scan,
        });
        assert!(matches!(
            outcome.decision,
            AdmissionDecision::Rejected(AdmissionRejection::InodeFlagsUnavailable { .. })
        ));

        let mut scan_with_dir = empty_scan();
        scan_with_dir.nocow_directories.push(NocowDirectory {
            relative_path: PathBuf::from("nocow"),
            flags: FS_NOCOW_FL,
        });
        let root = facts("/data/vaultsyncd/root", FSID_A, 2506, 256);
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[],
            registered_roots: &[],
            mounts: &[],
            scan: &scan_with_dir,
        });
        assert!(matches!(
            outcome.decision,
            AdmissionDecision::Rejected(AdmissionRejection::NocowInodeFlags { .. })
        ));
    }

    #[test]
    fn incomplete_scan_is_not_accepted() {
        let root = facts("/data/vaultsyncd/root", FSID_A, 2506, 256);
        let mut scan = empty_scan();
        scan.completeness = ScanCompleteness::EntryLimitReached {
            relative_path: PathBuf::from("deep"),
        };
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[],
            registered_roots: &[],
            mounts: &[],
            scan: &scan,
        });
        assert_eq!(
            outcome.decision,
            AdmissionDecision::Inconclusive(InconclusiveReason::ScanEntryLimitReached {
                relative_path: PathBuf::from("deep")
            })
        );
    }

    #[test]
    fn internal_state_directory_rules() {
        let root = facts("/data/vaultsyncd/root", FSID_A, 2506, 256);
        let scan = empty_scan();

        let inside = facts("/data/vaultsyncd/root/db", FSID_A, 2506, 400);
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[inside],
            registered_roots: &[],
            mounts: &[],
            scan: &scan,
        });
        assert_eq!(
            outcome.decision,
            AdmissionDecision::Rejected(AdmissionRejection::InternalStateOverlap {
                path: PathBuf::from("/data/vaultsyncd/root/db")
            })
        );

        let mut sibling = facts("/data/vaultsyncd/state/db", FSID_A, 2507, 256);
        sibling.mount = mount_entry(23, "/data/state", "btrfs", &["rw"]);
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[sibling],
            registered_roots: &[],
            mounts: &[],
            scan: &scan,
        });
        assert_eq!(outcome.decision, AdmissionDecision::Accepted);
        assert!(
            outcome
                .notes
                .iter()
                .any(|note| note.starts_with("internal_state_separate_subvolume=")),
            "同一文件系统的另一个子卷应记录为独立子卷，notes={:?}",
            outcome.notes
        );
        assert!(
            outcome
                .notes
                .iter()
                .any(|note| note.starts_with("internal_state_separate_mount=")),
            "不同 mount id 应记录为独立挂载，notes={:?}",
            outcome.notes
        );

        let other_fs = facts("/mnt/loop/state", FSID_B, 256, 256);
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[other_fs],
            registered_roots: &[],
            mounts: &[],
            scan: &scan,
        });
        assert!(matches!(
            outcome.decision,
            AdmissionDecision::Rejected(AdmissionRejection::InternalStateOtherFilesystem { .. })
        ));

        let tmpfs_state = StorageFacts {
            fs_magic: TMPFS_MAGIC,
            fs_type: "tmpfs".to_owned(),
            btrfs: None,
            ..facts("/run/state", FSID_A, 256, 256)
        };
        let outcome = evaluate(&AdmissionInput {
            root: &root,
            internal_state_dirs: &[tmpfs_state],
            registered_roots: &[],
            mounts: &[],
            scan: &scan,
        });
        assert!(matches!(
            outcome.decision,
            AdmissionDecision::Rejected(AdmissionRejection::InternalStateUnsupportedBackend { .. })
        ));
    }

    #[test]
    fn nested_mount_detection_uses_components_not_prefixes() {
        let root = facts("/data/vaultsyncd/root", FSID_A, 2506, 256);
        let mounts = vec![
            mount_entry(22, "/data", "btrfs", &["rw"]),
            mount_entry(61, "/data/vaultsyncd/rootx/sub", "btrfs", &["rw"]),
            mount_entry(62, "/data/vaultsyncd/root/sub", "btrfs", &["rw"]),
        ];
        let found = nested_mounts(&mounts, &root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].mount_id, 62);
    }
}
