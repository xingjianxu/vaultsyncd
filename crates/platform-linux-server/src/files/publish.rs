//! 受控替换：把暂存文件发布成工作文件（或把工作文件发布成内容对象）。
//!
//! 流程对应 design §11.2 步骤 4—6：
//!
//! 1. 从目标目录句柄读当前身份 → 核对替换前提（`ExpectedTarget`）。不满足就**不做任何修改**。
//! 2. 在目标目录内暂存（[`crate::files::clone::stage_copy`]）：`FICLONE` 或校验复制，`fsync` 文件。
//! 3. 替换前**再核对一次**目标身份：如果外部程序在这期间改写了目标，暂停并保留候选，不硬覆盖。
//! 4. 设置最终权限，`renameat2`（新建用 `RENAME_NOREPLACE`，替换用 0 旗标做原子替换），再
//!    `fsync` 目录，最后核对目标就是刚发布的那份内容。
//!
//! 边界（design §12.2 / C33）：核对与 `rename` 之间仍有一个不由本服务控制的窗口，外部程序可以
//! 在这期间写入。本模块不宣称消除了该竞态；它只在“已经观察到不一致”时停下来报告，并把内核能
//! 兜住的部分（`RENAME_NOREPLACE`）交给内核。

use crate::files::clone::{
    CloneMethod, CopyFallback, StageError, StageRequest, StagedFile, stage_copy,
};
use crate::files::hash::{self, Sha256Digest};
use crate::files::identity::{
    ExpectedTarget, FileIdentity, PreconditionOutcome, PreconditionRefusal, check_target,
};
use crate::files::raw;
use crate::files::restricted::{ResolveMechanism, RestrictError, RestrictedDir};
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 内容对象的权限：对象只由服务管理，外部写入者不可写（design C37）。
pub const OBJECT_FILE_MODE: u32 = 0o444;
/// 发布到用户工作目录时的默认权限（仍受部署的权限策略约束）。
pub const WORK_FILE_MODE: u32 = 0o644;

/// 暂存文件名前缀：便于运维识别残留，也避免与用户文件名冲突。
pub const STAGING_PREFIX: &str = ".vaultsyncd-stage.";

/// 生成一个独占的暂存文件名（相对目标目录，单段）。
///
/// 组成：固定前缀 + pid + 纳秒时间戳 + 计数器；单段长度远小于 `NAME_MAX`，且满足
/// [`crate::files::restricted::validate_relative_path`] 的规则（没有分隔符、`.`、`..`）。
#[must_use]
pub fn staging_name(pid: u32, nanos: u128, counter: u32) -> String {
    format!("{STAGING_PREFIX}{pid}.{nanos}.{counter}")
}

fn next_staging_name(attempt: u32) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    staging_name(std::process::id(), nanos, attempt)
}

/// 保留下来供人工处理的候选副本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedCandidate {
    /// 暂存文件所在目录（绝对路径，仅供报告）。
    pub directory: PathBuf,
    /// 暂存文件名。
    pub name: String,
    /// 候选内容的 SHA-256。
    pub sha256: Sha256Digest,
}

impl RetainedCandidate {
    /// 候选文件的完整路径。
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.directory.join(&self.name)
    }
}

/// 发布失败原因。
#[derive(Debug)]
pub enum PublishFailure {
    /// 读取目标当前身份失败。
    TargetObservation(RestrictError),
    /// 暂存失败（含 `FICLONE` 的真实错误与复制校验失败）。
    Staging(Box<StageError>),
    /// 替换前提不再成立：暂停并保留候选，不硬覆盖。
    PreconditionRefused(Box<PreconditionRefusal>),
    /// 发布前的准备步骤失败（设置最终权限、读取暂存文件身份）。
    Prepare {
        /// 失败阶段短名。
        stage: &'static str,
        /// 原始错误。
        source: io::Error,
    },
    /// `renameat2` 失败（含跨文件系统、权限等）。
    Rename(RestrictError),
    /// 重命名成功但目录刷盘失败（重启后可能丢失该重命名）。
    DirectoryFsync(RestrictError),
    /// 发布后核对目标不一致。
    TargetVerify(String),
}

