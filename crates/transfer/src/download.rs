//! 下载执行：固定 VersionId → 检查空间 → 只取缺失块 → 整份摘要校验（design §13.1—13.3）。
//!
//! # 不变量
//!
//! - **版本固定**：请求只带 `(entry, version)`，字节只来自那个不可变版本；整份摘要与大小是校验
//!   依据，不是“当前内容”的样子。服务端若拿另一个版本、或已清理的版本回答，摘要对不上就不会安装
//!   （[`DownloadError::ContentMismatch`]）。这也意味着本模块**不做**跨版本块增量：没有“复用旧版本
//!   块”的入口。
//! - **进度可持久化**：位图按 `(binding, entry, version)` 存放，块几何或临时路径变了就作废旧位图。
//!   断线后只请求缺失块；已写入临时文件的块不会白丢。
//! - **先检查空间再写**：空间不足立刻失败，不删除既有内容，也不留下半份可安装内容。
//! - **校验通过才交给上层安装**：本模块只产出“临时文件 + 已核对的整份摘要”；写发布意图、受控
//!   安装与状态更新由上层完成（design §13.2 的最后两步），因此这里不会覆盖用户文件，也不会把
//!   “下载完成”当成“条目已提交”。
//! - **整份摘要流式计算**：校验读回临时文件时按固定窗口读，不把内容整体读进内存。

use crate::bitmap::PartBitmap;
use crate::chunk::ChunkGeometry;
use crate::concurrency::{CancelToken, bounded_join};
use crate::hash::{DIGEST_BYTES, to_hex};
use crate::ports::{
    DownloadKey, DownloadProgressStore, PortError, PortFuture, StoredProgress, TempContent,
    VersionPin, VersionedContent,
};
use crate::stream::{ChunkReader, StreamError, streamed_digest};

/// 整份摘要不符时的重下次数（首次 + 一次重下）：持续不符要升为重要故障，不能无限重试。
pub const DEFAULT_CONTENT_ATTEMPTS: usize = 2;

/// 下载执行参数。
#[derive(Clone, Debug)]
pub struct DownloadOptions {
    /// 块大小（客户端决定分块边界；服务端按 `Range` 供字节）。
    pub chunk_size: u64,
    /// 同时在飞行的块数上限（至少 1）。
    pub part_concurrency: usize,
    /// 整份摘要不符时的最多尝试次数（至少 1）。
    pub content_attempts: usize,
    /// 取消开关。
    pub cancel: CancelToken,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            chunk_size: crate::upload::DEFAULT_CHUNK_SIZE,
            part_concurrency: crate::upload::DEFAULT_PART_CONCURRENCY,
            content_attempts: DEFAULT_CONTENT_ATTEMPTS,
            cancel: CancelToken::new(),
        }
    }
}

impl DownloadOptions {
    /// 用给定块大小与默认并发/尝试次数构造。
    #[must_use]
    pub fn new(chunk_size: u64) -> Self {
        Self {
            chunk_size,
            ..Self::default()
        }
    }

    /// 设置块并发上限。
    #[must_use]
    pub fn with_part_concurrency(mut self, part_concurrency: usize) -> Self {
        self.part_concurrency = part_concurrency;
        self
    }

    /// 设置整份摘要不符时的最多尝试次数。
    #[must_use]
    pub fn with_content_attempts(mut self, content_attempts: usize) -> Self {
        self.content_attempts = content_attempts;
        self
    }

    /// 挂上取消开关。
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    fn validate(&self) -> Result<(), DownloadError> {
        if self.chunk_size == 0 {
            return Err(DownloadError::InvalidOptions {
                detail: "块大小必须大于 0".to_owned(),
            });
        }
        if self.part_concurrency == 0 || self.content_attempts == 0 {
            return Err(DownloadError::InvalidOptions {
                detail: "块并发与整份尝试次数都至少为 1".to_owned(),
            });
        }
        Ok(())
    }
}

