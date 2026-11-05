//! 本机 IPC 消息帧：`FRAME_HEADER_BYTES` 字节的大端长度头加该长度的 UTF-8 JSON 负载。
//!
//! 帧层只认字节边界，不认识命令，也不做平台 I/O：传输实现（S071 的 Windows 命名管道）负责读
//! 管道与校验调用者身份，把读到的任意分块交给 [`FrameDecoder::push`]，再用
//! [`FrameDecoder::next_frame`] 取完整负载交给 `serde_json` 解码成 [`crate::generated::Request`]。
//!
//! 不变式：
//!
//! - 一个帧要么完整产出，要么不产出：截断的负载永远不会作为短帧交给解码器。
//! - 声明长度为 0 或超过 [`MAX_FRAME_BYTES`] 立即报错，不为坏帧扩容。
//! - 缓冲有界：等待中的部分帧最多 `FRAME_HEADER_BYTES + MAX_FRAME_BYTES` 字节，已完整但未取走
//!   的帧最多 [`MAX_QUEUED_FRAMES`] 个；超过任一上限即报错而不是继续吃内存。
//! - 报错后连接不可继续使用：调用方应结束该连接（缓冲区里可能还有未消费的字节），重新建立
//!   连接即可从干净状态开始。

use core::fmt;
use std::collections::VecDeque;

use crate::generated::{FRAME_HEADER_BYTES, MAX_FRAME_BYTES};

/// 长度头固定为 4 字节（大端 `u32`）；契约里的 `x-vaultsyncd.frame.header_bytes` 与生成常量
/// 必须一致，宽度不同就编译失败而不是运行期错位。
const _: () = assert!(
    FRAME_HEADER_BYTES == 4,
    "帧长度头固定为 4 字节，与 api/local-ipc.schema.json 的 x-vaultsyncd.frame.header_bytes 一致"
);

/// 允许积压的完整帧数：调用方必须及时取走，避免一次读入就把内存吃满。
pub const MAX_QUEUED_FRAMES: usize = 4;

/// 单次读入后允许暂存的字节上限：一帧在等、其余已完整但未取走。
const MAX_BUFFERED_BYTES: usize = (MAX_QUEUED_FRAMES + 1) * MAX_FRAME_BYTES + FRAME_HEADER_BYTES;

/// 帧编解码错误。每种都是协议级失败，调用方应结束连接而不是继续猜。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// 长度为 0 的帧：空负载不是合法 JSON。
    EmptyFrame,
    /// 声明长度或待写入负载超过 [`MAX_FRAME_BYTES`]。
    FrameTooLarge {
        /// 声明（或实际）的字节数。
        bytes: usize,
    },
    /// 未消费的帧或未消费的字节超过上限：调用方没有按帧取走数据。
    Backlog,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyFrame => f.write_str("帧长度为 0：空负载不是合法请求"),
            Self::FrameTooLarge { bytes } => {
                write!(f, "帧长度 {bytes} 超过上限 {MAX_FRAME_BYTES}")
            }
            Self::Backlog => write!(
                f,
                "未取走的帧超过 {MAX_QUEUED_FRAMES} 个或未取走的字节超过 {MAX_BUFFERED_BYTES}"
            ),
        }
    }
}

impl core::error::Error for FrameError {}

/// 把一份 JSON 负载编码成完整帧（长度头 + 负载）。
pub fn encode(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.is_empty() {
        return Err(FrameError::EmptyFrame);
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge {
            bytes: payload.len(),
        });
    }
    let length = u32::try_from(payload.len()).map_err(|_| FrameError::FrameTooLarge {
        bytes: payload.len(),
    })?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// 增量帧解码器：接受任意切分的读入块，按帧产出完整负载。
#[derive(Debug, Default)]
pub struct FrameDecoder {
    /// 尚未凑满一帧的字节（含长度头）。
    buffer: Vec<u8>,
    /// 已完整、等待调用方取走的负载。
    ready: VecDeque<Vec<u8>>,
}

impl FrameDecoder {
    /// 新建空解码器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 等待中的部分帧字节数（诊断用）。
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// 已完整但尚未取走的帧数（诊断用）。
    pub fn queued_frames(&self) -> usize {
        self.ready.len()
    }

    /// 追加一段从管道读到的字节；凑满的帧按到达顺序进入队列。
    ///
    /// 空的读入块是合法的（底层读取允许暂时没有新数据），直接忽略。错误返回值表示连接已不可
    /// 继续使用：长度为 0、超过上限、或调用方积压了太多未取走的帧。
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), FrameError> {
        if chunk.is_empty() {
            return Ok(());
        }
        if self.buffer.len() + chunk.len() > MAX_BUFFERED_BYTES {
            return Err(FrameError::Backlog);
        }
        self.buffer.extend_from_slice(chunk);

        while let Some(declared) = self.declared_len() {
            if declared == 0 {
                return Err(FrameError::EmptyFrame);
            }
            if declared > MAX_FRAME_BYTES {
                return Err(FrameError::FrameTooLarge { bytes: declared });
            }
            if self.buffer.len() < FRAME_HEADER_BYTES + declared {
                break;
            }
            if self.ready.len() >= MAX_QUEUED_FRAMES {
                return Err(FrameError::Backlog);
            }
            let start = FRAME_HEADER_BYTES;
            let end = start + declared;
            self.ready.push_back(self.buffer[start..end].to_vec());
            self.buffer.drain(..end);
        }
        Ok(())
    }

    /// 取走下一个完整帧的负载；没有完整帧时返回 `None`（截断的数据不会被当成帧）。
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        self.ready.pop_front()
    }

    /// 读取已声明负载长度；长度头还不完整时返回 `None`。
    fn declared_len(&self) -> Option<usize> {
        if self.buffer.len() < FRAME_HEADER_BYTES {
            return None;
        }
        let mut header = [0u8; 4];
        header.copy_from_slice(&self.buffer[..FRAME_HEADER_BYTES]);
        Some(u32::from_be_bytes(header) as usize)
    }
}
