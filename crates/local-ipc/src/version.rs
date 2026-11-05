//! 本机 IPC 协议版本与兼容判定（`api/local-ipc.schema.json` 的 `x-vaultsyncd.protocol`）。
//!
//! 版本号是契约的一部分：schema 里声明、生成成常量，本模块只做判定，不另外维护一份数字。
//!
//! 规则：
//!
//! - `major` 不同即不兼容。帧格式与命令语义的不兼容变更只通过 major 表达，不做“尽量兼容”的
//!   猜测。
//! - `minor` 高于 agent 支持的版本同样拒绝：调用方可能假设本 agent 尚未实现的命令存在，静默
//!   接受会把“命令不存在”拖成运行期失败。
//! - `minor` 更低是兼容的：旧 GUI/CLI 只使用它认识的命令与字段。

use crate::generated::{IpcVersion, PROTOCOL_MAJOR, PROTOCOL_MINOR};

/// agent 支持的协议 major 版本。
pub const SUPPORTED_MAJOR: u32 = PROTOCOL_MAJOR;
/// agent 支持的协议 minor 版本。
pub const SUPPORTED_MINOR: u32 = PROTOCOL_MINOR;

/// agent 支持的协议版本，用于 `get_status` 的应答。
pub fn supported() -> IpcVersion {
    IpcVersion {
        major: SUPPORTED_MAJOR,
        minor: SUPPORTED_MINOR,
    }
}

/// 调用方声明的版本是否可被本 agent 处理。
///
/// 用 [`supported`] 的字段参与比较，而不是把当前常量值（当前 minor 是 0）写进比较式：规则要随
/// 契约版本演进，不能因为本轮 minor 恰好是 0 就退化成“必须等于 0”。
pub fn is_compatible(version: &IpcVersion) -> bool {
    let supported = supported();
    version.major == supported.major && version.minor <= supported.minor
}