impl PublishFailure {
    /// 稳定原因码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            PublishFailure::TargetObservation(error) => error.code(),
            PublishFailure::Staging(error) => error.code(),
            PublishFailure::PreconditionRefused(refusal) => refusal.code,
            PublishFailure::Prepare { .. } => "prepare_failed",
            PublishFailure::Rename(error) => error.code(),
            PublishFailure::DirectoryFsync(_) => "directory_fsync_failed",
            PublishFailure::TargetVerify(_) => "target_verify_failed",
        }
    }

    /// 是否属于“暂停并报告”的拒绝（不是 I/O 故障）。
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        matches!(self, PublishFailure::PreconditionRefused(_))
    }
}

impl fmt::Display for PublishFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishFailure::TargetObservation(error) => write!(f, "读取目标身份失败：{error}"),
            PublishFailure::Staging(error) => write!(f, "{error}"),
            PublishFailure::PreconditionRefused(refusal) => {
                write!(
                    f,
                    "替换前提不成立（{}）：{}",
                    refusal.code, refusal.observed
                )
            }
            PublishFailure::Prepare { stage, source } => {
                write!(f, "{stage} 阶段失败：{source}")
            }
            PublishFailure::Rename(error) => write!(f, "{error}"),
            PublishFailure::DirectoryFsync(error) => write!(f, "目录刷盘失败：{error}"),
            PublishFailure::TargetVerify(detail) => write!(f, "发布后核对失败：{detail}"),
        }
    }
}

/// 发布错误：原因 + 拒绝细节 + 保留的候选副本。
#[derive(Debug)]
pub struct PublishError {
    /// 失败原因。
    pub reason: PublishFailure,
    /// 拒绝细节（`reason` 为 [`PublishFailure::PreconditionRefused`] 时必有）。
    pub refusal: Option<Box<PreconditionRefusal>>,
    /// 保留下来、可继续使用的候选副本（没有保留时为 `None`）。
    pub candidate: Option<Box<RetainedCandidate>>,
}

impl PublishError {
    fn new(reason: PublishFailure) -> Self {
        Self {
            reason,
            refusal: None,
            candidate: None,
        }
    }

    fn refused(refusal: PreconditionRefusal) -> Self {
        Self {
            reason: PublishFailure::PreconditionRefused(Box::new(refusal.clone())),
            refusal: Some(Box::new(refusal)),
            candidate: None,
        }
    }

    fn with_candidate(reason: PublishFailure, candidate: RetainedCandidate) -> Self {
        Self {
            reason,
            refusal: None,
            candidate: Some(Box::new(candidate)),
        }
    }

    fn refused_with_candidate(reason: PreconditionRefusal, candidate: RetainedCandidate) -> Self {
        Self {
            reason: PublishFailure::PreconditionRefused(Box::new(reason.clone())),
            refusal: Some(Box::new(reason)),
            candidate: Some(Box::new(candidate)),
        }
    }

    /// 稳定原因码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.reason.code()
    }

    /// 是否属于“暂停并报告”的拒绝。
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        self.reason.is_refusal()
    }
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)?;
        if let Some(candidate) = &self.candidate {
            write!(f, "；候选保留在 {}", candidate.path().display())?;
        }
        Ok(())
    }
}

impl std::error::Error for PublishError {}

/// 发布请求。
pub struct PublishRequest<'a> {
    /// 已打开的源文件句柄（对象或工作文件，只读即可）。
    pub source: &'a File,
    /// 打开源文件时观察到的身份。
    pub source_identity: &'a FileIdentity,
    /// 目标目录句柄。
    pub target_dir: &'a RestrictedDir,
    /// 目标名称（相对目标目录）。
    pub target_name: &'a Path,
    /// 替换前提。
    pub expected: &'a ExpectedTarget,
    /// 发布后的文件权限。
    pub mode: u32,
}

