//! 上传 finalize 与固定版本内容下载（design §13.2、§15.2、I01、I05、I12、I15）。
//!
//! 两个端点的共同点是“服务端只对**完整的、具名的**字节负责”：
//!
//! # finalize
//!
//! 顺序固定为：读会话 → 核对块齐 → 标记 `complete` → 按序拼装并发布对象 → 核对总长度与总哈希 →
//! 登记内容对象 → 标记 `finalized`。
//!
//! - **块不齐不 finalize**：缺块时直接失败，不改会话状态（`INVALID_REQUEST`：请求的前提还不成立）。
//! - **先校验后发布**：拼装出的总长度必须等于会话总大小、总 SHA-256 必须等于会话声明的哈希；任一
//!   不符就丢弃暂存对象并放弃会话（design §13.2「校验失败：不安装、不提交」，I05）。
//! - **finalize 不提交条目**：它只产出可提交内容引用（内容地址 + 长度 + 对象区路径），条目的 head
//!   由后续 put/create 操作按 CAS 提交（design §15.2「不修改条目 head」，I01：finalize 成功不等于
//!   同步完成）。
//! - **重放幂等**：已 `finalized` 的会话再次 finalize 返回同一份内容引用，不重新发布、不改状态
//!   （响应丢失后的重试必须得到同一结果）。
//!
//! # 下载
//!
//! 顺序固定为：解析 `(root, entry, version)` → 解析并收敛 Range → **建立下载租约** → 从对象区按
//! 切片读取 → 结束时解除租约。
//!
//! - **按版本取字节**：内容地址只来自该版本行；路径上后来出现的新 head 与本请求无关（I12）。版本
//!   过期被清理 → [`ContentFailure::VersionGone`]（410 `VERSION_GONE`），绝不回退成当前内容。
//! - **下载租约**：历史版本只在保留期内被引用，GC 按引用标记回收。租约在“开始读”之前建立，读取
//!   结束才解除，因此读取期间这个对象一定还在（design §7.2 `object_leases`、§11、§19.4）。
//! - **Range 只做切片**：`bytes=start-end`、`bytes=start-`、`bytes=-suffix` 三种形式；越界或语法不
//!   合法都是请求错误，不用空内容或整文件代替。
//! - **整文件校验**：请求覆盖整个对象时，读出的字节必须与内容地址的 SHA-256 一致才返回；部分切片
//!   无法在服务端核对整份摘要，由客户端按 design §13.2 的“校验各块及整文件”负责。

use vaultsyncd_domain::ids::{EntryId, RootId, UploadId, VersionId};
use vaultsyncd_domain::version::UtcTimestamp;
use vaultsyncd_transfer::hash;

use crate::api::uploads::{StoredPart, UploadFailure, UploadPhase, UploadSession};
use crate::objects::{ContentDigest, ObjectLayout, ReuseOutcome};
use crate::ports::content_lease::{
    ContentLease, ContentLeaseError, ContentLeasePort, ContentLeasePurpose, ContentLeaseRequest,
};
use crate::ports::content_publish::{ContentObjectStore, ContentPublisher, PublishFailure};
use crate::ports::object_store::{BlobError, ObjectBlobPort, StorageFailure};
use crate::ports::upload_session::UploadSessionStore;
use crate::ports::version_content::{VersionContentError, VersionContentStore};

/// 下载租约的默认时长（秒）。
///
/// 只覆盖一次下载请求的读取窗口；租约到期后 GC 可以回收该对象，客户端要么已经拿到字节，要么按
/// `VERSION_GONE` 重新查询（design §15.4）。
pub const DOWNLOAD_LEASE_TTL_SECONDS: i64 = 900;

/// 上传会话中块数与块行的对照结果（finalize 的输入前提）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssembledParts {
    /// 块行，按索引升序。
    pub parts: Vec<StoredPart>,
    /// 会话的块数。
    pub part_count: u64,
}

impl AssembledParts {
    /// 块行是否覆盖了会话的全部块，且每块长度与几何一致。
    ///
    /// # Errors
    ///
    /// 块行数量不足、索引不连续或长度不符时返回 [`ContentFailure::IncompleteUpload`]。
    pub fn verify(&self, session: &UploadSession) -> Result<(), ContentFailure> {
        let geometry = session.geometry().map_err(ContentFailure::Upload)?;
        let expected = geometry.part_count();
        if self.part_count != expected || self.parts.len() as u64 != expected {
            return Err(ContentFailure::IncompleteUpload {
                expected,
                present: self.parts.len() as u64,
            });
        }
        for (position, part) in self.parts.iter().enumerate() {
            let index = position as u64;
            if part.index != index {
                return Err(ContentFailure::IncompleteUpload {
                    expected,
                    present: self.parts.len() as u64,
                });
            }
            let expected_length = geometry.part_length(index).unwrap_or(0);
            if part.length != expected_length {
                return Err(ContentFailure::IncompleteUpload {
                    expected,
                    present: self.parts.len() as u64,
                });
            }
        }
        Ok(())
    }
}

/// finalize 前读到的会话与块行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizeFacts {
    /// 上传会话。
    pub session: UploadSession,
    /// 已登记块，按索引升序。
    pub parts: Vec<StoredPart>,
}

/// 可提交内容引用：`op_id` 提交时按它引用已发布对象（design §15.2 finalize「只生成可提交内容引用」）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentReference {
    /// 内容地址。
    pub digest: ContentDigest,
    /// 字节数。
    pub size: u64,
    /// 对象区内的规范相对路径（写进 `objects.storage_path` 的同一个值）。
    pub storage_path: String,
}

/// 一次已发布的对象（内容引用 + 本次是新建还是复用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedContent {
    /// 内容地址。
    pub digest: ContentDigest,
    /// 字节数。
    pub size: u64,
    /// 对象区内的规范相对路径。
    pub storage_path: String,
    /// 本次新建还是复用既有对象。
    pub reuse: ReuseOutcome,
}

