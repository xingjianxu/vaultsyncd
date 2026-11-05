//! vaultsyncd 的 Linux 服务端平台适配层（`docs/design.md` §5.4 的 `files/`、`mounts/`、
//! `watcher/`、`ipc/`；无 FUSE）。
//!
//! 本卡（S013）只落地 [`mounts`] 中的挂载准入原型：打开目录句柄后按 `fstatfs(2)` 的
//! `f_type`、`statx(2)` 的 `STATX_MNT_ID`、`/proc/self/mountinfo` 的挂载记录，以及三个只读
//! btrfs/通用 ioctl 判定后端、文件系统身份（FSID）、子卷 ID 与 NOCOW 标志，输出结构化的
//! 接受/拒绝结论。`files/`（S014 起）、`watcher/`、`ipc/` 尚未实现，这里没有占位实现。
//!
//! 边界（design §11.4—11.5、§16.1）：
//!
//! - 准入以打开目录句柄上的内核事实为准，不用路径字符串前缀推断后端；容器内路径本身不是
//!   后端类型证明，检查发生在服务进程自己的 mount namespace（读本进程的
//!   `/proc/self/mountinfo`）。
//! - 只读：本包没有任何设置 inode 标志、清理 NOCOW、改变挂载或自动转换目录的调用。
//! - 平台 I/O 只在本层出现；判定规则（后端分类、路径关系、身份比较、内容形态）是不做 I/O
//!   的纯函数，可在任意平台 `cargo test` 验证。
//! - 普通稀疏文件不自动等于客户端占位/按需回源文件；第三方 stub 不存在可依赖的统一判定，
//!   详见 [`mounts::content`] 与 `docs/architecture/interfaces.md`。
//!
//! 本包不在 workspace `default-members` 中：它只在 Linux 上编译，便携检查不构建它。

#[cfg(target_os = "linux")]
pub mod mounts;
