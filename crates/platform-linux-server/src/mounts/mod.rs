//! 挂载准入（`crates/platform-linux-server/src/mounts/`）。
//!
//! 模块划分：
//!
//! - [`raw`]（crate 内可见）：`fstatfs`/`statx`/`ioctl` 的最小 FFI 与三个只读请求（布局有
//!   const 断言）；`files/` 复用其中的 `statx_of_fd` 取文件身份。
//! - [`mountinfo`]：`/proc/self/mountinfo` 解析，纯逻辑。
//! - [`paths`]：按路径分量判断关系，纯逻辑（design §16.1 不用字符串前缀）。
//! - [`overlap`]：Root 与内部目录的父子/别名/bind 重叠判定，纯逻辑（只看路径会漏 bind
//!   mount，只看 inode 会混淆子卷）。
//! - [`probe`]：打开目录句柄、读内核事实、遍历根目录，只读 I/O。
//! - [`admission`]：准入判定与身份比较，纯逻辑。
//! - [`health`]：运行期挂载健康复核（卷身份、根目录身份、子挂载、权限），只读 I/O。
//! - [`content`]：稀疏与占位的边界（稀疏不是占位证据），纯逻辑。
//!
//! 一次服务的典型调用顺序是 [`probe::probe_path`] → [`probe::scan_root`] →
//! [`admission::evaluate`]；运行期发现挂载变化时用 [`admission::verify_unchanged`] 复核身份，
//! 漂移按 `ROOT_UNAVAILABLE` 处理并暂停 Root，不推导删除（design §11.4—11.5）。

pub mod admission;
pub mod content;
pub mod health;
pub mod mountinfo;
pub mod overlap;
pub mod paths;
pub mod probe;
pub(crate) mod raw;

pub use admission::{
    AdmissionDecision, AdmissionInput, AdmissionOutcome, AdmissionRejection, BackendKind,
    InconclusiveReason, StorageIdentity, classify_backend, evaluate, has_nocow_flag, same_storage,
};
pub use health::{MountChange, MountHealth, submounts_under, verify_root_mount};
pub use mountinfo::{DeviceId, MountEntry};
pub use overlap::{DirectoryRecord, DirectoryRole, Overlap, OverlapKind, find_overlaps};
pub use probe::{ScanPolicy, ScanReport, StorageFacts, probe_path, scan_root};
