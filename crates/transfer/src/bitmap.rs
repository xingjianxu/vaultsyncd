//! 续传位图：哪些块已经落地，跨重启后仍然只需要补齐缺失块（design §13.2「续传」）。
//!
//! 位图只有一种字节表示：第 i 位对应第 i 块，低位在前，字节数等于 `ceil(块数 / 8)`。块数之后的
//! 多余 bit 必须为 0，否则同一份进度会有两种写法，写进数据库再读回来就可能把“没有的块”当成
//! “已收到”。因此构造、反序列化与写入前都做同一套校验。
//!
//! 位图不决定“内容是什么”：它只记录客户端已经收到/服务端已经收下的块。上传侧服务端的
//! `upload_parts` 才是权威（design §9.3），位图只用来说明本地还需要补哪几块；下载侧服务端没有
//! 进度，位图是唯一依据，因此整份摘要校验通过后才能安装（design §13.2）。

use crate::chunk::{ChunkGeometry, GeometryError};

/// 一段连续的块索引（含起点，`count` 至少为 1）。
///
/// 用区间而不是逐块列表表达“缺哪些块”：长度最多是“已收块数 + 1”，不随文件大小增长。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartRange {
    /// 区间起点（含）。
    pub first_index: u64,
    /// 区间内的块数。
    pub count: u64,
}

impl PartRange {
    /// 构造；`count` 为 0 时返回 `None`（空区间没有意义，也不该出现在状态里）。
    #[must_use]
    pub const fn new(first_index: u64, count: u64) -> Option<Self> {
        if count == 0 {
            None
        } else {
            Some(Self { first_index, count })
        }
    }

    /// 区间终点（不含）。
    #[must_use]
    pub const fn end_index(self) -> u64 {
        self.first_index.saturating_add(self.count)
    }

    /// 区间内的块索引。
    pub fn indices(self) -> impl Iterator<Item = u64> {
        self.first_index..self.end_index()
    }
}

/// 一份内容已收块的位图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartBitmap {
    geometry: ChunkGeometry,
    bytes: Vec<u8>,
}

impl PartBitmap {
    /// 全部为 0 的位图（一块都没有）。
    #[must_use]
    pub fn empty(geometry: ChunkGeometry) -> Self {
        let length = usize::try_from(geometry.part_count().div_ceil(8)).unwrap_or(0);
        Self {
            geometry,
            bytes: vec![0_u8; length],
        }
    }

    /// 从持久化字节恢复位图，并核对块几何与尾部 bit。
    ///
    /// # Errors
    ///
    /// 长度与块几何不符、块数之后仍有非 0 bit 时返回 [`BitmapError`]：宁可要求重新开始一块，
    /// 也不接受一份含义不明的位图。
    pub fn from_bytes(geometry: ChunkGeometry, bytes: Vec<u8>) -> Result<Self, BitmapError> {
        let expected = usize::try_from(geometry.part_count().div_ceil(8))
            .map_err(|_| BitmapError::Unrepresentable)?;
        if bytes.len() != expected {
            return Err(BitmapError::SizeMismatch {
                expected,
                found: bytes.len(),
            });
        }
        let unused = (8 - geometry.part_count() % 8) % 8;
        if unused > 0 {
            let mask = u8::MAX << (8 - unused);
            if let Some(last) = bytes.last() {
                if last & mask != 0 {
                    return Err(BitmapError::TailNotZero);
                }
            }
        }
        Ok(Self { geometry, bytes })
    }

    /// 块几何。
    #[must_use]
    pub const fn geometry(&self) -> ChunkGeometry {
        self.geometry
    }

    /// 已收块数。
    #[must_use]
    pub fn count(&self) -> u64 {
        self.bytes
            .iter()
            .map(|byte| u64::from(byte.count_ones()))
            .sum()
    }

