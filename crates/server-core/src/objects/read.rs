//! 读取不可变内容对象（design §11.2 步骤 3、I12、§13 固定版本下载）。
//!
//! 读取走内容地址，读多少、读到的字节必须是哪份内容都由元数据决定：长度与 SHA-256 任一不符
//! 都返回错误，不把截断或改写后的字节当成“这个版本”。

use crate::objects::error::{ReadFailure, ReadFailureReason};
use crate::objects::{ContentDigest, ObjectLayout};
use crate::ports::object_store::ObjectBlobPort;

/// 一次成功读取的对象字节。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectBytes {
    /// 内容地址（请求值）。
    pub digest: ContentDigest,
    /// 对象字节。
    pub data: Vec<u8>,
}

impl ObjectBytes {
    /// 字节数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// 是否为空内容（空文件也是合法对象）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// 按内容地址读取对象并核对长度与 SHA-256。
///
/// `expected_bytes` 来自 `objects.size`：它是“这份内容应该是多长”的唯一依据，不用实际读到的
/// 长度反推元数据。
///
/// # Errors
///
/// 见 [`ReadFailure`]：对象缺失/读取失败（端口错误）、长度不符、摘要不符。
pub fn read_object(
    port: &mut dyn ObjectBlobPort,
    layout: &ObjectLayout,
    digest: ContentDigest,
    expected_bytes: u64,
) -> Result<ObjectBytes, ReadFailure> {
    let object = layout.object_path(&digest);
    let stored = port
        .read_object(&object)
        .map_err(|error| ReadFailure::new(ReadFailureReason::Port(error)))?;

    if stored.bytes != expected_bytes || stored.data.len() as u64 != stored.bytes {
        return Err(ReadFailure::new(ReadFailureReason::LengthMismatch {
            expected: expected_bytes,
            observed: stored.bytes,
        }));
    }
    if stored.digest != digest {
        return Err(ReadFailure::new(ReadFailureReason::DigestMismatch {
            expected: digest,
            observed: stored.digest,
        }));
    }
    Ok(ObjectBytes {
        digest,
        data: stored.data,
    })
}
