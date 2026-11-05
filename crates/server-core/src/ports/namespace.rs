//! Root 工作目录的命名空间端口：观察路径现场、创建目录、同 Root 改名（design §9.5、§11.2 步骤 5—6）。
//!
//! 内容 put 用 [`crate::ports::target::TargetPort`] 发布文件字节；目录创建与移动不碰内容，只改命名空间，
//! 因此需要另一组动作：`mkdir` 与同一次 `rename`。端口只回答“这个相对路径现在是什么”并执行调用方
//! 给出的动作，不判断前提是否成立（那是 [`crate::commit::namespace`]）。
//!
//! 两个适配器契约与 [`TargetPort`](crate::ports::target::TargetPort) 相同：
//!
//! 1. 路径不存在只有在**父目录可访问且确认没有该名字**时才返回 [`PathState::Absent`]；祖先目录确认
//!    不存在返回 [`PathState::AncestorMissing`]，读不到（权限/挂载/IO）返回
//!    [`NamespaceError::Unreadable`]。**故障不得报告成“不存在”**，否则会被读成删除（根 `AGENTS.md`）。
//! 2. [`rename`](NamespacePort::rename) 必须是同一次改名（同一 inode），不得实现成“复制再删除”：移动
//!    只改父关系，内容与条目引用保持不变（design §9.5、I03）。跨文件系统等做不到单次 rename 的情形要
//!    如实报 [`NamespaceError::Storage`]，不许悄悄退化成搬运字节。
//! 3. 端口不覆盖目标：目标已存在时 `mkdir`/`rename` 都必须原样失败，不删除、不替换（design §10.1
//!    “目标路径已存在……不覆盖目标”）。

use std::fmt;

use crate::ports::object_store::{FileKey, StorageFailure};

/// Root 内某个相对路径现在的现场。
///
/// 命名空间操作只关心“存在什么种类”和“还是不是同一个 inode”，不读内容摘要：目录没有内容，文件的内容
/// 由 put 路径负责。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathState {
    /// 目标确实不存在（父目录可访问且确认没有该名字）。
    Absent,
    /// 目标路径的某个祖先目录确认不存在：不能当成“目标可用”。
    AncestorMissing,
    /// 目标是普通文件。
    File {
        /// 实测文件身份（`device`/`inode`/`links`；`bytes` 是被测长度）。
        identity: FileKey,
    },
    /// 目标是目录。
    Directory {
        /// 实测目录身份（目录的 `bytes` 是目录项长度，不参与判定）。
        identity: FileKey,
    },
}

impl PathState {
    /// 实测身份（不存在或祖先缺失时为 `None`）。
    #[must_use]
    pub fn identity(&self) -> Option<&FileKey> {
        match self {
            PathState::Absent | PathState::AncestorMissing => None,
            PathState::File { identity } | PathState::Directory { identity } => Some(identity),
        }
    }

    /// 是否就是这个目录。
    #[must_use]
    pub fn is_directory(&self) -> bool {
        matches!(self, PathState::Directory { .. })
    }

    /// 是否是普通文件。
    #[must_use]
    pub fn is_file(&self) -> bool {
        matches!(self, PathState::File { .. })
    }

    /// 现场是否就是这个身份（同设备同 inode）。
    #[must_use]
    pub fn same_inode_as(&self, expected: &FileKey) -> bool {
        self.identity()
            .is_some_and(|identity| identity.same_inode(expected))
    }

    /// 稳定短名（日志与恢复报告用）。
    #[must_use]
    pub fn state_name(&self) -> &'static str {
        match self {
            PathState::Absent => "absent",
            PathState::AncestorMissing => "ancestor_missing",
            PathState::File { .. } => "file",
            PathState::Directory { .. } => "directory",
        }
    }
}

impl fmt::Display for PathState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathState::Absent => f.write_str("路径不存在"),
            PathState::AncestorMissing => f.write_str("祖先目录不存在"),
            PathState::File { identity } => write!(
                f,
                "普通文件（设备 {} inode {}，{} 字节）",
                identity.device, identity.inode, identity.bytes
            ),
            PathState::Directory { identity } => write!(
                f,
                "目录（设备 {} inode {}）",
                identity.device, identity.inode
            ),
        }
    }
}

/// 命名空间端口错误。`Unreadable` 与 `Absent` 是两件不同的事：前者不能当成删除。
#[derive(Clone, Debug)]
pub enum NamespaceError {
    /// 路径或其祖先不可读（权限、挂载、IO）：不得解释为“不存在”。
    Unreadable {
        /// 原始错误文本。
        detail: String,
    },
    /// 动作失败（含分类）。
    Storage {
        /// 发生失败的阶段（稳定短名，例如 `mkdir`、`rename`）。
        stage: &'static str,
        /// 失败分类。
        failure: StorageFailure,
        /// 原始错误文本。
        detail: String,
    },
    /// 请求本身不成立，适配器未做任何 I/O。
    InvalidRequest {
        /// 原因文本。
        detail: String,
    },
}

impl NamespaceError {
    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            NamespaceError::Unreadable { .. } => "target_unreadable",
            NamespaceError::Storage { failure, .. } => failure.as_str(),
            NamespaceError::InvalidRequest { .. } => "invalid_request",
        }
    }
}

impl fmt::Display for NamespaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceError::Unreadable { detail } => write!(f, "路径不可读：{detail}"),
            NamespaceError::Storage {
                stage,
                failure,
                detail,
            } => write!(f, "命名空间动作失败（{stage}/{failure}）：{detail}"),
            NamespaceError::InvalidRequest { detail } => {
                write!(f, "命名空间请求不成立：{detail}")
            }
        }
    }
}

impl std::error::Error for NamespaceError {}

/// Root 工作目录的命名空间端口。
pub trait NamespacePort {
    /// 观察一个 Root 内相对路径现在的现场。
    ///
    /// # Errors
    ///
    /// 见 [`NamespaceError`]：不可读与请求不成立都是错误，不是“不存在”。
    fn inspect(&mut self, path: &str) -> Result<PathState, NamespaceError>;

    /// 在 `path` 创建目录并返回创建后的实测现场（必须是目录）。
    ///
    /// 父目录不存在时不得递归创建：那会把“目标祖先已删除”变成静默重建。
    ///
    /// # Errors
    ///
    /// 见 [`NamespaceError`]：存储失败（含分类）、不可读、请求不成立；目标已存在必须失败而不是覆盖。
    fn create_directory(&mut self, path: &str) -> Result<PathState, NamespaceError>;

    /// 把 `from` 原地改名到 `to`（同一次 `rename`，inode 不变），返回 `to` 的实测现场。
    ///
    /// # Errors
    ///
    /// 见 [`NamespaceError`]：存储失败（含分类）、不可读、请求不成立；`to` 已存在必须失败。
    fn rename(&mut self, from: &str, to: &str) -> Result<PathState, NamespaceError>;
}