impl PublishedContent {
    /// 转成可提交内容引用（丢弃发布方式这类诊断字段）。
    #[must_use]
    pub fn reference(&self) -> ContentReference {
        ContentReference {
            digest: self.digest,
            size: self.size,
            storage_path: self.storage_path.clone(),
        }
    }
}

/// finalize 的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizedContent {
    /// 会话身份。
    pub upload: UploadId,
    /// 所属 Root。
    pub root: RootId,
    /// 可提交内容引用。
    pub content: ContentReference,
    /// 块数。
    pub part_count: u64,
    /// 发布方式（首次新建/复用）。
    pub reuse: ReuseOutcome,
    /// 是否为幂等重放（会话本来就已经 `finalized`）。
    pub replayed: bool,
}

/// finalize 与下载失败的分类。
#[derive(Clone, Debug)]
pub enum ContentFailure {
    /// 会话不存在/越权/状态不允许等上传层失败。
    Upload(UploadFailure),
    /// 块没有收齐（含索引不连续、块行长度与几何不符）：前提不成立，不改会话状态。
    IncompleteUpload {
        /// 会话块数。
        expected: u64,
        /// 实际块行数。
        present: u64,
    },
    /// 拼装出的内容与快照声明不符：不发布为可提交引用，会话被放弃（I05）。
    ContentMismatch {
        /// 声明的内容地址。
        expected: ContentDigest,
        /// 拼装出的内容地址。
        actual: ContentDigest,
        /// 声明的字节数。
        expected_size: u64,
        /// 拼装出的字节数。
        actual_size: u64,
    },
    /// 拼装/发布/登记失败。
    Publish(PublishFailure),
    /// 版本已过期或被清理（design §15.4 `VERSION_GONE`）。
    VersionGone,
    /// 条目不存在、不在该 Root 内、或是目录。
    EntryUnavailable,
    /// 下载的切片落在对象范围之外（含空区间）。
    RangeNotSatisfiable {
        /// 对象字节数。
        size: u64,
        /// 请求区间的文本（原样回显，便于诊断）。
        requested: String,
    },
    /// Range 语法不合法。
    InvalidRange {
        /// 请求头的原始文本。
        requested: String,
        /// 原因。
        detail: String,
    },
    /// 读取对象字节失败。
    Read {
        /// 失败细节。
        detail: String,
        /// 是否空间不足（读取不会满盘，保留字段以便统一映射）。
        no_space: bool,
    },
    /// 内容租约失败（无法在读取期间保护对象）。
    Lease(ContentLeaseError),
    /// 存储层失败。
    Storage {
        /// 原始错误文本。
        detail: String,
    },
}

impl From<UploadFailure> for ContentFailure {
    fn from(failure: UploadFailure) -> Self {
        Self::Upload(failure)
    }
}

impl From<PublishFailure> for ContentFailure {
    fn from(failure: PublishFailure) -> Self {
        Self::Publish(failure)
    }
}

impl From<BlobError> for ContentFailure {
    fn from(error: BlobError) -> Self {
        let no_space = matches!(
            &error,
            BlobError::Storage {
                failure: StorageFailure::NoSpace | StorageFailure::QuotaExceeded,
                ..
            }
        );
        Self::Read {
            detail: error.to_string(),
            no_space,
        }
    }
}

impl From<VersionContentError> for ContentFailure {
    fn from(error: VersionContentError) -> Self {
        match error {
            VersionContentError::VersionGone => Self::VersionGone,
            VersionContentError::EntryNotFound | VersionContentError::NotAFile => {
                Self::EntryUnavailable
            }
            other => Self::Storage {
                detail: other.to_string(),
            },
        }
    }
}

impl std::fmt::Display for ContentFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Upload(failure) => write!(f, "{failure}"),
            Self::IncompleteUpload { expected, present } => {
                write!(f, "上传尚未收齐块：应有 {expected} 块，实际 {present} 块")
            }
            Self::ContentMismatch {
                expected,
                actual,
                expected_size,
                actual_size,
            } => write!(
                f,
                "拼装内容与快照不符：期望 {expected}/{expected_size} 字节，实测 {actual}/{actual_size} 字节"
            ),
            Self::Publish(failure) => write!(f, "{failure}"),
            Self::VersionGone => f.write_str("版本已过期或被清理"),
            Self::EntryUnavailable => f.write_str("条目不存在、不是文件或版本不属于它"),
            Self::RangeNotSatisfiable { size, requested } => {
                write!(f, "区间 `{requested}` 超出对象范围（{size} 字节）")
            }
            Self::InvalidRange { requested, detail } => {
                write!(f, "区间 `{requested}` 不合法：{detail}")
            }
            Self::Read { detail, .. } => write!(f, "读取对象字节失败：{detail}"),
            Self::Lease(error) => write!(f, "内容租约失败：{error}"),
            Self::Storage { detail } => write!(f, "版本内容存储失败：{detail}"),
        }
    }
}

impl std::error::Error for ContentFailure {}

