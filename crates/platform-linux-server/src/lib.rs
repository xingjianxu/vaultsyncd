//! vaultsyncd 的 Linux 服务端平台适配层（`docs/design.md` §5.4 的 `files/`、`mounts/`、
//! `watcher/`、`ipc/`；无 FUSE）。
//!
//! 本包目前落地两类原型：
//!
//! - S013：`mounts/` 的挂载准入——打开目录句柄后按 `fstatfs(2)` 的 `f_type`、`statx(2)` 的
//!   `STATX_MNT_ID`、`/proc/self/mountinfo` 的挂载记录，以及三个只读 btrfs/通用 ioctl 判定后端、
//!   文件系统身份（FSID）、子卷 ID 与 NOCOW 标志，输出结构化的接受/拒绝结论。
//! - S014：`files/` 的普通文件发布原型——受限目录句柄访问（`openat2` 或逐段 `O_NOFOLLOW`）、
//!   目标文件系统内暂存、`FICLONE`/校验复制、文件与目录刷盘、受控替换与前提核对。
//!
//! - S050：`watcher/` 的 inotify 目录监视——递归登记、变化线索、队列溢出重建与登记缺口上报。
//!
//! - S075：`ipc/` 的受限 Unix socket、SO_PEERCRED 身份与有界管理帧。
//!
//! 边界（design §11.4—11.5、§12.2、§16.1）：
//!
//! - 准入以打开目录句柄上的内核事实为准，不用路径字符串前缀推断后端；容器内路径本身不是
//!   后端类型证明，检查发生在服务进程自己的 mount namespace（读本进程的
//!   `/proc/self/mountinfo`）。
//! - 运行时以根句柄为边界逐层安全访问：逃逸、符号链接与 `.`/`..` 一律 fail-closed。
//! - 文件发布不宣称消除与外部非协作写入之间的竞态（C33）：只核对已观察到的身份，冲突时暂停
//!   并保留候选，不硬覆盖。
//! - 平台 I/O 只在本层出现；判定规则（后端分类、路径关系、身份比较、内容形态、替换前提）是
//!   不做 I/O 的纯函数，可在任意平台 `cargo test` 验证。
//! - 普通稀疏文件不自动等于客户端占位/按需回源文件；第三方 stub 不存在可依赖的统一判定，
//!   详见 [`mounts::content`] 与 `docs/architecture/interfaces.md`。
//!
//! 本包不在 workspace `default-members` 中：它只在 Linux 上编译，便携检查不构建它。

#[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
compile_error!("vaultsyncd Linux 服务端只支持 x86_64");

#[cfg(target_os = "linux")]
pub mod backup;
#[cfg(target_os = "linux")]
pub mod files;
#[cfg(target_os = "linux")]
pub mod ipc;
#[cfg(target_os = "linux")]
pub mod mounts;
#[cfg(target_os = "linux")]
pub mod watcher;

#[cfg(target_os = "linux")]
pub mod restore;