/// 一次成功下载的结果：可安装的临时内容事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadOutcome {
    /// 临时文件路径（上层据此写发布意图并受控安装）。
    pub temp_path: String,
    /// 内容字节数（等于固定版本的大小）。
    pub size: u64,
    /// 实测整份 SHA-256（等于固定版本的摘要）。
    pub digest: [u8; DIGEST_BYTES],
    /// 块数。
    pub part_count: u64,
    /// 本次实际下载的块数（不含重启前已收到的块）。
    pub downloaded_parts: u64,
    /// 本次是否使用了已持久化的进度。
    pub resumed: bool,
    /// 用掉的整份尝试次数（1 表示一次通过）。
    pub attempts: usize,
}

/// 下载失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadError {
    /// 参数或身份非法。
    InvalidOptions {
        /// 面向诊断的原因。
        detail: String,
    },
    /// 临时区空间不足：不写半份内容，也不删除既有内容。
    NoSpace {
        /// 需要保留的字节数。
        needed: u64,
    },
    /// 调用方取消：已写入的块与进度保留。
    Cancelled,
    /// 整份摘要与固定版本不符（含服务端供给了另一个版本的内容）：不安装。
    ContentMismatch {
        /// 固定版本的摘要。
        expected: [u8; DIGEST_BYTES],
        /// 实测摘要。
        actual: [u8; DIGEST_BYTES],
    },
    /// 端口失败（网络、认证、服务端拒绝、本地 I/O）。
    Port(PortError),
}

impl DownloadError {
    /// 退避后原样重试是否可能成功。
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Port(error) if error.is_retryable())
    }

    /// 是否被取消。
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled | Self::Port(PortError::Cancelled))
    }

    /// 是否需要用户重新登录。
    #[must_use]
    pub const fn needs_reauth(&self) -> bool {
        matches!(self, Self::Port(error) if error.needs_reauth())
    }

    /// 空间不足（应立即通知用户，不当作网络失败退避）。
    #[must_use]
    pub const fn is_no_space(&self) -> bool {
        matches!(self, Self::NoSpace { .. })
    }

    /// 端口错误（不是端口错误时为 `None`）。
    #[must_use]
    pub const fn port_error(&self) -> Option<&PortError> {
        match self {
            Self::Port(error) => Some(error),
            _ => None,
        }
    }
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidOptions { detail } => f.write_str(detail),
            Self::NoSpace { needed } => write!(f, "临时空间不足，需要 {needed} 字节"),
            Self::Cancelled => f.write_str("已取消"),
            Self::ContentMismatch { expected, actual } => write!(
                f,
                "内容摘要不符：固定版本是 {}，实测 {}",
                to_hex(expected),
                to_hex(actual)
            ),
            Self::Port(error) => write!(f, "端口失败：{error}"),
        }
    }
}

impl std::error::Error for DownloadError {}