/// finalize：拼装、校验完整内容并发布对象，返回可提交内容引用。
///
/// 顺序见模块文档。已 `finalized` 的会话是幂等重放：直接返回同一份引用，不再发布。
///
/// # Errors
///
/// 见 [`ContentFailure`]：会话不可用（`Upload`）、块未收齐（`IncompleteUpload`）、拼装内容与快照
/// 不符（`ContentMismatch`，会话被放弃）、发布/登记失败（`Publish`）。
pub async fn finalize_upload<S, P, O>(
    store: &mut S,
    publisher: &mut P,
    objects: &mut O,
    upload: UploadId,
    staging_name: &str,
    now: UtcTimestamp,
) -> Result<FinalizedContent, ContentFailure>
where
    S: UploadSessionStore,
    P: ContentPublisher,
    O: ContentObjectStore,
{
    let session = store.load(upload).await?;
    // 过期 finalize 不能重放一个已失去租约、可能被 GC 回收的内容引用。
    if session.expires_at <= now {
        return Err(ContentFailure::Upload(UploadFailure::UnknownUpload));
    }
    match session.phase {
        UploadPhase::Finalized => {
            return Ok(FinalizedContent {
                upload,
                root: session.root,
                content: ContentReference {
                    digest: session.hash,
                    size: session.total_size,
                    storage_path: publisher.storage_path(&session.hash).display().to_string(),
                },
                part_count: session.geometry().map(|g| g.part_count()).unwrap_or(0),
                reuse: ReuseOutcome::ExistingVerified,
                replayed: true,
            });
        }
        UploadPhase::Aborted => {
            return Err(ContentFailure::Upload(UploadFailure::PartsClosed {
                phase: session.phase,
            }));
        }
        UploadPhase::Open | UploadPhase::Complete => {}
    }

    let parts = store.parts(upload).await?;
    let assembled = AssembledParts {
        part_count: session
            .geometry()
            .map(|g| g.part_count())
            .unwrap_or(u64::MAX),
        parts,
    };
    assembled.verify(&session)?;
    if session.phase == UploadPhase::Open {
        // 先持久化“块齐”，再发布：崩溃后会话停在 complete，重试会重新拼装并发布（幂等）。
        store.mark_complete(upload).await?;
    }

    let published = match publisher.publish_parts(upload, staging_name, &assembled.parts) {
        Ok(published) => published,
        Err(failure) => return Err(ContentFailure::Publish(failure)),
    };
    if published.digest != session.hash || published.size != session.total_size {
        // 校验失败：不提交、不引用；放弃会话，让客户端重新上传（design §13.2）。
        let abort = store.abort(upload).await;
        if let Err(failure) = abort {
            return Err(ContentFailure::Upload(failure));
        }
        return Err(ContentFailure::ContentMismatch {
            expected: session.hash,
            actual: published.digest,
            expected_size: session.total_size,
            actual_size: published.size,
        });
    }

    objects
        .record_published(session.root, &published, now)
        .await?;
    store.mark_finalized(upload).await?;
    Ok(FinalizedContent {
        upload,
        root: session.root,
        content: published.reference(),
        part_count: assembled.part_count,
        reuse: published.reuse,
        replayed: false,
    })
}

/// 一个具名条目版本的内容事实（下载的唯一依据）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionContent {
    /// 所属 Root。
    pub root: RootId,
    /// 条目身份。
    pub entry: EntryId,
    /// 版本身份。
    pub version: VersionId,
    /// 内容地址。
    pub digest: ContentDigest,
    /// 字节数。
    pub size: u64,
}

/// 要读取的字节区间。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteSlice {
    /// 起始偏移（含）。
    pub offset: u64,
    /// 字节数（大于 0）。
    pub length: u64,
}

impl ByteSlice {
    /// 是否覆盖整个对象。
    #[must_use]
    pub fn covers_whole(&self, size: u64) -> bool {
        self.offset == 0 && self.length == size
    }

    /// `Content-Range` 的区间文本（`bytes start-end/total`）。
    #[must_use]
    pub fn content_range(&self, size: u64) -> String {
        let last = self.offset + self.length.saturating_sub(1);
        format!("bytes {}-{last}/{size}", self.offset)
    }
}

/// 解析 `Range` 头并收敛成一个可读区间。
///
/// 支持 `bytes=start-end`、`bytes=start-`、`bytes=-suffix` 三种形式；多区间（逗号）不支持——首版
/// 下载按单区间返回，遇到多区间报请求错误而不是悄悄只给第一段。
///
/// # Errors
///
/// 不是 `bytes=` 形式、数值非规范十进制、区间反向/null、起点超出对象长度时返回
/// [`ContentFailure::InvalidRange`] 或 [`ContentFailure::RangeNotSatisfiable`]；空对象上任何区间都
/// 不可满足。
pub fn parse_range(header: &str, size: u64) -> Result<ByteSlice, ContentFailure> {
    let requested = header.trim();
    let invalid = |detail: &str| ContentFailure::InvalidRange {
        requested: requested.to_owned(),
        detail: detail.to_owned(),
    };
    let spec = requested
        .strip_prefix("bytes=")
        .ok_or_else(|| invalid("只支持 `bytes=` 形式"))?
        .trim();
    if spec.is_empty() {
        return Err(invalid("没有区间"));
    }
    if spec.contains(',') {
        return Err(invalid("不支持多区间"));
    }
    let (first, last) = spec
        .split_once('-')
        .ok_or_else(|| invalid("区间缺少 `-`"))?;
    let parse = |text: &str| -> Result<u64, ContentFailure> {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid("区间端点不是规范十进制文本"));
        }
        text.parse::<u64>().map_err(|_| invalid("区间端点溢出"))
    };

    if first.is_empty() {
        // `bytes=-suffix`：最后 suffix 个字节。
        let suffix = parse(last)?;
        if suffix == 0 {
            return Err(ContentFailure::RangeNotSatisfiable {
                size,
                requested: requested.to_owned(),
            });
        }
        if size == 0 {
            return Err(ContentFailure::RangeNotSatisfiable {
                size,
                requested: requested.to_owned(),
            });
        }
        let length = suffix.min(size);
        return Ok(ByteSlice {
            offset: size - length,
            length,
        });
    }

    let start = parse(first)?;
    if start >= size {
        return Err(ContentFailure::RangeNotSatisfiable {
            size,
            requested: requested.to_owned(),
        });
    }
    let end = if last.is_empty() {
        size - 1
    } else {
        let end = parse(last)?;
        if end < start {
            return Err(invalid("区间终点小于起点"));
        }
        end.min(size - 1)
    };
    Ok(ByteSlice {
        offset: start,
        length: end - start + 1,
    })
}

