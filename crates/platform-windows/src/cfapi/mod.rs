//! Cloud Files API（CFAPI）适配层。
//!
//! 模块划分：
//!
//! - [`identity`]：占位符 `FileIdentity` 的编解码与校验，纯逻辑。
//! - [`prototype_layout`]：S010 原型的固定同步根/条目集合，纯逻辑。
//! - [`lifecycle`]：FFI 生命周期记录（`stage` + 参数摘要 + `HRESULT`）与阶段错误类型。
//! - `raw`（仅 Windows）：`cfapi.h`/`fileapi.h` 的最小 FFI 声明。
//! - `prototype`（仅 Windows）：注册 → 连接 → 创建占位 → 查询 → 断开 → 注销的实际调用序列。

pub mod identity;
pub mod lifecycle;
pub mod prototype_layout;

#[cfg(windows)]
pub mod prototype;
#[cfg(windows)]
pub mod raw;

pub use identity::{EntryId, FileIdentity, IdentityDecodeError, RootId, VersionHint};
pub use lifecycle::{HResult, StageError};
