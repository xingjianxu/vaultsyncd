//! 在目标文件系统内建立暂存副本：优先 `FICLONE`，跨文件系统时做校验复制。
//!
//! design §11.2 步骤 6 与 §11.5 的要求：
//!
//! - 暂存文件必须创建在**目标所在的文件系统/挂载内**（跨设备 `rename` 不可用），因此暂存总是
//!   由目标目录句柄下的独占新文件承担；创建后立即核对暂存与目标的挂载 ID 相同；
//! - 优先 `FICLONE`（同卷甚至跨子卷的 reflink）；`EXDEV` 表示跨文件系统，必须在目标内做校验
//!   复制，**不得忽略**也不能当成功；文件系统明确不支持 reflink（`EOPNOTSUPP`/`ENOTTY`/`EINVAL`）
//!   时可以退回完整复制，但原因要暴露出来；
//! - `EIO`/`ENOSPC`/`EDQUOT`/权限错误不退回、不重试、不伪装成“不支持”；
//! - 复制期间核对源文件身份（前后 `statx` 对比），校验复制结果的长度与 SHA-256；
//! - 任何失败都删除未验证的暂存文件并如实报告删除结果（残留也要报告，不能静默留下垃圾）。

use crate::files::hash::{self, Sha256, Sha256Digest};
use crate::files::identity::FileIdentity;
use crate::files::raw::{self, errno};
use crate::files::restricted::{RestrictError, RestrictedDir};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

/// 复制缓冲区大小（与常见顺序写块大小一致）。
const COPY_BUFFER_BYTES: usize = 128 * 1024;

/// 暂存阶段的文件权限：先收紧到 0600，最终权限在发布前由 [`crate::files::publish`] 设置。
pub(crate) const STAGING_CREATE_MODE: u32 = 0o600;

/// 副本是怎么来的。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloneMethod {
    /// `FICLONE` 成功：共享 extent 的 CoW 克隆。
    Ficlone,
    /// 校验复制：把源字节流写入目标暂存文件并读回校验。
    VerifiedCopy,
}

impl CloneMethod {
    /// 稳定短名（输出与测试按它断言）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CloneMethod::Ficlone => "ficlone",
            CloneMethod::VerifiedCopy => "verified_copy",
        }
    }
}

impl fmt::Display for CloneMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 为什么没走 `FICLONE`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFallback {
    /// 跨文件系统（`EXDEV`）：必须复制到目标文件系统内的暂存文件。
    CrossDevice,
    /// 文件系统不支持 reflink（`EOPNOTSUPP`/`ENOTTY`/`EINVAL`）。
    ReflinkUnsupported,
}

impl CopyFallback {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CopyFallback::CrossDevice => "exdev",
            CopyFallback::ReflinkUnsupported => "unsupported",
        }
    }
}

impl fmt::Display for CopyFallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `FICLONE` 失败的分类结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReflinkVerdict {
    /// `EXDEV`：跨文件系统，必须在目标内做校验复制。
    CrossDevice,
    /// 文件系统不支持 reflink：可以退回完整复制，但要暴露原因。
    Unsupported,
    /// 真实错误：不得退回、不得当成功。
    Fatal,
}

/// 仅依据 errno 分类 `FICLONE` 失败（纯逻辑，便于单元测试）。
#[must_use]
pub fn classify_reflink_failure(errno: Option<i32>) -> ReflinkVerdict {
    match errno {
        Some(errno::EXDEV) => ReflinkVerdict::CrossDevice,
        Some(errno::EOPNOTSUPP | errno::ENOTTY | errno::EINVAL) => ReflinkVerdict::Unsupported,
        _ => ReflinkVerdict::Fatal,
    }
}