/// 发布结果（成功时目标已是新内容，且目录已刷盘）。
pub struct PublishOutcome {
    /// 本次使用的暂存文件名（成功后它已经成为目标）。
    pub staging_name: String,
    /// 副本来源方式。
    pub method: CloneMethod,
    /// 没走 `FICLONE` 的原因。
    pub fallback: Option<CopyFallback>,
    /// `FICLONE` 的原始错误文本。
    pub reflink_error: Option<String>,
    /// 已发布内容的 SHA-256。
    pub sha256: Sha256Digest,
    /// 已发布内容的字节数。
    pub bytes: u64,
    /// 发布后的权限位。
    pub mode: u32,
    /// 源文件身份。
    pub source_identity: FileIdentity,
    /// 源文件所在挂载 ID。
    pub source_mount_id: Option<u64>,
    /// 目标目录挂载 ID。
    pub target_mount_id: Option<u64>,
    /// 暂存文件挂载 ID（必须与目标目录一致）。
    pub staging_mount_id: Option<u64>,
    /// 替换前的目标身份（不存在时为 `None`）。
    pub target_before: Option<FileIdentity>,
    /// 替换后的目标身份。
    pub target_after: FileIdentity,
    /// 是否替换了已存在的目标（`false` 表示新建）。
    pub replaced: bool,
    /// 目录是否已刷盘。
    pub directory_fsynced: bool,
    /// 本次使用的解析机制。
    pub resolve: ResolveMechanism,
}

/// 暂存完成、等待发布的候选（对应 design §11.2 步骤 5—6：意图已写、等待替换的状态）。
///
/// 拆成 [`stage_for_publish`] + [`commit_staged`] 两步是为了让调用方在两者之间持久化
/// `fs_intent`（design §11.2 步骤 5），也让验收脚本能在两步之间注入一次**真实**的外部写入，
/// 从而验证“替换前复核失败就暂停”。
pub struct StagedPublish<'a> {
    /// 目标目录句柄（必须与暂存时同一个）。
    target_dir: &'a RestrictedDir,
    /// 暂存前读到的目标身份。
    target_before: Option<FileIdentity>,
    /// 本次是新建（`RENAME_NOREPLACE`）还是替换。
    create: bool,
    /// 源文件身份（写进结果）。
    source_identity: FileIdentity,
    /// 暂存结果。
    staged: StagedFile,
}