    /// 是否覆盖全部块（空内容视为完整）。
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.count() == self.geometry.part_count()
    }

    /// 第 `index` 块是否已收；越界索引没有这一块，返回 `false`。
    #[must_use]
    pub fn contains(&self, index: u64) -> bool {
        if index >= self.geometry.part_count() {
            return false;
        }
        let byte = usize::try_from(index / 8).unwrap_or(usize::MAX);
        let bit = (index % 8) as u32;
        self.bytes
            .get(byte)
            .is_some_and(|value| value & (1 << bit) != 0)
    }

    /// 标记第 `index` 块已收；返回它是否从“未收”变成“已收”。
    ///
    /// # Errors
    ///
    /// 索引大于等于块数时返回 [`BitmapError::IndexOutOfRange`]（与“已经收过”区分开）。
    pub fn mark(&mut self, index: u64) -> Result<bool, BitmapError> {
        if index >= self.geometry.part_count() {
            return Err(BitmapError::IndexOutOfRange {
                index,
                parts: self.geometry.part_count(),
            });
        }
        let byte = usize::try_from(index / 8).map_err(|_| BitmapError::Unrepresentable)?;
        let bit = (index % 8) as u32;
        let slot = self
            .bytes
            .get_mut(byte)
            .ok_or(BitmapError::Unrepresentable)?;
        let was_set = *slot & (1 << bit) != 0;
        *slot |= 1 << bit;
        Ok(!was_set)
    }

    /// 缺失块的连续区间（按索引升序）。
    #[must_use]
    pub fn missing(&self) -> Vec<PartRange> {
        let mut ranges = Vec::new();
        let mut run: Option<u64> = None;
        for index in 0..self.geometry.part_count() {
            if self.contains(index) {
                if let Some(first) = run.take() {
                    if let Some(range) = PartRange::new(first, index - first) {
                        ranges.push(range);
                    }
                }
            } else if run.is_none() {
                run = Some(index);
            }
        }
        if let Some(first) = run {
            if let Some(range) = PartRange::new(first, self.geometry.part_count() - first) {
                ranges.push(range);
            }
        }
        ranges
    }

    /// 持久化字节（低位在前）。
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 取出持久化字节。
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// 位图校验失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitmapError {
    /// 字节长度与块几何算出的长度不同。
    SizeMismatch {
        /// 期望字节数。
        expected: usize,
        /// 实际字节数。
        found: usize,
    },
    /// 块数之后仍有非 0 bit。
    TailNotZero,
    /// 索引大于等于块数。
    IndexOutOfRange {
        /// 越界索引。
        index: u64,
        /// 块数。
        parts: u64,
    },
    /// 块数或字节数无法用本机 `usize` 表示。
    Unrepresentable,
}

impl std::fmt::Display for BitmapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SizeMismatch { expected, found } => {
                write!(f, "位图长度不符：期望 {expected} 字节，实际 {found} 字节")
            }
            Self::TailNotZero => f.write_str("位图在块数之后仍有非 0 bit"),
            Self::IndexOutOfRange { index, parts } => {
                write!(f, "块索引 {index} 超出块数 {parts}")
            }
            Self::Unrepresentable => f.write_str("位图规模无法用本机字长表示"),
        }
    }
}

impl std::error::Error for BitmapError {}

