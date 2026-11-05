//! 固定块几何（design §13.1）：一份不可变快照的总大小与块大小决定块数、每块长度与块边界。
//!
//! 上传与下载两端都必须用同一个定义算块边界，否则“第 3 块是哪几个字节”会不一致，续传就会把
//! 两份内容拼在一起。因此本模块是块几何的唯一权威：`crates/server-store` 的 `uploads` 表读写与
//! `crates/server-core` 的接收判定都通过它取块数/块长度，不再各写一份除法。
//!
//! 不变量：
//!
//! - 块大小必须大于 0；块数为 `ceil(total / chunk)`，空内容（`total = 0`）有 **0 块**，不用
//!   零长度块表达空文件（design §13.1「空文件合法」）。
//! - 每块长度等于块大小，最后一块是其剩余长度；索引大于等于块数时没有这一块（`None`），
//!   而不是返回 0——越界与“空块”必须能区分。
//! - 边界计算用 `checked_*`：`total_size`/`chunk_size` 是 `u64`，乘法溢出不回绕成小偏移。

use std::fmt;

/// 固定块几何：一份快照的总大小与块大小。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkGeometry {
    total_size: u64,
    chunk_size: u64,
}

impl ChunkGeometry {
    /// 由总大小与块大小构造。
    ///
    /// # Errors
    ///
    /// `chunk_size` 为 0 时返回 [`GeometryError::ZeroChunkSize`]（无法定义块边界）。
    pub fn new(total_size: u64, chunk_size: u64) -> Result<Self, GeometryError> {
        if chunk_size == 0 {
            return Err(GeometryError::ZeroChunkSize);
        }
        Ok(Self {
            total_size,
            chunk_size,
        })
    }

    /// 快照总大小（字节）。
    #[must_use]
    pub const fn total_size(self) -> u64 {
        self.total_size
    }

    /// 块大小（字节）。
    #[must_use]
    pub const fn chunk_size(self) -> u64 {
        self.chunk_size
    }

    /// 块数：完整块与最后一个不足块合计；空内容为 0。
    #[must_use]
    pub fn part_count(self) -> u64 {
        self.total_size.div_ceil(self.chunk_size)
    }

    /// 第 `index` 块的期望长度；索引大于等于块数时返回 `None`。
    #[must_use]
    pub fn part_length(self, index: u64) -> Option<u64> {
        if index >= self.part_count() {
            return None;
        }
        let offset = index.checked_mul(self.chunk_size)?;
        let remaining = self.total_size.checked_sub(offset)?;
        Some(remaining.min(self.chunk_size))
    }
}

/// 块几何构造失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryError {
    /// 块大小为 0。
    ZeroChunkSize,
}

impl fmt::Display for GeometryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroChunkSize => f.write_str("块大小必须大于 0"),
        }
    }
}

impl std::error::Error for GeometryError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_content_has_no_parts() {
        let geometry = ChunkGeometry::new(0, 4).expect("块大小 4");
        assert_eq!(geometry.part_count(), 0);
        assert_eq!(geometry.part_length(0), None);
        assert_eq!(geometry.total_size(), 0);
        assert_eq!(geometry.chunk_size(), 4);
    }

    #[test]
    fn part_lengths_cover_total_without_gaps() {
        let geometry = ChunkGeometry::new(10, 4).expect("块大小 4");
        assert_eq!(geometry.part_count(), 3);
        assert_eq!(geometry.part_length(0), Some(4));
        assert_eq!(geometry.part_length(1), Some(4));
        assert_eq!(geometry.part_length(2), Some(2));
        assert_eq!(geometry.part_length(3), None);
        let sum: u64 = (0..geometry.part_count())
            .filter_map(|index| geometry.part_length(index))
            .sum();
        assert_eq!(sum, 10);
    }

    #[test]
    fn exact_multiple_has_no_short_last_part() {
        let geometry = ChunkGeometry::new(8, 4).expect("块大小 4");
        assert_eq!(geometry.part_count(), 2);
        assert_eq!(geometry.part_length(1), Some(4));
        assert_eq!(geometry.part_length(2), None);
    }

    #[test]
    fn single_part_when_total_is_below_chunk_size() {
        let geometry = ChunkGeometry::new(1, 4 * 1024 * 1024).expect("块大小 4 MiB");
        assert_eq!(geometry.part_count(), 1);
        assert_eq!(geometry.part_length(0), Some(1));
    }

    #[test]
    fn zero_chunk_size_is_rejected() {
        assert_eq!(ChunkGeometry::new(10, 0), Err(GeometryError::ZeroChunkSize));
    }
}
