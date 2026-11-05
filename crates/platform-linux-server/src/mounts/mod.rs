//! 挂载准入（`crates/platform-linux-server/src/mounts/`）。
//!
//! 模块划分：
//!
//! - [`raw`]（私有）：`fstatfs`/`statx`/`ioctl` 的最小 FFI 与三个只读请求（布局有 const 断言）。
//! - [`mountinfo`]：`/proc/self/mountinfo` 解析，纯逻辑。
//! - [`paths`]：按路径分量判断关系，纯逻辑（design §16.1 不用字符串前缀）。
//! - [`probe`]：打开目录句柄、读内核事实、遍历根目录，只读 I/O。
//! - [`admission`]：准入判定与身份比较，纯逻辑。
//! - [`content`]：稀疏与占位的边界（稀疏不是占位证据），纯逻辑。
//!
//! 一次服务的典型调用顺序是 [`probe::probe_path`] → [`probe::scan_root`] →
//! [`admission::evaluate`]；运行期发现挂载变化时用 [`admission::verify_unchanged`] 复核身份，
//! 漂移按 `ROOT_UNAVAILABLE` 处理并暂停 Root，不推导删除（design §11.4—11.5）。

pub mod admission;
pub mod content;
pub mod mountinfo;
pub mod paths;
pub mod probe;
mod raw;

pub use admission::{
    AdmissionDecision, AdmissionInput, AdmissionOutcome, AdmissionRejection, BackendKind,
    InconclusiveReason, StorageIdentity, classify_backend, evaluate, has_nocow_flag, same_storage,
};
pub use mountinfo::{DeviceId, MountEntry};
pub use probe::{ScanPolicy, ScanReport, StorageFacts, probe_path, scan_root};