/// 第二步：替换前的最终核对、设置权限、`renameat2`、目录刷盘与发布后核对。
///
/// # Errors
///
/// 见 [`PublishError`]。前提在暂存期间被打破时返回“拒绝”并保留候选。
pub fn commit_staged<'a>(
    candidate: StagedPublish<'a>,
    target_name: &Path,
    mode: u32,
) -> Result<PublishOutcome, PublishError> {
    let StagedPublish {
        target_dir,
        target_before,
        create,
        source_identity,
        staged,
    } = candidate;
    let retained = RetainedCandidate {
        directory: target_dir.path().to_path_buf(),
        name: staged.staging_name.clone(),
        sha256: staged.sha256,
    };

    // 替换前的最后一次核对：外部程序在这期间改写了目标就停下来。
    let target_now = target_dir.identity_at(target_name).map_err(|error| {
        PublishError::with_candidate(PublishFailure::TargetObservation(error), retained.clone())
    })?;
    let expected = match &target_before {
        Some(identity) => ExpectedTarget::Present(identity.clone()),
        None => ExpectedTarget::Absent,
    };
    match check_target(&expected, target_now.as_ref()) {
        PreconditionOutcome::Create | PreconditionOutcome::Replace => {}
        PreconditionOutcome::Refused(refusal) => {
            return Err(PublishError::refused_with_candidate(*refusal, retained));
        }
    }

    if let Err(source) = raw::fchmod(staged.file.as_raw_fd(), mode) {
        return Err(PublishError::with_candidate(
            PublishFailure::Prepare {
                stage: "fchmod_staging",
                source,
            },
            retained,
        ));
    }
    let candidate_identity = FileIdentity::of_fd(staged.file.as_raw_fd()).map_err(|source| {
        PublishError::with_candidate(
            PublishFailure::Prepare {
                stage: "stat_staging",
                source,
            },
            retained.clone(),
        )
    })?;
    let staging_rel = Path::new(&staged.staging_name).to_path_buf();
    match target_dir.rename(&staging_rel, target_dir, target_name, create) {
        Ok(()) => {}
        Err(RestrictError::AlreadyExists { .. }) if create => {
            // 内核用 RENAME_NOREPLACE 兜住了“检查后、重命名前”出现的竞争目标。
            let observed = target_dir.identity_at(target_name).ok().flatten();
            let refusal = PreconditionRefusal {
                code: "target_appeared",
                expected: ExpectedTarget::Absent.as_str().to_owned(),
                observed: observed
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), ToString::to_string),
                expected_identity: None,
                observed_identity: observed,
                changed_fields: Vec::new(),
            };
            return Err(PublishError::refused_with_candidate(refusal, retained));
        }
        Err(error) => {
            return Err(PublishError::with_candidate(
                PublishFailure::Rename(error),
                retained,
            ));
        }
    }

    target_dir
        .fsync_dir()
        .map_err(|error| PublishError::new(PublishFailure::DirectoryFsync(error)))?;

    let target_after = target_dir
        .identity_at(target_name)
        .map_err(|error| PublishError::new(PublishFailure::TargetObservation(error)))?
        .ok_or_else(|| {
            PublishError::new(PublishFailure::TargetVerify(
                "重命名后目标路径不存在".to_owned(),
            ))
        })?;
    let mut problems: Vec<String> = Vec::new();
    if !candidate_identity.same_object(&target_after) {
        problems.push(format!(
            "目标身份与刚发布的暂存文件不同（{} vs {}）",
            target_after, candidate_identity
        ));
    }
    if target_after.size != staged.bytes {
        problems.push(format!(
            "目标大小 {} 与发布字节数 {} 不一致",
            target_after.size, staged.bytes
        ));
    }
    if target_after.permission_bits() != mode & 0o7777 {
        problems.push(format!(
            "目标权限 {:04o} 与期望 {:04o} 不一致",
            target_after.permission_bits(),
            mode & 0o7777
        ));
    }
    if !problems.is_empty() {
        return Err(PublishError::new(PublishFailure::TargetVerify(
            problems.join("；"),
        )));
    }

    Ok(PublishOutcome {
        staging_name: staged.staging_name,
        method: staged.method,
        fallback: staged.fallback,
        reflink_error: staged.reflink_error,
        sha256: staged.sha256,
        bytes: staged.bytes,
        mode: mode & 0o7777,
        source_identity,
        source_mount_id: staged.source_mount_id,
        target_mount_id: staged.target_mount_id,
        staging_mount_id: staged.staging_mount_id,
        target_before,
        replaced: !create,
        target_after,
        directory_fsynced: true,
        resolve: target_dir.mechanism(),
    })
}

/// 一次完整的受控替换（[`stage_for_publish`] + [`commit_staged`]）。
///
/// # Errors
///
/// 见 [`PublishError`]。前提不成立时返回“拒绝”，此时目标未被修改；已建立的候选副本随错误一起
/// 报告。
pub fn publish(request: PublishRequest<'_>) -> Result<PublishOutcome, PublishError> {
    let candidate = stage_for_publish(
        request.source,
        request.source_identity,
        request.target_dir,
        request.target_name,
        request.expected,
    )?;
    commit_staged(candidate, request.target_name, request.mode)
}