/// 一次下载请求（Range 文本原样传入，由 [`parse_range`] 收敛）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadRequest {
    /// 条目身份。
    pub entry: EntryId,
    /// 版本身份（必须是该条目的版本）。
    pub version: VersionId,
    /// `Range` 头文本；`None` 表示整份内容。
    pub range: Option<String>,
}

/// 打开下载的结果：固定版本事实 + 已收敛的区间 + 保护该内容的下载租约。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadHandle {
    /// 版本内容事实。
    pub content: VersionContent,
    /// 要读取的区间。
    pub slice: ByteSlice,
    /// 下载租约（在读取结束前必须保持有效）。
    pub lease: ContentLease,
}

/// 打开一次固定版本下载：解析版本 → 收敛区间 → **建立下载租约**。
///
/// 租约在读取之前建立，因此“读到一半 GC 把这个历史对象回收”不会发生（design §7.2、§19.4）。
/// 调用方读完字节后必须调用 [`finish_download`] 解除租约。
///
/// # Errors
///
/// 见 [`ContentFailure`]：版本过期（`VersionGone`）、条目不可用（`EntryUnavailable`）、区间不合法
/// （`InvalidRange`/`RangeNotSatisfiable`）、租约失败（`Lease`）、存储失败（`Storage`）。
pub async fn open_download<S, L>(
    store: &mut S,
    leases: &mut L,
    root: RootId,
    request: &DownloadRequest,
    lease_expires_at: UtcTimestamp,
) -> Result<DownloadHandle, ContentFailure>
where
    S: VersionContentStore,
    L: ContentLeasePort,
{
    let content = store
        .version_content(root, request.entry, request.version)
        .await?;
    if content.root != root || content.entry != request.entry || content.version != request.version
    {
        // 适配器返回的事实与本请求不符：不拿“另一个版本/条目”的字节冒充。
        return Err(ContentFailure::EntryUnavailable);
    }
    let slice = match request.range.as_deref() {
        Some(text) if !text.trim().is_empty() => parse_range(text, content.size)?,
        _ => {
            if content.size == 0 {
                ByteSlice {
                    offset: 0,
                    length: 0,
                }
            } else {
                ByteSlice {
                    offset: 0,
                    length: content.size,
                }
            }
        }
    };
    let lease = leases
        .acquire(&ContentLeaseRequest {
            root,
            digest: content.digest,
            purpose: ContentLeasePurpose::Download,
            expires_at: lease_expires_at,
        })
        .await
        .map_err(ContentFailure::Lease)?;
    Ok(DownloadHandle {
        content,
        slice,
        lease,
    })
}

/// 读出的下载字节。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadBytes {
    /// 版本内容事实（区间起点/长度由 [`DownloadHandle::slice`] 给出）。
    pub content: VersionContent,
    /// 区间起点。
    pub offset: u64,
    /// 读出的字节。
    pub data: Vec<u8>,
}

/// 按已打开的下载句柄读取字节。
///
/// 覆盖整个对象时核对整份 SHA-256（不允许把改写/截断的字节当成这个版本）；部分区间只核对读到的
/// 字节数——整文件校验在客户端按 design §13.2 完成。
///
/// # Errors
///
/// 对象读取失败 → [`ContentFailure::Read`]；字节数与区间不符或整文件摘要不符 →
/// [`ContentFailure::ContentMismatch`]。
pub fn read_download(
    port: &mut dyn ObjectBlobPort,
    layout: &ObjectLayout,
    handle: &DownloadHandle,
) -> Result<DownloadBytes, ContentFailure> {
    let object = layout.object_path(&handle.content.digest);
    let stored = port.read_object_range(&object, handle.slice.offset, handle.slice.length)?;
    if stored.bytes != handle.slice.length || stored.data.len() as u64 != stored.bytes {
        return Err(ContentFailure::ContentMismatch {
            expected: handle.content.digest,
            actual: handle.content.digest,
            expected_size: handle.slice.length,
            actual_size: stored.bytes,
        });
    }
    if handle.slice.covers_whole(handle.content.size) {
        let digest = ContentDigest::from_bytes(hash::digest_of(&stored.data));
        if digest != handle.content.digest {
            return Err(ContentFailure::ContentMismatch {
                expected: handle.content.digest,
                actual: digest,
                expected_size: handle.content.size,
                actual_size: stored.bytes,
            });
        }
    }
    Ok(DownloadBytes {
        content: handle.content.clone(),
        offset: handle.slice.offset,
        data: stored.data,
    })
}