/// 暂存失败的具体原因。
#[derive(Debug)]
pub enum StageFailure {
    /// 创建独占暂存文件失败（目标目录不可写、名称冲突等）。
    Create(RestrictError),
    /// 暂存文件与目标目录不在同一挂载内（不应发生；发生就说明边界假设不成立）。
    WrongFilesystem {
        /// 暂存文件的挂载 ID。
        staging_mount: Option<u64>,
        /// 目标目录的挂载 ID。
        target_mount: Option<u64>,
    },
    /// `FICLONE` 返回了不能退回的真实错误。
    ReflinkFailed(io::Error),
    /// 源文件在暂存期间被改写（身份/指纹变化）。
    SourceChanged {
        /// 变化的字段名。
        changed: Vec<&'static str>,
        /// 变化前身份。
        before: Box<FileIdentity>,
        /// 变化后身份。
        after: Box<FileIdentity>,
    },
    /// 读元数据、定位、读写等 I/O 失败。
    Io {
        /// 失败阶段短名。
        stage: &'static str,
        /// 原始错误。
        source: io::Error,
    },
    /// 校验复制结果不一致（长度或 SHA-256 不同）。
    Verify {
        /// 人类可读的不一致描述。
        detail: String,
    },
}

impl StageFailure {
    /// 稳定原因码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            StageFailure::Create(_) => "staging_create_failed",
            StageFailure::WrongFilesystem { .. } => "staging_wrong_filesystem",
            StageFailure::ReflinkFailed(_) => "reflink_failed",
            StageFailure::SourceChanged { .. } => "source_changed",
            StageFailure::Io { .. } => "staging_io_failed",
            StageFailure::Verify { .. } => "staging_verify_failed",
        }
    }
}

impl fmt::Display for StageFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageFailure::Create(error) => write!(f, "创建暂存文件失败：{error}"),
            StageFailure::WrongFilesystem {
                staging_mount,
                target_mount,
            } => write!(
                f,
                "暂存文件不在目标挂载内（staging_mount={staging_mount:?}, target_mount={target_mount:?}）"
            ),
            StageFailure::ReflinkFailed(source) => write!(f, "FICLONE 失败且不可退回：{source}"),
            StageFailure::SourceChanged { changed, .. } => {
                write!(f, "源文件在暂存期间发生变化（{}）", changed.join(","))
            }
            StageFailure::Io { stage, source } => write!(f, "{stage} 阶段 I/O 失败：{source}"),
            StageFailure::Verify { detail } => write!(f, "暂存副本校验失败：{detail}"),
        }
    }
}

/// 暂存错误：原因 + 暂存文件名 + 是否已删除。
#[derive(Debug)]
pub struct StageError {
    /// 失败原因。
    pub reason: StageFailure,
    /// 暂存文件名（相对目标目录）。
    pub staging_name: String,
    /// 是否已删除未验证的暂存文件。
    pub staging_removed: bool,
    /// 删除失败时的原始错误（保留现场，不假装已清理）。
    pub cleanup_error: Option<String>,
}

