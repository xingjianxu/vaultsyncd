//! Root 工作目录的目标端口：观察目标现场、按意图发布工作文件（design §11.2 步骤 4—6、§11.3）。
//!
//! 端口只回答“目标现在是什么”和“把这份内容发布上去之后目标是什么”，不做判定：现场是否等于意图
//! 前提、发布结果是否等于意图目标，都由核心比较。适配器必须报告实测事实（身份、SHA-256、字节数），
//! 读不到目标时返回错误，**不得**把权限/挂载/IO 故障报告成“目标不存在”——那会把故障读成删除
//! （根 `AGENTS.md`、design §11.5）。

use std::fmt;

use crate::objects::ContentDigest;
use crate::ports::object_store::{FileKey, StorageFailure};

/// 目标路径现在的现场。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetObservation {
    /// 目标不存在（父目录可访问且确认没有该名字）。
    Absent,
    /// 目标是普通文件，附带实测身份与内容摘要。
    Present {
        /// 实测文件身份（含可见字节数）。
        identity: FileKey,
        /// 实测内容的 SHA-256。
        digest: ContentDigest,
    },
}

impl TargetObservation {
    /// 实测字节数（不存在时为 `None`）。
    #[must_use]
    pub fn size(&self) -> Option<u64> {
        match self {
            TargetObservation::Absent => None,
            TargetObservation::Present { identity, .. } => Some(identity.bytes),
        }
    }

    /// 实测内容摘要（不存在时为 `None`）。
    #[must_use]
    pub fn digest(&self) -> Option<ContentDigest> {
        match self {
            TargetObservation::Absent => None,
            TargetObservation::Present { digest, .. } => Some(*digest),
        }
    }

    /// 稳定短名（日志与恢复报告用）。
    #[must_use]
    pub fn state_name(&self) -> &'static str {
        match self {
            TargetObservation::Absent => "absent",
            TargetObservation::Present { .. } => "present",
        }
    }
}

impl fmt::Display for TargetObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetObservation::Absent => f.write_str("目标不存在"),
            TargetObservation::Present { identity, digest } => write!(
                f,
                "目标存在（设备 {} inode {} 链接 {} {} 字节，SHA-256 {digest}）",
                identity.device, identity.inode, identity.links, identity.bytes
            ),
        }
    }
}

/// 一次发布请求：把内容地址上的对象发布到 Root 内的目标路径。
#[derive(Clone, Debug)]
pub struct PublishRequest {
    /// 目标在 Root 内的相对路径。
    pub target_path: String,
    /// 目标所在子卷内的暂存文件名（单个文件名，由意图记录）。
    pub staging_path: String,
    /// 要发布的对象内容地址。
    pub digest: ContentDigest,
    /// 对象字节数（发布后必须与实测长度一致）。
    pub size: u64,
}

/// 发布完成后的目标现场。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedTarget {
    /// 实测文件身份（含可见字节数）。
    pub identity: FileKey,
    /// 实测内容的 SHA-256。
    pub digest: ContentDigest,
}

/// 目标端口错误。`Unreadable` 与 `Absent` 是两件不同的事：前者不能当成删除。
#[derive(Clone, Debug)]
pub enum TargetError {
    /// 目标或其父目录不可读（权限、挂载、IO）：不得解释为“目标不存在”。
    Unreadable {
        /// 原始错误文本。
        detail: String,
    },
    /// 发布动作失败（含分类）。
    Storage {
        /// 发生失败的阶段（稳定短名，例如 `stage_publish`、`rename`）。
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

impl TargetError {
    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            TargetError::Unreadable { .. } => "target_unreadable",
            TargetError::Storage { failure, .. } => failure.as_str(),
            TargetError::InvalidRequest { .. } => "invalid_request",
        }
    }
}

impl fmt::Display for TargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetError::Unreadable { detail } => write!(f, "目标不可读：{detail}"),
            TargetError::Storage {
                stage,
                failure,
                detail,
            } => write!(f, "目标发布失败（{stage}/{failure}）：{detail}"),
            TargetError::InvalidRequest { detail } => write!(f, "发布请求不成立：{detail}"),
        }
    }
}

impl std::error::Error for TargetError {}

/// Root 工作目录的目标端口。
///
/// 实现方（Linux 适配器）必须满足的契约：
///
/// 1. [`observe`](TargetPort::observe) 返回实测身份与 SHA-256；目标确实不存在才返回
///    [`TargetObservation::Absent`]，读不到（权限/挂载/IO）一律 [`TargetError::Unreadable`]。
/// 2. [`publish`](TargetPort::publish) 先把对象复制/克隆成目标所在子卷内的暂存文件并刷盘，再做平台
///    受控替换；跨文件系统时不要依赖一次 `rename`。替换后必须报告实测身份与 SHA-256。发布失败时
///    必须清理自己的暂存文件，并把错误分类上报（`ENOSPC` 等不得伪装成功或“不支持”）。
/// 3. 端口**不判断**现场是否匹配意图前提/目标：那是核心的比较（design §11.2 步骤 4—7）。
pub trait TargetPort {
    /// 观察目标路径现在的现场。
    ///
    /// # Errors
    ///
    /// 见 [`TargetError`]：不可读与请求不成立都是错误，不是“不存在”。
    fn observe(&mut self, target_path: &str) -> Result<TargetObservation, TargetError>;

    /// 把内容对象发布到目标路径并报告发布后的实测现场。
    ///
    /// # Errors
    ///
    /// 见 [`TargetError`]：存储失败（含分类）、不可读、请求不成立。
    fn publish(&mut self, request: &PublishRequest) -> Result<PublishedTarget, TargetError>;
}