/// 结束一次下载：解除下载租约。
///
/// 解除失败**不**改变已经发出的字节（那些字节已经是客户端的事），但必须如实报告，由调用方记录并
/// 让租约自然到期回收（design §7.2：不能因此把已完成的读取改判为失败）。
///
/// # Errors
///
/// 租约端口失败 → [`ContentFailure::Lease`]。
pub async fn finish_download<L>(
    leases: &mut L,
    handle: DownloadHandle,
) -> Result<(), ContentFailure>
where
    L: ContentLeasePort,
{
    leases
        .release(&handle.lease)
        .await
        .map_err(ContentFailure::Lease)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::uploads::{PartWrite, UploadFailure};
    use crate::objects::ReuseOutcome;
    use crate::ports::object_store::{
        PublishedBlob, StagedBlob, StoredBytes, StoredFacts, StoredSlice,
    };
    use std::path::{Path, PathBuf};
    use vaultsyncd_domain::ids::ObjectLeaseId;

    fn digest(bytes: &[u8]) -> ContentDigest {
        ContentDigest::from_bytes(hash::digest_of(bytes))
    }

    fn session(total: u64, chunk: u64, phase: UploadPhase) -> UploadSession {
        UploadSession {
            upload: UploadId::from_bytes([0x51; 16]),
            root: RootId::from_bytes([0x52; 16]),
            session: vaultsyncd_domain::ids::SessionId::from_bytes([0x53; 16]),
            total_size: total,
            hash: digest(b"whole"),
            chunk_size: chunk,
            phase,
            expires_at: UtcTimestamp::from_unix_seconds(1_700_010_000)
                .expect("测试会话在调用时间之后到期"),
        }
    }

    /// finalize 用的假会话存储：记录状态推进顺序。
    #[derive(Default)]
    struct FakeUploads {
        session: Option<UploadSession>,
        parts: Vec<StoredPart>,
        calls: Vec<String>,
        fail_complete: bool,
    }

    #[allow(async_fn_in_trait)]
    impl UploadSessionStore for FakeUploads {
        async fn load(&mut self, _upload: UploadId) -> Result<UploadSession, UploadFailure> {
            self.calls.push("load".to_owned());
            self.session.clone().ok_or(UploadFailure::UnknownUpload)
        }

        async fn existing_part(
            &mut self,
            _upload: UploadId,
            index: u64,
        ) -> Result<Option<StoredPart>, UploadFailure> {
            Ok(self.parts.iter().find(|part| part.index == index).cloned())
        }

        async fn record_part(
            &mut self,
            _upload: UploadId,
            _part: &StoredPart,
        ) -> Result<PartWrite, UploadFailure> {
            unreachable!("finalize 不接收块")
        }

        async fn parts(&mut self, _upload: UploadId) -> Result<Vec<StoredPart>, UploadFailure> {
            self.calls.push("parts".to_owned());
            Ok(self.parts.clone())
        }

        async fn mark_complete(&mut self, _upload: UploadId) -> Result<(), UploadFailure> {
            self.calls.push("complete".to_owned());
            if self.fail_complete {
                return Err(UploadFailure::Storage {
                    detail: "规则失败".to_owned(),
                });
            }
            Ok(())
        }

        async fn mark_finalized(&mut self, _upload: UploadId) -> Result<(), UploadFailure> {
            self.calls.push("finalized".to_owned());
            Ok(())
        }

        async fn abort(&mut self, _upload: UploadId) -> Result<(), UploadFailure> {
            self.calls.push("abort".to_owned());
            Ok(())
        }
    }

    /// 拼装发布端口：只报告事实，不决定成败。
    struct FakePublisher {
        published: Option<PublishedContent>,
        parts_seen: Vec<u64>,
    }

    impl ContentPublisher for FakePublisher {
        fn publish_parts(
            &mut self,
            _upload: UploadId,
            _staging_name: &str,
            parts: &[StoredPart],
        ) -> Result<PublishedContent, PublishFailure> {
            self.parts_seen = parts.iter().map(|part| part.index).collect();
            self.published.take().ok_or(PublishFailure::Storage {
                stage: "assemble",
                no_space: false,
                detail: "测试必须预设发布结果".to_owned(),
            })
        }
    }

    #[derive(Default)]
    struct FakeObjects {
        recorded: Vec<(RootId, PublishedContent)>,
    }

    #[allow(async_fn_in_trait)]
    impl ContentObjectStore for FakeObjects {
        async fn record_published(
            &mut self,
            root: RootId,
            content: &PublishedContent,
            _verified_at: UtcTimestamp,
        ) -> Result<(), PublishFailure> {
            self.recorded.push((root, content.clone()));
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeLeases {
        acquired: Vec<ContentLeaseRequest>,
        released: Vec<ObjectLeaseId>,
    }

    #[allow(async_fn_in_trait)]
    impl ContentLeasePort for FakeLeases {
        async fn acquire(
            &mut self,
            request: &ContentLeaseRequest,
        ) -> Result<ContentLease, ContentLeaseError> {
            self.acquired.push(*request);
            Ok(ContentLease {
                id: ObjectLeaseId::from_bytes([0x61; 16]),
                purpose: request.purpose,
                expires_at: request.expires_at,
            })
        }

        async fn release(&mut self, lease: &ContentLease) -> Result<(), ContentLeaseError> {
            self.released.push(lease.id);
            Ok(())
        }
    }

    /// 假版本事实来源：`versions` 里有什么就返回什么，`current` 模拟“路径上的 head 已经变了”。
    struct FakeVersions {
        facts: Option<VersionContent>,
        current: ContentDigest,
    }

    #[allow(async_fn_in_trait)]
    impl VersionContentStore for FakeVersions {
        async fn version_content(
            &mut self,
            root: RootId,
            entry: EntryId,
            version: VersionId,
        ) -> Result<VersionContent, VersionContentError> {
            let _ = self.current;
            match &self.facts {
                Some(facts)
                    if facts.root == root && facts.entry == entry && facts.version == version =>
                {
                    Ok(facts.clone())
                }
                Some(_) => Err(VersionContentError::VersionGone),
                None => Err(VersionContentError::VersionGone),
            }
        }
    }

    #[derive(Default)]
    struct FakeBlobPort {
        slice: Option<StoredSlice>,
        read_calls: Vec<(PathBuf, u64, u64)>,
    }

    impl ObjectBlobPort for FakeBlobPort {
        fn stage_working_copy(
            &mut self,
            _source: &Path,
            _staging: &Path,
        ) -> Result<StagedBlob, BlobError> {
            Err(BlobError::InvalidRequest {
                detail: "本用例不建副本".to_owned(),
            })
        }

        fn publish_staged(
            &mut self,
            _staged: &StagedBlob,
            _object: &Path,
        ) -> Result<PublishedBlob, BlobError> {
            Err(BlobError::InvalidRequest {
                detail: "本用例不发布".to_owned(),
            })
        }

        fn discard_staged(&mut self, _staging: &Path) -> Result<(), BlobError> {
            Ok(())
        }

        fn verify_object(&mut self, _object: &Path) -> Result<StoredFacts, BlobError> {
            Err(BlobError::InvalidRequest {
                detail: "本用例不复核".to_owned(),
            })
        }

        fn read_object(&mut self, _object: &Path) -> Result<StoredBytes, BlobError> {
            Err(BlobError::InvalidRequest {
                detail: "本用例不整读".to_owned(),
            })
        }

        fn read_object_range(
            &mut self,
            object: &Path,
            offset: u64,
            length: u64,
        ) -> Result<StoredSlice, BlobError> {
            self.read_calls.push((object.to_path_buf(), offset, length));
            self.slice.take().ok_or(BlobError::ObjectMissing {
                detail: "测试必须预设切片结果".to_owned(),
            })
        }
    }

    fn time(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_seconds(seconds).expect("时间")
    }

    fn published(bytes: &[u8]) -> PublishedContent {
        PublishedContent {
            digest: digest(bytes),
            size: bytes.len() as u64,
            storage_path: ObjectLayout::relative_object_path(&digest(bytes))
                .display()
                .to_string(),
            reuse: ReuseOutcome::FreshlyPublished,
        }
    }

    fn parts_of(pieces: &[&[u8]]) -> Vec<StoredPart> {
        pieces
            .iter()
            .enumerate()
            .map(|(index, bytes)| StoredPart {
                index: index as u64,
                length: bytes.len() as u64,
                hash: digest(bytes),
                path: format!("uploads/51/{index:020}"),
            })
            .collect()
    }

    #[test]
    fn assembled_parts_require_contiguous_full_geometry() {
        let whole = b"abcdef";
        let session = UploadSession {
            hash: digest(whole),
            ..session(6, 3, UploadPhase::Open)
        };
        let complete = AssembledParts {
            parts: parts_of(&[b"abc", b"def"]),
            part_count: 2,
        };
        assert!(complete.verify(&session).is_ok());

        let missing = AssembledParts {
            parts: parts_of(&[b"abc"]),
            part_count: 2,
        };
        assert!(matches!(
            missing.verify(&session),
            Err(ContentFailure::IncompleteUpload {
                expected: 2,
                present: 1
            })
        ));

        let mut gap = complete.clone();
        gap.parts[1].index = 5;
        assert!(matches!(
            gap.verify(&session),
            Err(ContentFailure::IncompleteUpload { .. })
        ));

        let mut short = complete.clone();
        short.parts[0].length = 2;
        assert!(matches!(
            short.verify(&session),
            Err(ContentFailure::IncompleteUpload { .. })
        ));
    }

    #[tokio::test]
    async fn finalize_publishes_content_and_only_returns_a_reference() {
        let piece_a: &[u8] = b"abc";
        let piece_b: &[u8] = b"def";
        let whole: &[u8] = b"abcdef";
        let session = UploadSession {
            hash: digest(whole),
            ..session(6, 3, UploadPhase::Open)
        };
        let mut store = FakeUploads {
            session: Some(session.clone()),
            parts: parts_of(&[piece_a, piece_b]),
            ..FakeUploads::default()
        };
        let mut publisher = FakePublisher {
            published: Some(published(whole)),
            parts_seen: Vec::new(),
        };
        let mut objects = FakeObjects::default();

        let finished = finalize_upload(
            &mut store,
            &mut publisher,
            &mut objects,
            session.upload,
            "cas-1",
            time(1_700_000_100),
        )
        .await
        .expect("finalize 成功");

        assert_eq!(finished.content.digest, digest(whole));
        assert_eq!(finished.content.size, 6);
        assert_eq!(finished.part_count, 2);
        assert!(!finished.replayed);
        assert_eq!(publisher.parts_seen, vec![0, 1], "必须按索引升序拼装");
        assert_eq!(store.calls, vec!["load", "parts", "complete", "finalized"]);
        assert_eq!(objects.recorded.len(), 1, "内容对象在 finalize 时登记一次");
        assert_eq!(
            finished.content.storage_path,
            objects.recorded[0].1.storage_path
        );
        // finalize 只碰上传会话与内容对象：没有任何“条目提交”的调用（条目 head 不变）。
        assert!(
            !store.calls.iter().any(|call| call == "abort"),
            "成功路径不放弃会话"
        );
    }

    #[tokio::test]
    async fn finalize_with_missing_parts_changes_nothing() {
        let session = UploadSession {
            hash: digest(b"abcdef"),
            ..session(6, 3, UploadPhase::Open)
        };
        let mut store = FakeUploads {
            session: Some(session.clone()),
            parts: parts_of(&[b"abc"]),
            ..FakeUploads::default()
        };
        let mut publisher = FakePublisher {
            published: Some(published(b"abcdef")),
            parts_seen: Vec::new(),
        };
        let mut objects = FakeObjects::default();
        let failure = finalize_upload(
            &mut store,
            &mut publisher,
            &mut objects,
            session.upload,
            "cas-1",
            time(1_700_000_100),
        )
        .await
        .expect_err("缺块不得 finalize");
        assert!(matches!(
            failure,
            ContentFailure::IncompleteUpload {
                expected: 2,
                present: 1
            }
        ));
        assert_eq!(store.calls, vec!["load", "parts"], "不得推进会话状态");
        assert!(objects.recorded.is_empty());
        assert!(publisher.parts_seen.is_empty(), "不得拼装任何字节");
    }

    #[tokio::test]
    async fn content_mismatch_aborts_the_session_without_registering_anything() {
        let session = UploadSession {
            hash: digest(b"abcdef"),
            ..session(6, 3, UploadPhase::Open)
        };
        let mut store = FakeUploads {
            session: Some(session.clone()),
            parts: parts_of(&[b"abc", b"def"]),
            ..FakeUploads::default()
        };
        // 拼装出的内容与声明的快照不符（模拟暂存块被换掉）。
        let mut publisher = FakePublisher {
            published: Some(published(b"abcdeg")),
            parts_seen: Vec::new(),
        };
        let mut objects = FakeObjects::default();
        let failure = finalize_upload(
            &mut store,
            &mut publisher,
            &mut objects,
            session.upload,
            "cas-1",
            time(1_700_000_100),
        )
        .await
        .expect_err("内容不符必须失败");
        match failure {
            ContentFailure::ContentMismatch {
                expected,
                actual,
                expected_size,
                actual_size,
            } => {
                assert_eq!(expected, digest(b"abcdef"));
                assert_eq!(actual, digest(b"abcdeg"));
                assert_eq!((expected_size, actual_size), (6, 6));
            }
            other => panic!("期望 ContentMismatch，实际 {other:?}"),
        }
        assert_eq!(
            store.calls,
            vec!["load", "parts", "complete", "abort"],
            "校验失败：放弃会话、不标 finalized"
        );
        assert!(objects.recorded.is_empty(), "不符的内容不得登记为对象");
    }

    #[tokio::test]
    async fn finalize_replay_returns_the_same_reference_without_republishing() {
        let session = UploadSession {
            hash: digest(b"abcdef"),
            ..session(6, 3, UploadPhase::Finalized)
        };
        let mut store = FakeUploads {
            session: Some(session.clone()),
            ..FakeUploads::default()
        };
        let mut publisher = FakePublisher {
            published: None,
            parts_seen: Vec::new(),
        };
        let mut objects = FakeObjects::default();
        let replayed = finalize_upload(
            &mut store,
            &mut publisher,
            &mut objects,
            session.upload,
            "cas-1",
            time(1_700_000_100),
        )
        .await
        .expect("幂等重放");
        assert!(replayed.replayed);
        assert_eq!(replayed.content.digest, digest(b"abcdef"));
        assert_eq!(replayed.content.size, 6);
        assert_eq!(store.calls, vec!["load"], "重放不再拼装、不再改状态");
        assert!(objects.recorded.is_empty());
    }

    #[tokio::test]
    async fn expired_finalize_cannot_replay_a_collected_content_reference() {
        let session = session(6, 3, UploadPhase::Finalized);
        let mut store = FakeUploads {
            session: Some(session.clone()),
            ..FakeUploads::default()
        };
        let mut publisher = FakePublisher {
            published: None,
            parts_seen: Vec::new(),
        };
        let mut objects = FakeObjects::default();
        assert!(matches!(
            finalize_upload(
                &mut store,
                &mut publisher,
                &mut objects,
                session.upload,
                "expired",
                session.expires_at
            )
            .await,
            Err(ContentFailure::Upload(UploadFailure::UnknownUpload))
        ));
        assert_eq!(store.calls, vec!["load"]);
        assert!(objects.recorded.is_empty());
    }

    #[tokio::test]
    async fn aborted_session_cannot_be_finalized() {
        let session = session(6, 3, UploadPhase::Aborted);
        let mut store = FakeUploads {
            session: Some(session.clone()),
            ..FakeUploads::default()
        };
        let mut publisher = FakePublisher {
            published: None,
            parts_seen: Vec::new(),
        };
        let mut objects = FakeObjects::default();
        let failure = finalize_upload(
            &mut store,
            &mut publisher,
            &mut objects,
            session.upload,
            "cas-1",
            time(1_700_000_100),
        )
        .await
        .expect_err("已放弃的会话不能 finalize");
        assert!(matches!(
            failure,
            ContentFailure::Upload(UploadFailure::PartsClosed {
                phase: UploadPhase::Aborted
            })
        ));
        assert_eq!(store.calls, vec!["load"]);
    }

    fn range_slice(header: &str, size: u64) -> ByteSlice {
        parse_range(header, size).expect("区间应当合法")
    }

    #[test]
    fn range_parsing_covers_the_three_forms_and_rejects_the_rest() {
        assert_eq!(
            range_slice("bytes=0-3", 10),
            ByteSlice {
                offset: 0,
                length: 4
            }
        );
        assert_eq!(
            range_slice("bytes=4-", 10),
            ByteSlice {
                offset: 4,
                length: 6
            }
        );
        assert_eq!(
            range_slice("bytes=-3", 10),
            ByteSlice {
                offset: 7,
                length: 3
            }
        );
        // 终点超出对象长度：收敛到最后一个字节，而不是报错。
        assert_eq!(
            range_slice("bytes=8-99", 10),
            ByteSlice {
                offset: 8,
                length: 2
            }
        );
        // 后缀长于对象：整份内容。
        assert_eq!(
            range_slice("bytes=-99", 10),
            ByteSlice {
                offset: 0,
                length: 10
            }
        );
        assert!(matches!(
            parse_range("items=0-1", 10),
            Err(ContentFailure::InvalidRange { .. })
        ));
        assert!(matches!(
            parse_range("bytes=0-1,3-4", 10),
            Err(ContentFailure::InvalidRange { .. })
        ));
        assert!(matches!(
            parse_range("bytes=5-1", 10),
            Err(ContentFailure::InvalidRange { .. })
        ));
        assert!(matches!(
            parse_range("bytes=x-1", 10),
            Err(ContentFailure::InvalidRange { .. })
        ));
        assert!(matches!(
            parse_range("bytes=10-", 10),
            Err(ContentFailure::RangeNotSatisfiable { size: 10, .. })
        ));
        assert!(matches!(
            parse_range("bytes=-0", 10),
            Err(ContentFailure::RangeNotSatisfiable { .. })
        ));
        assert!(matches!(
            parse_range("bytes=0-0", 0),
            Err(ContentFailure::RangeNotSatisfiable { .. })
        ));
    }

    #[tokio::test]
    async fn download_uses_the_requested_version_even_after_head_moved() {
        let old_bytes: &[u8] = b"version-one";
        let new_bytes: &[u8] = b"version-two-with-more-bytes";
        let facts = VersionContent {
            root: RootId::from_bytes([0x52; 16]),
            entry: EntryId::from_bytes([0x54; 16]),
            version: VersionId::from_bytes([0x55; 16]),
            digest: digest(old_bytes),
            size: old_bytes.len() as u64,
        };
        let mut store = FakeVersions {
            facts: Some(facts.clone()),
            // 路径上的 head 已经换了新内容：按版本下载必须仍然是旧字节。
            current: digest(new_bytes),
        };
        let mut leases = FakeLeases::default();
        let handle = open_download(
            &mut store,
            &mut leases,
            facts.root,
            &DownloadRequest {
                entry: facts.entry,
                version: facts.version,
                range: None,
            },
            time(1_700_000_200),
        )
        .await
        .expect("打开下载");
        assert_eq!(handle.content, facts);
        assert_eq!(
            handle.slice,
            ByteSlice {
                offset: 0,
                length: old_bytes.len() as u64
            }
        );
        assert_eq!(leases.acquired.len(), 1, "读取前必须建立下载租约");
        assert_eq!(leases.acquired[0].purpose, ContentLeasePurpose::Download);
        assert_eq!(leases.acquired[0].digest, digest(old_bytes));

        let layout = ObjectLayout::new("/objects");
        let mut port = FakeBlobPort {
            slice: Some(StoredSlice {
                data: old_bytes.to_vec(),
                bytes: old_bytes.len() as u64,
            }),
            read_calls: Vec::new(),
        };
        let bytes = read_download(&mut port, &layout, &handle).expect("读取旧版本字节");
        assert_eq!(bytes.data, old_bytes);
        assert_eq!(
            port.read_calls[0].0,
            layout.object_path(&digest(old_bytes)),
            "读取路径必须来自版本的内容地址，而不是当前 head"
        );
        finish_download(&mut leases, handle)
            .await
            .expect("解除租约");
        assert_eq!(leases.released, vec![ObjectLeaseId::from_bytes([0x61; 16])]);
    }

    #[tokio::test]
    async fn expired_version_is_version_gone_and_holds_no_lease() {
        let mut store = FakeVersions {
            facts: None,
            current: digest(b"new"),
        };
        let mut leases = FakeLeases::default();
        let failure = open_download(
            &mut store,
            &mut leases,
            RootId::from_bytes([0x52; 16]),
            &DownloadRequest {
                entry: EntryId::from_bytes([0x54; 16]),
                version: VersionId::from_bytes([0x55; 16]),
                range: None,
            },
            time(1_700_000_200),
        )
        .await
        .expect_err("过期版本");
        assert!(matches!(failure, ContentFailure::VersionGone));
        assert!(leases.acquired.is_empty(), "拿不到版本事实就不建租约");
    }

    #[tokio::test]
    async fn range_download_reads_exactly_the_requested_slice() {
        let bytes: &[u8] = b"0123456789";
        let facts = VersionContent {
            root: RootId::from_bytes([0x52; 16]),
            entry: EntryId::from_bytes([0x54; 16]),
            version: VersionId::from_bytes([0x55; 16]),
            digest: digest(bytes),
            size: bytes.len() as u64,
        };
        let mut store = FakeVersions {
            facts: Some(facts.clone()),
            current: digest(bytes),
        };
        let mut leases = FakeLeases::default();
        let handle = open_download(
            &mut store,
            &mut leases,
            facts.root,
            &DownloadRequest {
                entry: facts.entry,
                version: facts.version,
                range: Some("bytes=2-5".to_owned()),
            },
            time(1_700_000_200),
        )
        .await
        .expect("打开区间下载");
        assert_eq!(handle.slice.content_range(10), "bytes 2-5/10");

        let layout = ObjectLayout::new("/objects");
        let mut port = FakeBlobPort {
            slice: Some(StoredSlice {
                data: bytes[2..6].to_vec(),
                bytes: 4,
            }),
            read_calls: Vec::new(),
        };
        let read = read_download(&mut port, &layout, &handle).expect("读取区间");
        assert_eq!(read.offset, 2);
        assert_eq!(read.data, b"2345");
        assert_eq!(port.read_calls[0].1, 2);
        assert_eq!(port.read_calls[0].2, 4);

        // 短读：端口少给字节时必须报错，不能把少了字节的切片当成内容。
        let mut short = FakeBlobPort {
            slice: Some(StoredSlice {
                data: bytes[2..4].to_vec(),
                bytes: 2,
            }),
            read_calls: Vec::new(),
        };
        assert!(matches!(
            read_download(&mut short, &layout, &handle),
            Err(ContentFailure::ContentMismatch { .. })
        ));
    }

    #[test]
    fn whole_object_read_is_verified_against_the_version_digest() {
        let bytes: &[u8] = b"fixed bytes";
        let content = VersionContent {
            root: RootId::from_bytes([0x52; 16]),
            entry: EntryId::from_bytes([0x54; 16]),
            version: VersionId::from_bytes([0x55; 16]),
            digest: digest(bytes),
            size: bytes.len() as u64,
        };
        let handle = DownloadHandle {
            slice: ByteSlice {
                offset: 0,
                length: content.size,
            },
            content,
            lease: ContentLease {
                id: ObjectLeaseId::from_bytes([0x61; 16]),
                purpose: ContentLeasePurpose::Download,
                expires_at: time(1_700_000_200),
            },
        };
        let layout = ObjectLayout::new("/objects");
        let mut good = FakeBlobPort {
            slice: Some(StoredSlice {
                data: bytes.to_vec(),
                bytes: bytes.len() as u64,
            }),
            read_calls: Vec::new(),
        };
        assert_eq!(
            read_download(&mut good, &layout, &handle)
                .expect("整份一致")
                .data,
            bytes
        );

        let mut tampered = FakeBlobPort {
            slice: Some(StoredSlice {
                data: b"fixed bytez".to_vec(),
                bytes: 11,
            }),
            read_calls: Vec::new(),
        };
        assert!(matches!(
            read_download(&mut tampered, &layout, &handle),
            Err(ContentFailure::ContentMismatch { actual, .. }) if actual == digest(b"fixed bytez")
        ));
    }
}