/// 执行一次下载（新建或续传）。
///
/// 成功时临时文件里是**已经按整份摘要核对过**的固定版本内容；安装与状态更新由上层完成。
///
/// # Errors
///
/// 见 [`DownloadError`]：参数非法、空间不足、取消、整份摘要不符（有限次重下后仍不符）或端口失败。
pub async fn run(
    content: &dyn VersionedContent,
    temp: &dyn TempContent,
    progress: &dyn DownloadProgressStore,
    pin: &VersionPin,
    options: &DownloadOptions,
) -> Result<DownloadOutcome, DownloadError> {
    options.validate()?;
    let geometry = ChunkGeometry::new(pin.size, options.chunk_size).map_err(|error| {
        DownloadError::InvalidOptions {
            detail: format!("块几何非法：{error}"),
        }
    })?;
    let key = DownloadKey::new(&pin.binding, &pin.entry, &pin.version)
        .map_err(|detail| DownloadError::InvalidOptions { detail })?;

    let mut bitmap = load_progress(progress, &key, temp, geometry).await?;
    let mut resumed = bitmap.count() > 0;
    let mut downloaded_parts = 0_u64;

    // 先检查空间：不足时立刻失败，不动临时文件，更不删既有内容。
    temp.ensure_capacity(pin.size)
        .await
        .map_err(|error| local_error(error, pin.size))?;

    let mut attempt = 0_usize;
    loop {
        attempt += 1;
        let missing = bitmap.missing();
        if !missing.is_empty() {
            let (completed, failure) =
                fetch_missing(content, temp, pin, geometry, &missing, options).await?;
            for (_, index) in &completed {
                if bitmap
                    .mark(*index)
                    .map_err(|error| DownloadError::InvalidOptions {
                        detail: format!("位图写入失败：{error}"),
                    })?
                {
                    downloaded_parts += 1;
                }
            }
            // 先落进度再处理失败：断线或取消后已完成的部分不会白丢。
            if !completed.is_empty() {
                save_progress(progress, &key, temp, geometry, &bitmap).await?;
            }
            if let Some((_, error)) = failure {
                return Err(error);
            }
        }

        // 校验前必须让字节真正到达磁盘。
        temp.sync()
            .await
            .map_err(|error| local_error(error, pin.size))?;
        let actual = streamed_digest(
            &TempReader(temp),
            pin.size,
            options.chunk_size,
            &options.cancel,
        )
        .await
        .map_err(stream_error)?;
        if actual == pin.digest {
            save_progress(progress, &key, temp, geometry, &bitmap).await?;
            return Ok(DownloadOutcome {
                temp_path: temp.path().to_owned(),
                size: pin.size,
                digest: actual,
                part_count: geometry.part_count(),
                downloaded_parts,
                resumed,
                attempts: attempt,
            });
        }

        // 摘要不符：无法定位坏块（服务端不提供每块摘要），按 design §13.2 有限次重新下载。
        // 先作废可重建的临时内容与位图，绝不安装这份内容。
        progress
            .discard(&key)
            .await
            .map_err(|error| local_error(error, pin.size))?;
        temp.discard()
            .await
            .map_err(|error| local_error(error, pin.size))?;
        if attempt >= options.content_attempts {
            return Err(DownloadError::ContentMismatch {
                expected: pin.digest,
                actual,
            });
        }
        bitmap = PartBitmap::empty(geometry);
        resumed = false;
    }
}

/// 下载缺失块：返回已完成块与（可选的）第一个失败。
async fn fetch_missing(
    content: &dyn VersionedContent,
    temp: &dyn TempContent,
    pin: &VersionPin,
    geometry: ChunkGeometry,
    missing: &[crate::bitmap::PartRange],
    options: &DownloadOptions,
) -> Result<(Vec<(usize, u64)>, Option<(usize, DownloadError)>), DownloadError> {
    let chunk_size = geometry.chunk_size();
    let cancel = &options.cancel;
    let mut futures = Vec::new();
    for index in missing.iter().flat_map(|range| range.indices()) {
        let length = geometry
            .part_length(index)
            .ok_or_else(|| DownloadError::InvalidOptions {
                detail: format!("第 {index} 块的索引超出几何"),
            })?;
        let offset =
            index
                .checked_mul(chunk_size)
                .ok_or_else(|| DownloadError::InvalidOptions {
                    detail: "块偏移乘法溢出".to_owned(),
                })?;
        futures.push(async move {
            if cancel.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            let bytes = content
                .read_range(pin, offset, length)
                .await
                .map_err(DownloadError::Port)?;
            if bytes.len() as u64 != length {
                return Err(DownloadError::Port(PortError::protocol(format!(
                    "第 {index} 块应为 {length} 字节，服务端给了 {} 字节",
                    bytes.len()
                ))));
            }
            temp.write_at(offset, &bytes)
                .await
                .map_err(|error| local_error(error, pin.size))?;
            Ok(index)
        });
    }
    let outcome = bounded_join(futures, options.part_concurrency).await;
    Ok((outcome.completed, outcome.failure))
}

