//! Root 工作目录的移除端口：把条目（及其整棵子树）从工作目录删掉（design §10.3、§11.2 步骤 6）。
//!
//! 删除是唯一会毁掉工作目录内容的命名空间动作，因此端口契约比 `mkdir`/`rename` 更严：
//!
//! 1. **有界批次**：目录删除按 [`RemovalPort::remove_directory_batch`] 分批推进，每次调用最多处理
//!    `max_entries` 个目录项并报告是否做完。适配器不得为了图省事一次递归删完整棵树：大树删除必须
//!    可中断、可继续（design §10.3“大目录删除使用可恢复任务”）。
//! 2. **失败不是不存在**：删不掉（权限、忙、挂载故障、IO 错误）必须返回 [`RemovalError`]，不能报告
//!    成功让调用方以为“所有设备已删除”（design §10.3 第三条）。剩余项留在工作目录里，由核心按
//!    `prepared` 意图重试。
//! 3. **不猜**：端口不判断“该不该删”（那是 [`crate::commit::delete`]），只做调用方点名的路径。
//!
//! 观察现场用 [`NamespacePort`](crate::ports::namespace::NamespacePort)（同一个工作目录适配器同时实现
//! 两者），删除前后都要实测：只改数据库却声称删掉文件是 design §10.3 明确禁止的。

use std::fmt;

use crate::ports::object_store::StorageFailure;

/// 一批目录删除的进度。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemovalProgress {
    /// 本次调用删掉的目录项数量（含子目录自身，不含目录自身之外的东西）。
    pub removed: u64,
    /// 整棵树是否已经删完（`false` 表示还要继续调用同一路径）。
    pub complete: bool,
}

impl RemovalProgress {
    /// 已完成的一批。
    #[must_use]
    pub const fn finished(removed: u64) -> Self {
        RemovalProgress {
            removed,
            complete: true,
        }
    }

    /// 还没做完的一批。
    #[must_use]
    pub const fn partial(removed: u64) -> Self {
        RemovalProgress {
            removed,
            complete: false,
        }
    }
}

/// 移除端口的失败。
#[derive(Clone, Debug)]
pub enum RemovalError {
    /// 路径或其祖先不可读（权限、挂载、IO）：不得解释为“已经删掉”。
    Unreadable {
        /// 原始错误文本。
        detail: String,
    },
    /// 路径仍在被使用（外部句柄、只读挂载、目录非空且不是本任务加的项）：等占用结束后重试。
    InUse {
        /// 被占用的路径。
        path: String,
        /// 原始错误文本。
        detail: String,
    },
    /// 删除动作失败（含 `ENOSPC` 之类不适用、但 `EACCES`/`EROFS`/`EIO` 适用）的分类。
    Storage {
        /// 发生失败的阶段（稳定短名，例如 `unlink`、`rmdir`）。
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

impl RemovalError {
    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            RemovalError::Unreadable { .. } => "target_unreadable",
            RemovalError::InUse { .. } => "path_in_use",
            RemovalError::Storage { failure, .. } => failure.as_str(),
            RemovalError::InvalidRequest { .. } => "invalid_request",
        }
    }
}

impl fmt::Display for RemovalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemovalError::Unreadable { detail } => write!(f, "路径不可读：{detail}"),
            RemovalError::InUse { path, detail } => {
                write!(f, "路径 {path} 仍被占用：{detail}")
            }
            RemovalError::Storage {
                stage,
                failure,
                detail,
            } => write!(f, "移除动作失败（{stage}/{failure}）：{detail}"),
            RemovalError::InvalidRequest { detail } => write!(f, "移除请求不成立：{detail}"),
        }
    }
}

impl std::error::Error for RemovalError {}

/// Root 工作目录的移除端口。
pub trait RemovalPort {
    /// 删除一个普通文件。
    ///
    /// # Errors
    ///
    /// 见 [`RemovalError`]：不可读、被占用、存储失败都不得报成成功。
    fn remove_file(&mut self, path: &str) -> Result<(), RemovalError>;

    /// 删除一棵目录树的一批：本次最多处理 `max_entries` 个目录项，返回进度。
    ///
    /// 路径已经不存在时报告 [`RemovalProgress::finished`]（0 项）而不是失败：删除任务可被打断后重复
    /// 调用，第二次调用看到的“已经删完”与“这次删完了”对调用方是同一件事。
    ///
    /// # Errors
    ///
    /// 见 [`RemovalError`]：不可读、被占用、存储失败。失败时已经删掉的部分不回滚（删除不可逆），
    /// 由调用方按持久意图在下次重试。
    fn remove_directory_batch(
        &mut self,
        path: &str,
        max_entries: usize,
    ) -> Result<RemovalProgress, RemovalError>;
}