impl StageError {
    /// 稳定原因码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.reason.code()
    }

    /// 是否只是暂存名冲突（换名重试即可，不代表数据有问题）。
    #[must_use]
    pub fn is_name_collision(&self) -> bool {
        matches!(
            self.reason,
            StageFailure::Create(RestrictError::AlreadyExists { .. })
        )
    }
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}（暂存 {}，已删除={}）",
            self.reason, self.staging_name, self.staging_removed
        )?;
        if let Some(error) = &self.cleanup_error {
            write!(f, "；删除失败：{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for StageError {}

/// 暂存请求。
pub struct StageRequest<'a> {
    /// 已打开的源文件句柄（只读即可）。
    pub source: &'a File,
    /// 打开源文件时观察到的身份。
    pub source_identity: &'a FileIdentity,
    /// 目标目录句柄（暂存文件建在它下面，保证同一挂载）。
    pub target_dir: &'a RestrictedDir,
    /// 暂存文件名（相对目标目录，调用方保证独占且合规）。
    pub staging_name: &'a str,
}

/// 暂存结果。
pub struct StagedFile {
    /// 已打开且内容已验证、已 `fsync` 的暂存文件（读写句柄）。
    pub file: File,
    /// 暂存文件名。
    pub staging_name: String,
    /// 副本来源方式。
    pub method: CloneMethod,
    /// 没走 `FICLONE` 的原因。
    pub fallback: Option<CopyFallback>,
    /// `FICLONE` 的原始错误文本（发生退避时保留，便于诊断）。
    pub reflink_error: Option<String>,
    /// 暂存内容的 SHA-256。
    pub sha256: Sha256Digest,
    /// 暂存内容的字节数。
    pub bytes: u64,
    /// 暂存建立前的源身份。
    pub source_identity_before: FileIdentity,
    /// 暂存完成后的源身份。
    pub source_identity_after: FileIdentity,
    /// 源文件所在挂载 ID。
    pub source_mount_id: Option<u64>,
    /// 目标目录的挂载 ID。
    pub target_mount_id: Option<u64>,
    /// 暂存文件自己的挂载 ID（必须与目标一致）。
    pub staging_mount_id: Option<u64>,
}

/// 暂存内容（不含句柄；句柄由 [`stage_copy`] 负责与文件一起返回）。
struct StagedContent {
    method: CloneMethod,
    fallback: Option<CopyFallback>,
    reflink_error: Option<String>,
    sha256: Sha256Digest,
    bytes: u64,
    source_identity_after: FileIdentity,
    source_mount_id: Option<u64>,
    target_mount_id: Option<u64>,
    staging_mount_id: Option<u64>,
}

/// 把源文件内容暂存到目标目录下的独占新文件，并完成校验与刷盘。
///
/// # Errors
///
/// 见 [`StageError`]；失败时未验证的暂存文件会被删除（删除本身失败也如实报告）。
pub fn stage_copy(request: StageRequest<'_>) -> Result<StagedFile, StageError> {
    let StageRequest {
        source,
        source_identity,
        target_dir,
        staging_name,
    } = request;
    let name = Path::new(staging_name);
    let file = target_dir
        .create_exclusive(name, STAGING_CREATE_MODE)
        .map_err(|error| StageError {
            reason: StageFailure::Create(error),
            staging_name: staging_name.to_owned(),
            staging_removed: true,
            cleanup_error: None,
        })?;

    match fill_staging(&file, source, source_identity, target_dir) {
        Ok(content) => Ok(StagedFile {
            file,
            staging_name: staging_name.to_owned(),
            method: content.method,
            fallback: content.fallback,
            reflink_error: content.reflink_error,
            sha256: content.sha256,
            bytes: content.bytes,
            source_identity_before: source_identity.clone(),
            source_identity_after: content.source_identity_after,
            source_mount_id: content.source_mount_id,
            target_mount_id: content.target_mount_id,
            staging_mount_id: content.staging_mount_id,
        }),
        Err(failure) => {
            let cleanup_error = target_dir.unlink(name).err().map(|error| error.to_string());
            Err(StageError {
                reason: failure,
                staging_name: staging_name.to_owned(),
                staging_removed: cleanup_error.is_none(),
                cleanup_error,
            })
        }
    }
}

fn fill_staging(
    file: &File,
    source: &File,
    source_identity: &FileIdentity,
    target_dir: &RestrictedDir,
) -> Result<StagedContent, StageFailure> {
    let staging_identity =
        FileIdentity::of_fd(file.as_raw_fd()).map_err(|source| StageFailure::Io {
            stage: "stat_staging",
            source,
        })?;
    let target_mount_id = target_dir.mount_id().map_err(|error| StageFailure::Io {
        stage: "stat_target_dir",
        source: io::Error::other(error.to_string()),
    })?;
    if staging_identity.mount_id != target_mount_id {
        return Err(StageFailure::WrongFilesystem {
            staging_mount: staging_identity.mount_id,
            target_mount: target_mount_id,
        });
    }

    let (method, fallback, reflink_error, sha256, bytes) =
        match raw::ficlone(file.as_raw_fd(), source.as_raw_fd()) {
            Ok(()) => {
                raw::fsync(file.as_raw_fd()).map_err(|source| StageFailure::Io {
                    stage: "fsync_staging",
                    source,
                })?;
                let (digest, bytes) = hash_file(file).map_err(|source| StageFailure::Io {
                    stage: "hash_staging",
                    source,
                })?;
                (CloneMethod::Ficlone, None, None, digest, bytes)
            }
            Err(error) => {
                let fallback = match classify_reflink_failure(error.raw_os_error()) {
                    ReflinkVerdict::CrossDevice => CopyFallback::CrossDevice,
                    ReflinkVerdict::Unsupported => CopyFallback::ReflinkUnsupported,
                    ReflinkVerdict::Fatal => return Err(StageFailure::ReflinkFailed(error)),
                };
                let (digest, bytes) = verified_copy(file, source)?;
                (
                    CloneMethod::VerifiedCopy,
                    Some(fallback),
                    Some(error.to_string()),
                    digest,
                    bytes,
                )
            }
        };

    let source_after =
        FileIdentity::of_fd(source.as_raw_fd()).map_err(|source| StageFailure::Io {
            stage: "stat_source_after",
            source,
        })?;
    let changed = source_identity.changed_fields(&source_after);
    if !changed.is_empty() {
        return Err(StageFailure::SourceChanged {
            changed,
            before: Box::new(source_identity.clone()),
            after: Box::new(source_after),
        });
    }
    if bytes != source_identity.size {
        return Err(StageFailure::Verify {
            detail: format!(
                "暂存字节数 {bytes} 与源文件大小 {} 不一致",
                source_identity.size
            ),
        });
    }

    Ok(StagedContent {
        method,
        fallback,
        reflink_error,
        sha256,
        bytes,
        source_identity_after: source_after,
        source_mount_id: source_identity.mount_id,
        target_mount_id,
        staging_mount_id: staging_identity.mount_id,
    })
}

/// 校验复制：把源字节流写入暂存文件，`fsync` 后读回，比对长度与 SHA-256。
fn verified_copy(target: &File, source: &File) -> Result<(Sha256Digest, u64), StageFailure> {
    let mut reader = source;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|source| StageFailure::Io {
            stage: "seek_source",
            source,
        })?;
    let mut writer = target;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    let mut written = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|source| StageFailure::Io {
                stage: "read_source",
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        writer
            .write_all(&buffer[..read])
            .map_err(|source| StageFailure::Io {
                stage: "write_staging",
                source,
            })?;
        written += read as u64;
    }
    writer.flush().map_err(|source| StageFailure::Io {
        stage: "flush_staging",
        source,
    })?;
    raw::fsync(target.as_raw_fd()).map_err(|source| StageFailure::Io {
        stage: "fsync_staging",
        source,
    })?;
    let expected = hasher.finish();
    let (readback, readback_bytes) = hash_file(target).map_err(|source| StageFailure::Io {
        stage: "hash_staging",
        source,
    })?;
    if readback_bytes != written {
        return Err(StageFailure::Verify {
            detail: format!("写入 {written} 字节，读回 {readback_bytes} 字节"),
        });
    }
    if readback != expected {
        return Err(StageFailure::Verify {
            detail: format!(
                "写入流 SHA-256 {} 与读回 {} 不一致",
                hash::hex(&expected),
                hash::hex(&readback)
            ),
        });
    }
    Ok((expected, written))
}