/// 读取已持久化的位图；块几何、总大小或临时路径不符时作废旧进度。
async fn load_progress(
    progress: &dyn DownloadProgressStore,
    key: &DownloadKey,
    temp: &dyn TempContent,
    geometry: ChunkGeometry,
) -> Result<PartBitmap, DownloadError> {
    let stored = progress
        .load(key)
        .await
        .map_err(|error| local_error(error, geometry.total_size()))?;
    let Some(stored) = stored else {
        return Ok(PartBitmap::empty(geometry));
    };
    let usable = stored.chunk_size() == geometry.chunk_size()
        && stored.total_size() == geometry.total_size()
        && stored.temp_path() == temp.path()
        && stored.bitmap().geometry() == geometry;
    if usable {
        return Ok(stored.bitmap().clone());
    }
    // 临时文件或块边界变了：旧位图没有意义，丢弃后从第 0 块开始。
    progress
        .discard(key)
        .await
        .map_err(|error| local_error(error, geometry.total_size()))?;
    Ok(PartBitmap::empty(geometry))
}

async fn save_progress(
    progress: &dyn DownloadProgressStore,
    key: &DownloadKey,
    temp: &dyn TempContent,
    geometry: ChunkGeometry,
    bitmap: &PartBitmap,
) -> Result<(), DownloadError> {
    let record = StoredProgress::new(
        geometry.chunk_size(),
        geometry.total_size(),
        temp.path(),
        bitmap.clone(),
    )
    .map_err(|detail| DownloadError::InvalidOptions { detail })?;
    progress
        .save(key, &record)
        .await
        .map_err(|error| local_error(error, geometry.total_size()))
}

/// 临时区的本地失败：空间不足单独成类并带上需要保留的字节数，其余按端口错误带出。
fn local_error(error: PortError, needed: u64) -> DownloadError {
    match error {
        PortError::Local {
            kind: crate::ports::LocalErrorKind::NoSpace,
            ..
        } => DownloadError::NoSpace { needed },
        other => DownloadError::Port(other),
    }
}

fn stream_error(error: StreamError) -> DownloadError {
    match error {
        StreamError::Port(error) => local_error(error, 0),
        StreamError::ShortRead {
            offset,
            expected,
            actual,
        } => DownloadError::Port(PortError::local(
            crate::ports::LocalErrorKind::Read,
            format!("校验时在偏移 {offset} 请求 {expected} 字节，实际读到 {actual} 字节"),
        )),
        StreamError::Cancelled => DownloadError::Cancelled,
    }
}

/// 把临时文件端口适配成流式摘要的读取入口。
struct TempReader<'a>(&'a dyn TempContent);