impl From<GeometryError> for BitmapError {
    fn from(error: GeometryError) -> Self {
        match error {
            GeometryError::ZeroChunkSize => Self::SizeMismatch {
                expected: 0,
                found: 0,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(total: u64, chunk: u64) -> ChunkGeometry {
        ChunkGeometry::new(total, chunk).expect("块几何")
    }

    #[test]
    fn empty_bitmap_reports_every_part_missing() {
        let geometry = geometry(10, 4);
        let bitmap = PartBitmap::empty(geometry);
        assert_eq!(bitmap.count(), 0);
        assert!(!bitmap.is_complete());
        assert_eq!(bitmap.bytes().len(), 1);
        assert_eq!(
            bitmap.missing(),
            vec![PartRange {
                first_index: 0,
                count: 3
            }]
        );
    }

    #[test]
    fn empty_content_is_complete_without_bytes() {
        let geometry = geometry(0, 4);
        let bitmap = PartBitmap::empty(geometry);
        assert!(bitmap.is_complete());
        assert!(bitmap.bytes().is_empty());
        assert!(bitmap.missing().is_empty());
        let mut bitmap = bitmap;
        assert_eq!(
            bitmap.mark(0),
            Err(BitmapError::IndexOutOfRange { index: 0, parts: 0 })
        );
    }

    #[test]
    fn marking_is_idempotent_and_bounded() {
        let mut bitmap = PartBitmap::empty(geometry(10, 4));
        assert_eq!(bitmap.mark(0), Ok(true));
        assert_eq!(bitmap.mark(0), Ok(false));
        assert_eq!(bitmap.mark(2), Ok(true));
        assert!(bitmap.contains(0));
        assert!(!bitmap.contains(1));
        assert!(bitmap.contains(2));
        assert!(!bitmap.contains(3), "越界索引不是已收块");
        assert_eq!(bitmap.count(), 2);
        assert_eq!(
            bitmap.mark(3),
            Err(BitmapError::IndexOutOfRange { index: 3, parts: 3 })
        );
        assert_eq!(
            bitmap.missing(),
            vec![PartRange {
                first_index: 1,
                count: 1
            }]
        );
    }

    #[test]
    fn byte_encoding_is_low_bit_first_and_round_trips() {
        let mut bitmap = PartBitmap::empty(geometry(20, 4));
        bitmap.mark(0).expect("块 0");
        bitmap.mark(3).expect("块 3");
        assert_eq!(bitmap.bytes(), &[0b0000_1001]);
        let restored =
            PartBitmap::from_bytes(bitmap.geometry(), bitmap.bytes().to_vec()).expect("回读位图");
        assert_eq!(restored, bitmap);
    }

    #[test]
    fn tail_bits_must_be_zero() {
        let geometry = geometry(10, 4);
        // 3 块：第 3..8 位必须是 0，否则它描述的是不存在的块。
        assert_eq!(
            PartBitmap::from_bytes(geometry, vec![0b0000_1000]),
            Err(BitmapError::TailNotZero)
        );
        assert!(PartBitmap::from_bytes(geometry, vec![0b0000_0111]).is_ok());
    }

    #[test]
    fn length_must_match_the_geometry() {
        let geometry = geometry(10, 4);
        assert_eq!(
            PartBitmap::from_bytes(geometry, vec![0, 0]),
            Err(BitmapError::SizeMismatch {
                expected: 1,
                found: 2
            })
        );
        let wide = ChunkGeometry::new(4 * 1024 * 1024 * 8, 4 * 1024 * 1024).expect("宽几何");
        assert_eq!(wide.part_count(), 8);
        assert_eq!(
            PartBitmap::from_bytes(wide, vec![1, 2]),
            Err(BitmapError::SizeMismatch {
                expected: 1,
                found: 2
            })
        );
    }

    #[test]
    fn completed_bitmap_has_no_missing_ranges() {
        let mut bitmap = PartBitmap::empty(geometry(10, 4));
        for index in 0..3 {
            bitmap.mark(index).expect("在范围内");
        }
        assert!(bitmap.is_complete());
        assert!(bitmap.missing().is_empty());
    }

    #[test]
    fn missing_ranges_do_not_merge_across_received_parts() {
        let mut bitmap = PartBitmap::empty(geometry(40, 4));
        bitmap.mark(1).expect("块 1");
        bitmap.mark(2).expect("块 2");
        bitmap.mark(7).expect("块 7");
        assert_eq!(
            bitmap.missing(),
            vec![
                PartRange {
                    first_index: 0,
                    count: 1
                },
                PartRange {
                    first_index: 3,
                    count: 4
                },
                PartRange {
                    first_index: 8,
                    count: 2
                },
            ]
        );
    }

    #[test]
    fn ranges_reject_empty_counts_and_bottom_out() {
        assert_eq!(PartRange::new(3, 0), None);
        let range = PartRange::new(3, 2).expect("非空区间");
        assert_eq!(range.end_index(), 5);
        assert_eq!(range.indices().collect::<Vec<_>>(), vec![3, 4]);
    }
}