/// 从文件开头读到末尾并算 SHA-256，返回摘要与字节数（暂存校验与原型命令共用）。
///
/// # Errors
///
/// 定位或读取失败时返回原始 `io::Error`。
pub fn hash_file(file: &File) -> io::Result<(Sha256Digest, u64)> {
    let mut reader = file;
    reader.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((hasher.finish(), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflink_failures_are_classified_by_errno() {
        assert_eq!(
            classify_reflink_failure(Some(errno::EXDEV)),
            ReflinkVerdict::CrossDevice
        );
        for errno in [errno::EOPNOTSUPP, errno::ENOTTY, errno::EINVAL] {
            assert_eq!(
                classify_reflink_failure(Some(errno)),
                ReflinkVerdict::Unsupported,
                "errno={errno}"
            );
        }
        // EIO/ENOSPC/EDQUOT/权限错误必须致命：不退回、不重试、不当成功。
        for errno in [5, 28, 122, 13, 1, 40] {
            assert_eq!(
                classify_reflink_failure(Some(errno)),
                ReflinkVerdict::Fatal,
                "errno={errno}"
            );
        }
        assert_eq!(classify_reflink_failure(None), ReflinkVerdict::Fatal);
    }

    #[test]
    fn method_and_fallback_names_are_stable() {
        assert_eq!(CloneMethod::Ficlone.as_str(), "ficlone");
        assert_eq!(CloneMethod::VerifiedCopy.as_str(), "verified_copy");
        assert_eq!(CopyFallback::CrossDevice.as_str(), "exdev");
        assert_eq!(CopyFallback::ReflinkUnsupported.as_str(), "unsupported");
        assert_eq!(
            StageFailure::Verify {
                detail: "x".to_owned()
            }
            .code(),
            "staging_verify_failed"
        );
    }
}