impl ChunkReader for TempReader<'_> {
    fn read_chunk<'a>(
        &'a self,
        offset: u64,
        length: u64,
    ) -> PortFuture<'a, Result<Vec<u8>, PortError>> {
        self.0.read_at(offset, length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concurrency::CancelToken;
    use crate::fakes::{MemoryContent, MemoryProgress, MemoryTemp};
    use crate::hash::digest_of;

    fn content(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 233) as u8).collect()
    }

    fn pin_of(binding: &str, content: &[u8]) -> VersionPin {
        VersionPin::new(
            binding,
            "entry-1",
            "version-1",
            digest_of(content),
            content.len() as u64,
        )
        .expect("固定版本")
    }

    fn key_of(pin: &VersionPin) -> DownloadKey {
        DownloadKey::new(&pin.binding, &pin.entry, &pin.version).expect("进度键")
    }

    fn options(chunk_size: u64) -> DownloadOptions {
        DownloadOptions::new(chunk_size).with_part_concurrency(2)
    }

    #[tokio::test]
    async fn downloads_every_part_and_verifies_the_whole_digest() {
        let bytes = content(10);
        let pin = pin_of("binding-1", &bytes);
        let content_port = MemoryContent::new(bytes.clone());
        let temp = MemoryTemp::new("temp/entry-1");
        let progress = MemoryProgress::new();

        let outcome = run(&content_port, &temp, &progress, &pin, &options(4))
            .await
            .expect("下载成功");
        assert_eq!(outcome.digest, digest_of(&bytes));
        assert_eq!(outcome.size, 10);
        assert_eq!(outcome.part_count, 3);
        assert_eq!(outcome.downloaded_parts, 3);
        assert!(!outcome.resumed);
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.temp_path, "temp/entry-1");
        assert_eq!(temp.contents(), bytes);
        // 固定 VersionId：所有请求都带同一个版本。
        assert!(
            content_port
                .requested_versions()
                .iter()
                .all(|(_, version)| version == "version-1")
        );
        // 进度已完整，且块几何与临时路径都被记录。
        let stored = progress.get(&key_of(&pin)).expect("进度已保存");
        assert!(stored.bitmap().is_complete());
        assert_eq!(stored.temp_path(), "temp/entry-1");
    }

    #[tokio::test]
    async fn a_disconnect_only_downloads_the_missing_parts() {
        let bytes = content(20);
        let pin = pin_of("binding-1", &bytes);
        let content_port = MemoryContent::new(bytes.clone());
        content_port.fail_once_at(8);
        let temp = MemoryTemp::new("temp/entry-1");
        let progress = MemoryProgress::new();
        let options_ = options(4);

        let failure = run(&content_port, &temp, &progress, &pin, &options_)
            .await
            .expect_err("块 2 断线");
        assert!(failure.is_retryable(), "{failure}");
        let written: Vec<u64> = temp
            .writes()
            .into_iter()
            .map(|(offset, _)| offset)
            .collect();
        assert!(!written.is_empty(), "已完成块已写入临时文件");
        let reads_before = content_port.reads().len();

        let outcome = run(&content_port, &temp, &progress, &pin, &options_)
            .await
            .expect("续传成功");
        assert!(outcome.resumed);
        let later = &content_port.reads()[reads_before..];
        for (offset, _) in later {
            assert!(
                !written.contains(offset),
                "已写入的块 {offset} 不该重新下载"
            );
        }
        let all_offsets: Vec<u64> = {
            let mut offsets: Vec<u64> = temp
                .writes()
                .into_iter()
                .map(|(offset, _)| offset)
                .collect();
            offsets.sort_unstable();
            offsets.dedup();
            offsets
        };
        assert_eq!(all_offsets, vec![0, 4, 8, 12, 16]);
        assert_eq!(temp.contents(), bytes);
    }

    #[tokio::test]
    async fn a_corrupted_part_is_downloaded_again_and_never_installed_as_is() {
        let bytes = content(8);
        let pin = pin_of("binding-1", &bytes);
        let content_port = MemoryContent::new(bytes.clone());
        content_port.corrupt_once_at(4);
        let temp = MemoryTemp::new("temp/entry-1");
        let progress = MemoryProgress::new();

        let outcome = run(&content_port, &temp, &progress, &pin, &options(4))
            .await
            .expect("第二次下载通过校验");
        assert_eq!(outcome.attempts, 2, "整份摘要不符要重下");
        assert_eq!(temp.contents(), bytes);
        assert!(progress.discard_count() >= 1, "坏的位图必须作废");
        assert!(temp.discards() >= 1, "坏内容不能留在临时区");
        assert_eq!(outcome.digest, pin.digest);
    }

    #[tokio::test]
    async fn content_that_stays_wrong_fails_without_publishing() {
        let bytes = content(8);
        let pin = pin_of("binding-1", &bytes);
        let content_port = MemoryContent::new(bytes.clone());
        content_port.corrupt_always_at(4);
        let temp = MemoryTemp::new("temp/entry-1");
        let progress = MemoryProgress::new();

        let error = run(&content_port, &temp, &progress, &pin, &options(4))
            .await
            .expect_err("持续不符必须失败");
        match error {
            DownloadError::ContentMismatch { expected, actual } => {
                assert_eq!(expected, pin.digest);
                assert_ne!(actual, pin.digest);
            }
            other => panic!("期望 ContentMismatch，实际 {other}"),
        }
        // 没有“已安装”这回事：临时内容与位图都被清掉，没有留下可被误当完整的进度。
        assert!(temp.discards() >= 1);
        assert!(progress.get(&key_of(&pin)).is_none());
    }

    #[tokio::test]
    async fn a_different_version_of_the_same_size_never_succeeds() {
        let pinned = content(8);
        let other = vec![0xee_u8; 8];
        let pin = pin_of("binding-1", &pinned);
        let content_port = MemoryContent::new(other);
        let temp = MemoryTemp::new("temp/entry-1");
        let progress = MemoryProgress::new();

        let error = run(&content_port, &temp, &progress, &pin, &options(4))
            .await
            .expect_err("另一个版本的内容不得发布");
        assert!(matches!(error, DownloadError::ContentMismatch { .. }));
        assert!(
            content_port
                .requested_versions()
                .iter()
                .all(|(entry, version)| entry == "entry-1" && version == "version-1"),
            "请求必须绑在固定 VersionId 上"
        );
        assert!(progress.get(&key_of(&pin)).is_none());
    }

    #[tokio::test]
    async fn progress_with_another_block_geometry_is_discarded() {
        let bytes = content(10);
        let pin = pin_of("binding-1", &bytes);
        let content_port = MemoryContent::new(bytes.clone());
        let temp = MemoryTemp::new("temp/entry-1");
        let progress = MemoryProgress::new();

        // 旧进度用 8 字节块记录过一块；这次用 4 字节块，旧位图不能套用。
        let old_geometry = ChunkGeometry::new(10, 8).expect("旧几何");
        let mut old_bitmap = PartBitmap::empty(old_geometry);
        old_bitmap.mark(0).expect("旧块 0");
        progress.seed(
            key_of(&pin),
            StoredProgress::new(8, 10, "temp/entry-1", old_bitmap).expect("旧进度"),
        );

        let outcome = run(&content_port, &temp, &progress, &pin, &options(4))
            .await
            .expect("下载成功");
        assert!(!outcome.resumed, "旧位图作废后不算续传");
        assert_eq!(outcome.downloaded_parts, 3);
        assert_eq!(progress.discard_count(), 1);
        let mut offsets = content_port.read_offsets();
        offsets.sort_unstable();
        assert_eq!(offsets, vec![0, 4, 8]);
    }

    #[tokio::test]
    async fn no_space_fails_before_writing_anything() {
        let bytes = content(10);
        let pin = pin_of("binding-1", &bytes);
        let content_port = MemoryContent::new(bytes);
        let temp = MemoryTemp::new("temp/entry-1").without_capacity();
        let progress = MemoryProgress::new();

        let error = run(&content_port, &temp, &progress, &pin, &options(4))
            .await
            .expect_err("空间不足");
        match error {
            DownloadError::NoSpace { needed } => assert_eq!(needed, 10),
            other => panic!("期望 NoSpace，实际 {other}"),
        }
        assert!(content_port.reads().is_empty(), "空间不足不发请求");
        assert!(temp.writes().is_empty());
        assert_eq!(temp.discards(), 0, "不删除既有内容");
        assert_eq!(progress.discard_count(), 0);
    }

    #[tokio::test]
    async fn cancelling_keeps_the_parts_already_on_disk() {
        let bytes = content(12);
        let pin = pin_of("binding-1", &bytes);
        let content_port = MemoryContent::new(bytes.clone());
        let temp = MemoryTemp::new("temp/entry-1");
        let progress = MemoryProgress::new();
        let cancel = CancelToken::new();
        temp.cancel_after_write(cancel.clone());

        let error = run(
            &content_port,
            &temp,
            &progress,
            &pin,
            &DownloadOptions::new(4)
                .with_part_concurrency(1)
                .with_cancel(cancel),
        )
        .await
        .expect_err("取消不是成功");
        assert!(error.is_cancelled(), "{error}");
        let stored = progress.get(&key_of(&pin)).expect("已下载块已落进度");
        assert_eq!(stored.bitmap().count(), 1);
        let reads_before = content_port.reads().len();

        let outcome = run(&content_port, &temp, &progress, &pin, &options(4))
            .await
            .expect("续传成功");
        assert!(outcome.resumed);
        assert_eq!(outcome.downloaded_parts, 2, "只补剩下两块");
        assert_eq!(content_port.reads().len() - reads_before, 2);
        assert_eq!(temp.contents(), bytes);
    }

    #[tokio::test]
    async fn the_whole_file_digest_is_verified_in_bounded_windows() {
        let bytes = content(40);
        let pin = pin_of("binding-1", &bytes);
        let content_port = MemoryContent::new(bytes.clone());
        let temp = MemoryTemp::new("temp/entry-1");
        let progress = MemoryProgress::new();

        run(&content_port, &temp, &progress, &pin, &options(4))
            .await
            .expect("下载成功");
        let windows: Vec<(u64, u64)> = temp.read_windows();
        assert!(!windows.is_empty());
        assert!(
            windows.iter().all(|(_, length)| *length <= 4),
            "校验必须按块流式读取：{windows:?}"
        );
        let total: u64 = windows.iter().map(|(_, length)| *length).sum();
        assert_eq!(total, 40);
    }

    #[tokio::test]
    async fn a_completed_progress_skips_the_network() {
        let bytes = content(10);
        let pin = pin_of("binding-1", &bytes);
        let content_port = MemoryContent::new(bytes.clone());
        let temp = MemoryTemp::new("temp/entry-1");
        temp.seed_bytes(bytes.clone());
        let progress = MemoryProgress::new();
        let geometry = ChunkGeometry::new(10, 4).expect("几何");
        let mut bitmap = PartBitmap::empty(geometry);
        for index in 0..3 {
            bitmap.mark(index).expect("在范围内");
        }
        progress.seed(
            key_of(&pin),
            StoredProgress::new(4, 10, "temp/entry-1", bitmap).expect("完整进度"),
        );

        let outcome = run(&content_port, &temp, &progress, &pin, &options(4))
            .await
            .expect("校验通过");
        assert!(outcome.resumed);
        assert_eq!(outcome.downloaded_parts, 0);
        assert!(content_port.reads().is_empty(), "完整进度不需要再取字节");
        assert_eq!(outcome.digest, digest_of(&bytes));
    }

    #[tokio::test]
    async fn empty_versions_are_valid_and_need_no_part() {
        let pin = pin_of("binding-1", b"");
        let content_port = MemoryContent::new(Vec::new());
        let temp = MemoryTemp::new("temp/entry-1");
        let progress = MemoryProgress::new();

        let outcome = run(&content_port, &temp, &progress, &pin, &options(4))
            .await
            .expect("空内容合法");
        assert_eq!(outcome.size, 0);
        assert_eq!(outcome.part_count, 0);
        assert_eq!(outcome.digest, digest_of(b""));
        assert!(content_port.reads().is_empty());
        assert!(temp.contents().is_empty());
    }

    #[tokio::test]
    async fn downloads_never_exceed_the_configured_part_concurrency() {
        let bytes = content(40);
        let pin = pin_of("binding-1", &bytes);
        let content_port = MemoryContent::new(bytes);
        let temp = MemoryTemp::new("temp/entry-1");
        let progress = MemoryProgress::new();
        run(
            &content_port,
            &temp,
            &progress,
            &pin,
            &DownloadOptions::new(4).with_part_concurrency(2),
        )
        .await
        .expect("下载成功");
        assert_eq!(content_port.peak_in_flight(), 2);
    }

    #[tokio::test]
    async fn invalid_pins_and_options_are_rejected_before_any_request() {
        let bytes = content(4);
        let content_port = MemoryContent::new(bytes.clone());
        let temp = MemoryTemp::new("temp/entry-1");
        let progress = MemoryProgress::new();
        let pin = pin_of("binding-1", &bytes);

        let error = run(
            &content_port,
            &temp,
            &progress,
            &pin,
            &DownloadOptions::new(0),
        )
        .await
        .expect_err("块大小 0");
        assert!(matches!(error, DownloadError::InvalidOptions { .. }));
        assert!(content_port.reads().is_empty());
    }
}