/// 第一步：核对替换前提 → 在目标目录内暂存并校验内容。
///
/// 与 [`commit_staged`] 一起构成两步发布；产品侧在两步之间持久化 `fs_intent`。
///
/// # Errors
///
/// 见 [`PublishError`]：读取目标身份失败、前提不成立（未创建暂存文件）或暂存/校验失败。
pub fn stage_for_publish<'a>(
    source: &File,
    source_identity: &FileIdentity,
    target_dir: &'a RestrictedDir,
    target_name: &Path,
    expected: &ExpectedTarget,
) -> Result<StagedPublish<'a>, PublishError> {
    let target_before = target_dir
        .identity_at(target_name)
        .map_err(|error| PublishError::new(PublishFailure::TargetObservation(error)))?;
    let create = match check_target(expected, target_before.as_ref()) {
        PreconditionOutcome::Create => true,
        PreconditionOutcome::Replace => false,
        PreconditionOutcome::Refused(refusal) => {
            return Err(PublishError::refused(*refusal));
        }
    };

    let mut attempt = 0u32;
    let staged = loop {
        let name = next_staging_name(attempt);
        match stage_copy(StageRequest {
            source,
            source_identity,
            target_dir,
            staging_name: &name,
        }) {
            Ok(value) => break value,
            // 暂存名冲突（同纳秒内并发发布）时换一个名字重试，其它错误原样上报。
            Err(error) if error.is_name_collision() && attempt < 3 => attempt += 1,
            Err(error) => {
                return Err(PublishError::new(PublishFailure::Staging(Box::new(error))));
            }
        }
    };

    Ok(StagedPublish {
        target_dir,
        target_before,
        create,
        source_identity: source_identity.clone(),
        staged,
    })
}

/// 一次“采集”的便捷封装：把工作文件复制成只读内容对象（目标必须不存在）。
///
/// # Errors
///
/// 见 [`PublishError`]；对象已存在时返回拒绝（`target_appeared`），不覆盖既有对象。
pub fn capture_object(
    source: &File,
    source_identity: &FileIdentity,
    object_dir: &RestrictedDir,
    object_name: &Path,
) -> Result<PublishOutcome, PublishError> {
    publish(PublishRequest {
        source,
        source_identity,
        target_dir: object_dir,
        target_name: object_name,
        expected: &ExpectedTarget::Absent,
        mode: OBJECT_FILE_MODE,
    })
}

/// 把发布结果渲染成稳定的 `key=value` 行（原型命令与测试共用，避免两处漂移）。
#[must_use]
pub fn render_outcome(outcome: &PublishOutcome) -> String {
    let mut lines = vec![
        "decision=published".to_owned(),
        format!("method={}", outcome.method),
        format!(
            "fallback={}",
            outcome
                .fallback
                .map_or("none".to_owned(), |fallback| fallback.to_string())
        ),
        format!(
            "reflink_error={}",
            outcome.reflink_error.as_deref().unwrap_or("none")
        ),
        format!("sha256={}", hash::hex(&outcome.sha256)),
        format!("bytes={}", outcome.bytes),
        format!("mode={:04o}", outcome.mode),
        format!("resolve={}", outcome.resolve),
        format!("source_mount_id={}", format_mount(outcome.source_mount_id)),
        format!("target_mount_id={}", format_mount(outcome.target_mount_id)),
        format!(
            "staging_mount_id={}",
            format_mount(outcome.staging_mount_id)
        ),
        format!("source_inode={}", outcome.source_identity.inode),
        format!(
            "target_inode_before={}",
            outcome
                .target_before
                .as_ref()
                .map_or_else(|| "none".to_owned(), |identity| identity.inode.to_string())
        ),
        format!("target_inode_after={}", outcome.target_after.inode),
        format!("replaced={}", outcome.replaced),
        format!("directory_fsynced={}", outcome.directory_fsynced),
        format!("staging_name={}", outcome.staging_name),
    ];
    lines.push(String::new());
    lines.join("\n")
}

fn format_mount(mount_id: Option<u64>) -> String {
    mount_id.map_or_else(|| "none".to_owned(), |id| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::restricted::validate_relative_path;

    #[test]
    fn staging_names_are_valid_unique_single_components() {
        let first = staging_name(4242, 1_793_881_188_822_826_519, 0);
        let second = staging_name(4242, 1_793_881_188_822_826_519, 1);
        assert_ne!(first, second);
        for name in [&first, &second] {
            assert!(name.starts_with(STAGING_PREFIX));
            assert!(
                validate_relative_path(Path::new(name)).is_ok(),
                "name={name}"
            );
            assert!(name.len() <= 255);
            assert!(!name.contains('/'));
        }
    }

    #[test]
    fn object_mode_is_read_only() {
        assert_eq!(OBJECT_FILE_MODE & 0o222, 0, "对象不得对任何人可写");
        assert_eq!(WORK_FILE_MODE & 0o600, 0o600);
    }
}
