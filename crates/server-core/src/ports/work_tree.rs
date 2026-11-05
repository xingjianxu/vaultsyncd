//! Root 工作目录的只读观察端口：枚举与指纹（design §12.1、§12.2、§11.5）。
//!
//! 外部采集只需要两个问题的答案：“这个目录下现在有哪些项”和“这个路径现在是什么、指纹是什么”。
//! 适配器必须报告实测事实；读不到（权限、挂载漂移、IO）一律是错误，**不得**报告成“目录为空”或
//! “文件不存在”——那会把故障读成删除（根 `AGENTS.md`、design §11.5）。端口不做稳定性判定，
//! 也不读取内容：内容固定由 [`crate::ports::object_store::ObjectBlobPort`] 的私有副本完成。

use std::fmt;

use crate::ports::object_store::FileKey;

/// 普通文件在某一时刻的指纹。
///
/// 稳定性判断**不以 mtime 为准**：`mtime` 可以被外部程序保留或回拨，所以指纹同时包含 inode、创建
/// 时间、大小、`ctime` 与链接数。`ctime` 无法由用户态设置，保留 mtime 的改写仍会推进它。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    /// 设备、inode、链接数与可见字节数。
    pub key: FileKey,
    /// 创建时间（秒、纳秒）；文件系统不提供时为 `None`。
    pub btime: Option<(i64, u32)>,
    /// 内容修改时间（秒、纳秒）：只作显示与线索，不用来决定谁获胜。
    pub mtime: (i64, u32),
    /// 元数据变更时间（秒、纳秒）。
    pub ctime: (i64, u32),
}

impl FileStamp {
    /// 是否同一个文件对象（设备、inode 与创建时间都相同）。
    #[must_use]
    pub fn same_object(&self, other: &Self) -> bool {
        self.key.same_inode(&other.key) && self.btime == other.btime
    }

    /// 列出与 `other` 不同的字段名（顺序固定）。空表示指纹完全相同。
    #[must_use]
    pub fn changed_fields(&self, other: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.key.device != other.key.device {
            changed.push("device");
        }
        if self.key.inode != other.key.inode {
            changed.push("inode");
        }
        if self.btime != other.btime {
            changed.push("btime");
        }
        if self.key.bytes != other.key.bytes {
            changed.push("size");
        }
        if self.mtime != other.mtime {
            changed.push("mtime");
        }
        if self.ctime != other.ctime {
            changed.push("ctime");
        }
        if self.key.links != other.key.links {
            changed.push("nlink");
        }
        changed
    }

    /// 指纹完全相同。
    #[must_use]
    pub fn fingerprint_equal(&self, other: &Self) -> bool {
        self.changed_fields(other).is_empty()
    }
}

/// 目录项的类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkEntryKind {
    /// 普通文件。
    File,
    /// 目录。
    Directory,
    /// 符号链接（不跟随，只上报）。
    Symlink,
    /// 设备、FIFO、套接字等。
    Other,
    /// 文件系统没给出类型（`DT_UNKNOWN`），适配器也没能分类：按不支持处理，不猜成文件。
    Unknown,
}

impl WorkEntryKind {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            WorkEntryKind::File => "file",
            WorkEntryKind::Directory => "directory",
            WorkEntryKind::Symlink => "symlink",
            WorkEntryKind::Other => "other",
            WorkEntryKind::Unknown => "unknown",
        }
    }
}

impl fmt::Display for WorkEntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 目录里的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkDirEntry {
    /// 名称（单个路径分量）。
    pub name: String,
    /// 类型。
    pub kind: WorkEntryKind,
}

/// 一个目录下被适配器无法表示成文本的名字（非 UTF-8）的数量。
///
/// 这些名字不能进入同步范围，但也不能静默消失：核心把它们记为“跳过”，使 Root 的初始采集不会被
/// 报告成已完整保护。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkDirListing {
    /// 可表示名字的目录项，适配器已按字节序排序。
    pub entries: Vec<WorkDirEntry>,
    /// 无法表示的名字（非 UTF-8）的数量。
    pub unrepresentable_names: usize,
}

/// 路径现在的现场。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkObservation {
    /// 路径不存在（父目录可访问且确认没有该名字）。
    Absent,
    /// 普通文件，附带实测指纹。
    File(FileStamp),
    /// 存在但不是普通文件（目录、符号链接等）。
    NotRegular(WorkEntryKind),
}

/// 观察错误：与 [`WorkObservation::Absent`] 是两件不同的事，**不得**当成删除。
#[derive(Clone, Debug)]
pub enum WorkTreeError {
    /// 目录或文件不可读（权限、IO、被并发替换成不同类型）。
    Unreadable {
        /// 相对路径。
        path: String,
        /// 原始错误文本。
        detail: String,
    },
    /// 目录或文件已确认不存在（父目录可访问且没有该名字）：枚举与检查之间被并发删除/改名。
    Gone {
        /// 相对路径。
        path: String,
    },
    /// Root 本身不可用（挂载漂移、根句柄身份变化、跨越挂载边界）：整个 Root 暂停，不推导删除。
    RootUnavailable {
        /// 原因文本。
        detail: String,
    },
    /// 请求路径不成立（绝对路径、`..` 等），适配器未做任何 I/O。
    InvalidPath {
        /// 原因文本。
        detail: String,
    },
}

impl WorkTreeError {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            WorkTreeError::Unreadable { .. } => "work_tree_unreadable",
            WorkTreeError::Gone { .. } => "work_tree_gone",
            WorkTreeError::RootUnavailable { .. } => "root_unavailable",
            WorkTreeError::InvalidPath { .. } => "invalid_path",
        }
    }
}

impl fmt::Display for WorkTreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkTreeError::Unreadable { path, detail } => {
                write!(f, "工作目录路径 {path} 不可读：{detail}")
            }
            WorkTreeError::Gone { path } => write!(f, "工作目录路径 {path} 已不存在"),
            WorkTreeError::RootUnavailable { detail } => write!(f, "Root 不可用：{detail}"),
            WorkTreeError::InvalidPath { detail } => write!(f, "路径不成立：{detail}"),
        }
    }
}

impl std::error::Error for WorkTreeError {}

/// Root 工作目录的只读观察端口。
///
/// 实现方（Linux 适配器）必须满足的契约：
///
/// 1. 所有路径都是 Root 内的相对路径（空串表示 Root 自身），经受限句柄解析：不跟随符号链接，
///    不穿过挂载边界，不逃出 Root；
/// 2. [`list_dir`](WorkTreePort::list_dir) 返回按名称字节序排序的列表；读不全就返回错误，
///    不返回截断的列表；
/// 3. [`observe`](WorkTreePort::observe) 只有确认父目录可访问且没有该名字时才返回
///    [`WorkObservation::Absent`]；[`list_dir`](WorkTreePort::list_dir) 同理只有确认目录不存在才返回
///    [`WorkTreeError::Gone`]；
/// 4. 每次调用前核对 Root 身份，漂移时返回 [`WorkTreeError::RootUnavailable`]。
pub trait WorkTreePort {
    /// 列出目录下的项。
    ///
    /// # Errors
    ///
    /// 见 [`WorkTreeError`]：目录不可读、Root 不可用、路径不成立。
    fn list_dir(&mut self, dir: &str) -> Result<WorkDirListing, WorkTreeError>;

    /// 观察路径现在的现场与指纹。
    ///
    /// # Errors
    ///
    /// 见 [`WorkTreeError`]：不可读、Root 不可用、路径不成立。
    fn observe(&mut self, path: &str) -> Result<WorkObservation, WorkTreeError>;
}
