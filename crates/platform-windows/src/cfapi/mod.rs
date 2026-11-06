//! Cloud Files API（CFAPI）适配层。
//!
//! 模块划分：
//!
//! - [`identity`]：占位符 `FileIdentity` 的编解码与校验，纯逻辑。
//! - [`prototype_layout`]：原型固定同步根/条目集合，纯逻辑。
//! - [`content`]：固定测试内容源（确定性字节 + SHA-256 + 范围校验），纯逻辑。
//! - [`lifecycle`]：FFI 生命周期记录（`stage` + 参数摘要 + `HRESULT`）与阶段错误类型。
//! - [`guard`]：释放前的「已观察 generation + 使用保护」判定规则，纯逻辑。
//! - `raw`（仅 Windows）：`cfapi.h`/`fileapi.h`/`kernel32` 的最小 FFI 声明。
//! - `prototype`（仅 Windows）：注册 → 连接 → 创建占位 → 查询 → 断开 → 注销的实际调用序列。
//! - `provider`（仅 Windows）：`FETCH_DATA`/`CANCEL_FETCH_DATA` 回调、分段回填、超时/离线与
//!   读取/取消验证。
//! - `release`（仅 Windows）：使用保护探测、内容代际核对与 `CfDehydratePlaceholder` 释放。

pub mod content;
pub mod guard;
pub mod identity;
pub mod lifecycle;
pub mod prototype_layout;

#[cfg(windows)]
pub mod prototype;
#[cfg(windows)]
pub mod provider;
#[cfg(windows)]
pub mod raw;
#[cfg(windows)]
pub mod release;

pub use identity::{EntryId, FileIdentity, IdentityDecodeError, RootId, VersionHint};
pub use lifecycle::{HResult, StageError};

#[cfg(windows)]
pub mod adapter;
pub mod catalog;
