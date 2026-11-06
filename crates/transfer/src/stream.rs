//! 流式读取整份内容并计算 SHA-256（design §13.3「整文件 hash 流式计算，不把整个文件读进内存」）。
//!
//! 上传要在申请会话之前知道整份摘要，下载要在安装之前核对整份摘要。两次都按固定窗口读取：
//! 内存占用只跟窗口大小有关，与文件大小无关；读取位置的来源由 [`ChunkReader`] 决定（本地快照或
//! 临时文件），此处不做任何 I/O。

use crate::concurrency::CancelToken;
use crate::hash::{ContentHasher, DIGEST_BYTES};
use crate::ports::{PortError, PortFuture};

/// 流式摘要的读取入口：一次一段，长度由调用方给定。
pub(crate) trait ChunkReader {
    /// 读取 `[offset, offset + length)`；短读是错误，不补零。
    fn read_chunk<'a>(
        &'a self,
        offset: u64,
        length: u64,
    ) -> PortFuture<'a, Result<Vec<u8>, PortError>>;
}

/// 流式计算失败：读取失败、短读或取消。
#[derive(Debug)]
pub(crate) enum StreamError {
    /// 读取端口失败。
    Port(PortError),
    /// 短读：读到的字节数与请求不符，说明内容在读取期间变化或适配器说谎。
    ShortRead {
        /// 读取位置。
        offset: u64,
        /// 请求的字节数。
        expected: u64,
        /// 实际读到的字节数。
        actual: usize,
    },
    /// 调用方取消。
    Cancelled,
}

/// 按不超过 `window` 字节的片段读完整份内容并返回摘要。
///
/// `window` 为 0 时按 1 字节处理（调用方应当已经校验过块大小，这里只是不让除零发生）。
/// 每读一段都检查取消，因此取消不会等到整份读完才生效。
pub(crate) async fn streamed_digest(
    reader: &dyn ChunkReader,
    total_size: u64,
    window: u64,
    cancel: &CancelToken,
) -> Result<[u8; DIGEST_BYTES], StreamError> {
    let window = window.max(1);
    let mut hasher = ContentHasher::new();
    let mut offset = 0_u64;
    while offset < total_size {
        if cancel.is_cancelled() {
            return Err(StreamError::Cancelled);
        }
        let length = window.min(total_size - offset);
        let bytes = reader
            .read_chunk(offset, length)
            .await
            .map_err(StreamError::Port)?;
        if bytes.len() as u64 != length {
            return Err(StreamError::ShortRead {
                offset,
                expected: length,
                actual: bytes.len(),
            });
        }
        hasher.update(&bytes);
        offset += length;
    }
    Ok(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::to_hex;
    use crate::ports::LocalErrorKind;
    use std::sync::Mutex;

    /// 固定内容的内存读取器：记录每次请求的窗口长度，用来断言“不整份读入”。
    struct MemoryReader {
        bytes: Vec<u8>,
        windows: Mutex<Vec<u64>>,
        short_at: Option<u64>,
    }

    impl ChunkReader for MemoryReader {
        fn read_chunk<'a>(
            &'a self,
            offset: u64,
            length: u64,
        ) -> PortFuture<'a, Result<Vec<u8>, PortError>> {
            Box::pin(async move {
                if self.short_at == Some(offset) {
                    return Ok(Vec::new());
                }
                self.windows.lock().expect("窗口记录").push(length);
                let start = usize::try_from(offset)
                    .map_err(|_| PortError::local(LocalErrorKind::Read, "偏移超出本机字长"))?;
                let end = usize::try_from(offset + length)
                    .map_err(|_| PortError::local(LocalErrorKind::Read, "长度超出本机字长"))?;
                self.bytes
                    .get(start..end)
                    .map(<[u8]>::to_vec)
                    .ok_or_else(|| PortError::local(LocalErrorKind::Read, "越界读取"))
            })
        }
    }

    #[tokio::test]
    async fn digest_matches_one_shot_and_never_reads_beyond_the_window() {
        let content = b"0123456789abcdefghij".to_vec();
        let reader = MemoryReader {
            bytes: content.clone(),
            windows: Mutex::new(Vec::new()),
            short_at: None,
        };
        let cancel = CancelToken::new();
        let digest = streamed_digest(&reader, 20, 6, &cancel)
            .await
            .expect("摘要");
        assert_eq!(digest, crate::hash::digest_of(&content));
        let windows = reader.windows.lock().expect("窗口记录").clone();
        assert_eq!(windows, vec![6, 6, 6, 2]);
        assert!(windows.iter().all(|window| *window <= 6));
    }

    #[tokio::test]
    async fn empty_content_has_the_nist_digest_without_reading() {
        let reader = MemoryReader {
            bytes: Vec::new(),
            windows: Mutex::new(Vec::new()),
            short_at: None,
        };
        let digest = streamed_digest(&reader, 0, 4, &CancelToken::new())
            .await
            .expect("摘要");
        assert_eq!(
            to_hex(&digest),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(reader.windows.lock().expect("窗口记录").is_empty());
    }

    #[tokio::test]
    async fn a_short_read_is_an_error_not_a_short_digest() {
        let reader = MemoryReader {
            bytes: vec![0_u8; 10],
            windows: Mutex::new(Vec::new()),
            short_at: Some(4),
        };
        let error = streamed_digest(&reader, 10, 4, &CancelToken::new())
            .await
            .expect_err("短读必须报错");
        assert!(
            matches!(
                error,
                StreamError::ShortRead {
                    offset: 4,
                    expected: 4,
                    actual: 0
                }
            ),
            "实际：{error:?}"
        );
    }

    #[tokio::test]
    async fn cancellation_stops_before_the_next_window() {
        let reader = MemoryReader {
            bytes: vec![7_u8; 64],
            windows: Mutex::new(Vec::new()),
            short_at: None,
        };
        let cancel = CancelToken::new();
        cancel.cancel();
        let error = streamed_digest(&reader, 64, 8, &cancel)
            .await
            .expect_err("已取消");
        assert!(matches!(error, StreamError::Cancelled));
        assert!(reader.windows.lock().expect("窗口记录").is_empty());
    }
}
