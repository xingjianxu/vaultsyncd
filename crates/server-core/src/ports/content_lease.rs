//! 内容租约端口：在“读到某个不可变版本”与“它的内容被新引用固定下来”之间保护该对象
//! （design §7.2 `object_leases`、§8.4、§10.3、§11“短期内容租约或等价机制”、§19.4）。
//!
//! 为什么需要它：历史版本与回收站内容的内容对象只在保留期内由版本行/回收站清单引用，GC 按引用标记
//! 回收（design §19.4）。恢复一次旧版本是“先按旧版本核对内容 → 再把它发布成新的当前版本”，两步之间
//! 没有任何新的持久引用；若不加保护，GC 可能正好在这一刻把对象判为无引用。租约把“这期间不许回收”
//! 变成**持久事实**：租约行在过期前被 GC 视作引用，进程崩溃也不会失去保护（比内存里的引用计数可靠）。
//!
//! 端口只回答“租约有没有建立/解除”，不做判定；租约只能保护**已登记**的内容对象，登记前写租约没有
//! 意义（`object_leases` 的外键就是这条契约）。

use std::fmt;

use vaultsyncd_domain::ids::{ObjectLeaseId, VaultId};
use vaultsyncd_domain::version::UtcTimestamp;

use crate::objects::ContentDigest;

/// 租约用途，与 `object_leases.purpose` 的取值一一对应。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentLeasePurpose {
    /// 固定版本下载/恢复/释放检查期间保护其内容（design §8.4）。
    Download,
    /// 对象已落盘、即将被新版本引用的发布窗口（design §11.2 步骤 3—7）。
    Commit,
}

impl ContentLeasePurpose {
    /// 数据库里的规范文本。
    #[must_use]
    pub const fn as_text(self) -> &'static str {
        match self {
            ContentLeasePurpose::Download => "download",
            ContentLeasePurpose::Commit => "commit",
        }
    }

    /// 解析 `object_leases.purpose`；未知文本返回 `None`，由调用方按坏行处理。
    #[must_use]
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "download" => Some(ContentLeasePurpose::Download),
            "commit" => Some(ContentLeasePurpose::Commit),
            _ => None,
        }
    }
}

/// 一次租约请求。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentLeaseRequest {
    /// 对象所属 Vault：对象按 `(vault_id, hash)` 登记，内容地址不跨 Vault 共享。
    pub vault: VaultId,
    /// 被保护的内容地址。
    pub digest: ContentDigest,
    /// 用途。
    pub purpose: ContentLeasePurpose,
    /// 租约到期时间：过了这一刻 GC 可以按引用标记回收该对象。
    pub expires_at: UtcTimestamp,
}

/// 已建立的租约。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentLease {
    /// 租约身份（`object_leases.id`），解除时按它定位。
    pub id: ObjectLeaseId,
    /// 用途，与请求一致（日志与诊断用）。
    pub purpose: ContentLeasePurpose,
    /// 到期时间：解除失败时也要如实报告，不能假装对象已被释放保护。
    pub expires_at: UtcTimestamp,
}

/// 租约端口的失败。
#[derive(Clone, Debug)]
pub enum ContentLeaseError {
    /// 存储层失败（SQLite 等），保留原始错误文本。
    Storage {
        /// 原始错误文本。
        detail: String,
    },
    /// Vault 不在调用方的 owner scope 内。
    OutOfScope,
    /// 内容地址上还没有登记的对象：没有可保护的内容。
    ObjectNotRegistered {
        /// 被请求保护的内容地址。
        digest: ContentDigest,
    },
    /// 写入被存储层约束拒绝（到期时间等）：核心应在写库前就拦住。
    Rejected {
        /// 稳定短名，例如 `invalid_lease_expiry`。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
}

impl ContentLeaseError {
    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ContentLeaseError::Storage { .. } => "content_lease_failed",
            ContentLeaseError::OutOfScope => "out_of_scope",
            ContentLeaseError::ObjectNotRegistered { .. } => "object_not_registered",
            ContentLeaseError::Rejected { code, .. } => code,
        }
    }
}

impl fmt::Display for ContentLeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContentLeaseError::Storage { detail } => write!(f, "内容租约存储失败：{detail}"),
            ContentLeaseError::OutOfScope => f.write_str("Vault 不在调用方的 owner scope 内"),
            ContentLeaseError::ObjectNotRegistered { digest } => {
                write!(f, "内容 {digest} 还没有登记对象，无法建立租约")
            }
            ContentLeaseError::Rejected { code, detail } => {
                write!(f, "内容租约被拒绝（{code}）：{detail}")
            }
        }
    }
}

impl std::error::Error for ContentLeaseError {}

/// 内容租约端口。
///
/// 实现方必须满足的契约：
///
/// 1. [`acquire`](ContentLeasePort::acquire) 只为**已登记**对象建租约；对象不存在时返回
///    [`ContentLeaseError::ObjectNotRegistered`]，不要凭空造一条保护；
/// 2. 到期时间按调用方给的值落库（UTC 秒）；过期租约由 GC 一侧的扫描清理，不在读取时“顺手删掉”；
/// 3. [`release`](ContentLeasePort::release) 只解除给定身份的租约；已过期或已解除时报错并保留原错文本，
///    由调用方如实记录（租约到期后 GC 本来就会回收，不能因此把已提交的业务结果改判为失败）。
#[allow(async_fn_in_trait)]
pub trait ContentLeasePort {
    /// 建立一条内容租约。
    ///
    /// # Errors
    ///
    /// 见 [`ContentLeaseError`]：存储失败、越权、对象未登记、写入被拒绝。
    async fn acquire(
        &mut self,
        request: &ContentLeaseRequest,
    ) -> Result<ContentLease, ContentLeaseError>;

    /// 解除一条内容租约。
    ///
    /// # Errors
    ///
    /// 见 [`ContentLeaseError`]：存储失败、越权、租约已不存在或已过期。
    async fn release(&mut self, lease: &ContentLease) -> Result<(), ContentLeaseError>;
}
