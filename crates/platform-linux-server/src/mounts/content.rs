//! 内容形态：普通稀疏文件**不等同**于客户端占位/按需回源文件。
//!
//! design §11.4 明确要求：不能仅因磁盘分配块数小于逻辑大小就判定为云文件或占位。btrfs 上
//! 稀疏是常态——空洞、预分配、压缩、CoW 共享 extent 都会让“已分配字节”小于逻辑大小，
//! 这些文件仍然是普通本地文件，字节可以完整读取。
//!
//! 反向边界同样存在且本模块**无法**自动识别：第三方 daemon 把本地目录当作占位/回源视图
//! （例如把远端对象映射成普通文件），Linux 没有可依赖的统一判定方式。存储准入因此只依赖
//! 允许的后端、已知标志和部署契约；`SERVER_PLACEHOLDER_UNSUPPORTED` 留给部署契约显式声明
//! 的占位目录，绝不由本模块的稀疏度推断。是否“能读到完整字节”由内容版本确认流程验证
//! （design §11.4 的“内容版本确认”一行），不属于本卡的挂载准入。

/// 逻辑大小与已分配字节的关系。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentShape {
    /// 逻辑大小为 0 且没有分配字节。
    Empty,
    /// 逻辑大小大于 0，但分配字节少于逻辑大小（稀疏）。
    Sparse {
        /// 已分配字节数。
        allocated_bytes: u64,
        /// 未分配（空洞）字节数。
        unallocated_bytes: u64,
    },
    /// 分配字节不少于逻辑大小（普通文件，或预分配使分配大于逻辑）。
    FullyAllocated {
        /// 已分配字节数。
        allocated_bytes: u64,
    },
}

impl ContentShape {
    /// 稳定输出用的短名（`empty`、`sparse`、`fully_allocated`）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ContentShape::Empty => "empty",
            ContentShape::Sparse { .. } => "sparse",
            ContentShape::FullyAllocated { .. } => "fully_allocated",
        }
    }
}

/// 按逻辑大小与已分配字节分类内容形态（纯函数）。
///
/// 预分配会让 `allocated_bytes` 大于 `logical_size`，此时同样归为
/// [`ContentShape::FullyAllocated`]：两种情况都不是占位证据。
#[must_use]
pub fn classify_content_shape(logical_size: u64, allocated_bytes: u64) -> ContentShape {
    if logical_size == 0 && allocated_bytes == 0 {
        ContentShape::Empty
    } else if allocated_bytes < logical_size {
        ContentShape::Sparse {
            allocated_bytes,
            unallocated_bytes: logical_size - allocated_bytes,
        }
    } else {
        ContentShape::FullyAllocated { allocated_bytes }
    }
}

/// 占位证据的结论文本。S013 的探针只能给出这一句：稀疏本身不是证据。
pub const PLACEHOLDER_EVIDENCE_NONE: &str = "none";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_is_not_treated_as_placeholder_evidence() {
        assert_eq!(classify_content_shape(0, 0), ContentShape::Empty);
        assert_eq!(
            classify_content_shape(8 * 1024 * 1024, 0),
            ContentShape::Sparse {
                allocated_bytes: 0,
                unallocated_bytes: 8 * 1024 * 1024
            }
        );
        assert_eq!(
            classify_content_shape(100, 4096),
            ContentShape::FullyAllocated {
                allocated_bytes: 4096
            }
        );
        assert_eq!(
            ContentShape::Sparse {
                allocated_bytes: 0,
                unallocated_bytes: 1
            }
            .as_str(),
            "sparse"
        );
        assert_eq!(PLACEHOLDER_EVIDENCE_NONE, "none");
    }
}
