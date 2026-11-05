//! 普通文件的受限访问、暂存、刷盘与受控替换（design §11.1—11.3、§11.5、§12.2、§16.1、C33、C37）。
//!
//! 模块划分：
//!
//! - [`raw`]（crate 内可见）：`openat2`/`openat`/`renameat2`/`unlinkat`/`fsync`/`fchmod` 与
//!   `FICLONE` 的最小 FFI 声明（常量与布局有 `const` 断言）。
//! - [`restricted`]：受限目录句柄，所有操作按相对名称在句柄之下进行；逃逸、符号链接、`.`/`..`
//!   一律 fail-closed，`openat2` 不可用时回退逐段 `O_NOFOLLOW` 打开并标明机制。
//! - [`identity`]：`statx` 身份/指纹与替换前提的纯逻辑（可单元测试，无 I/O）。
//! - [`hash`]：SHA-256（无第三方依赖），用于校验复制与记录内容对象。
//! - [`clone`]：在目标文件系统内建暂存副本——优先 `FICLONE`；`EXDEV` 走校验复制并暴露原因，
//!   `EIO`/`ENOSPC`/权限错误不退回。
//! - [`publish`]：替换前核对目标身份、`renameat2` 原子替换（新建用 `RENAME_NOREPLACE`）、
//!   文件和目录刷盘、发布后核对；前提不成立时暂停并保留候选。
//!
//! 典型顺序（design §11.2 步骤 6）：`RestrictedDir::open` → `open_read` 源文件 →
//! [`clone::stage_copy`] 到目标目录 → [`publish::publish`]（或采集方向的
//! [`publish::capture_object`]）。跨文件系统不会静默成功：暂存始终建在目标挂载内，`renameat2`
//! 返回 `EXDEV` 时按 [`restricted::RestrictError::CrossDeviceRename`] 报错。
//!
//! 边界：本模块不写数据库、不做意图日志、不识别外部 watcher 事件，也不宣称消除“核对后替换”
//! 与外部非协作写入之间的竞态（design §12.2 / C33）。产品级的 `fs_intent`、Epoch 与恢复流程
//! 属后续卡。

pub mod clone;
pub mod hash;
pub mod identity;
pub mod publish;
mod raw;
pub mod restricted;

pub use clone::{
    CloneMethod, CopyFallback, ReflinkVerdict, StageError, StageFailure, StagedFile,
    classify_reflink_failure, hash_file, stage_copy,
};
pub use identity::{
    ExpectedTarget, FileIdentity, PreconditionOutcome, PreconditionRefusal, check_target,
    parse_identity_spec,
};
pub use publish::{
    OBJECT_FILE_MODE, PublishError, PublishFailure, PublishOutcome, PublishRequest,
    RetainedCandidate, StagedPublish, WORK_FILE_MODE, capture_object, commit_staged, publish,
    render_outcome, stage_for_publish, staging_name,
};
pub use restricted::{
    OpenedObject, ResolveMechanism, ResolveMode, RestrictError, RestrictedDir,
    validate_relative_path,
};
