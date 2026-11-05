//! 状态目录互斥锁：同一状态目录同一时刻只允许一个“持有者进程”（design §5.10、§17.1）。
//!
//! 两个必须互斥的路径：
//!
//! - **服务端运行中不初始化账号**：`vaultsyncd-admin init` 直接改 `server.sqlite` 的账号表，如果
//!   服务端正在跑，就会绕过“运行中的进程自己决定什么时候写”的前提。init 因此必须先拿到同一把
//!   锁——拿不到就拒绝，而不是“读一下表看有没有用户在跑”。
//! - **同一状态目录不跑两个服务端**：两个进程同时指向一个状态目录时，写入准入只在各自进程内有效
//!   （S037/S041 的限制），有了这把锁就从“部署约定”变成进程可核对的拒绝。
//!
//! 实现是操作系统级的文件锁（`flock` 语义，经 `fs2` 的 `FileExt`），锁随进程退出或文件句柄
//! 关闭自动释放，**不使用 pid 文件**：pid 文件在崩溃后留下陈旧内容，还需要“这个 pid 是不是还
//! 活着/是不是同一个程序”的猜测，猜错就会放行或永久挡住。
//!
//! 锁文件是 `<状态目录>/server.lock`，只放锁，不放状态；内容为空。锁不保护“状态目录以外的东西”，
//! 也不替代数据库自己的短事务与 WAL（两者是不同层次的互斥，见 [`crate::db`]）。

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// 状态目录里的锁文件名。
pub const LOCK_FILE_NAME: &str = "server.lock";

/// 一份被持有的状态目录锁；`Drop` 时释放。
///
/// 句柄必须活到持有者用完为止：文件一关，锁就没有了。
#[derive(Debug)]
pub struct StateLock {
    file: File,
    path: PathBuf,
}

impl StateLock {
    /// 尝试独占状态目录；已被别的进程持有时返回 [`StateLockError::Busy`]。
    ///
    /// 不阻塞等待：调用方是启动路径与 init 命令，两者都需要“现在能不能做”的确定结论。
    ///
    /// # Errors
    ///
    /// - 状态目录不存在、不可写，或锁文件打不开： [`StateLockError::Io`]；
    /// - 锁已被其他进程持有： [`StateLockError::Busy`]。
    pub fn try_acquire_exclusive(state_dir: &Path) -> Result<Self, StateLockError> {
        let path = state_dir.join(LOCK_FILE_NAME);
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| StateLockError::Io {
                path: path.clone(),
                source,
            })?;
        // 必须用 trait 的全路径：Rust 1.89 起 `File` 自己也有 `try_lock`/`unlock`，方法解析会选到标准库
        // 那个（超出 workspace 的 MSRV 1.85）。
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Self { file, path }),
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
                Err(StateLockError::Busy { path })
            }
            Err(source) => Err(StateLockError::Io { path, source }),
        }
    }

    /// 锁文件路径（日志与错误信息用）。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        // 释放失败（例如文件已被关闭）时进程即将退出，锁也会随句柄关闭释放；这里不改写错误处理。
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// 状态目录锁的失败。
#[derive(Debug)]
pub enum StateLockError {
    /// 锁文件打不开或加锁调用失败。
    Io {
        /// 锁文件路径。
        path: PathBuf,
        /// 原始 I/O 错误。
        source: io::Error,
    },
    /// 锁已被其他进程持有。
    Busy {
        /// 锁文件路径。
        path: PathBuf,
    },
}

impl StateLockError {
    /// 稳定错误码（日志、退出码说明与验收脚本共用）。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "state_lock_io",
            Self::Busy { .. } => "state_locked",
        }
    }
}

impl std::fmt::Display for StateLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "无法打开状态目录锁 {}：{source}", path.display())
            }
            Self::Busy { path } => write!(
                f,
                "状态目录已被另一个 vaultsyncd 进程占用（锁文件 {}）",
                path.display()
            ),
        }
    }
}

impl std::error::Error for StateLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Busy { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TestDir;

    #[test]
    fn second_holder_of_the_same_state_dir_is_refused() {
        let dir = TestDir::new("state-lock");
        let held = StateLock::try_acquire_exclusive(&dir.path).expect("第一次加锁");
        assert_eq!(held.path(), dir.path.join(LOCK_FILE_NAME));

        // 同一进程内再开一个句柄也是“另一个持有者”：文件锁按打开的文件描述而不是按线程计。
        match StateLock::try_acquire_exclusive(&dir.path) {
            Err(StateLockError::Busy { path }) => {
                assert_eq!(path, dir.path.join(LOCK_FILE_NAME));
            }
            other => panic!("锁被持有时必须拒绝，实际：{other:?}"),
        }

        drop(held);
        let again = StateLock::try_acquire_exclusive(&dir.path).expect("释放后可以重新加锁");
        assert_eq!(again.path(), dir.path.join(LOCK_FILE_NAME));
    }

    #[test]
    fn missing_state_dir_is_an_io_error_not_a_silent_success() {
        let dir = TestDir::new("state-lock-missing");
        let missing = dir.path.join("does-not-exist");
        match StateLock::try_acquire_exclusive(&missing) {
            Err(StateLockError::Io { path, .. }) => {
                assert_eq!(path, missing.join(LOCK_FILE_NAME));
            }
            other => panic!("状态目录不存在时必须报错，实际：{other:?}"),
        }
    }
}
