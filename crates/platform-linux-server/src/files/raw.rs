//! `files/` 需要的内核 ABI 最小声明：`openat2`、`openat`、`renameat2`、`unlinkat`、
//! `fsync`、`fchmod` 与 `FICLONE` ioctl。
//!
//! 与 `mounts/raw.rs` 分开：那里只有只读查询，这里首次出现**写**系统调用（创建、重命名、
//! 删除、刷盘、改权限）。仍然只声明本卡实际用到的符号，没有 `libc` 依赖。
//!
//! 所有结构体与常量用 `const` 断言自检布局/取值；一旦内核 ABI 假设漂移就是编译期失败。

use std::ffi::{CStr, c_int, c_long, c_uint, c_ulong};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

const _: () = assert!(size_of::<usize>() == 8);

/// `O_CLOEXEC`：句柄不泄漏到子进程。
pub(crate) const O_CLOEXEC: i32 = 0o2000000;
/// `O_CREAT`：暂存文件必须新建（配合 `O_EXCL` 拒绝已存在）。
pub(crate) const O_CREAT: i32 = 0o100;
/// `O_EXCL`：`O_CREAT` 时目标已存在即失败，不静默复用别人的文件。
pub(crate) const O_EXCL: i32 = 0o200;
/// `O_RDWR`：暂存文件既要写内容，也要读回校验。
pub(crate) const O_RDWR: i32 = 0o2;

// `openat2(2)` 的 `resolve` 位（`linux/openat2.h`）。
/// 拒绝穿过 `/proc/*/fd` 之类的魔法链接。
pub(crate) const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
/// 拒绝路径中任何一段是符号链接。
pub(crate) const RESOLVE_NO_SYMLINKS: u64 = 0x04;
/// 拒绝解析结果跳出起始目录（逃逸返回 `EXDEV`，见 man 2 openat2）。
pub(crate) const RESOLVE_BENEATH: u64 = 0x08;

/// 本 crate 统一使用的解析约束：不跟随符号链接、不逃出根目录句柄。
pub(crate) const RESOLVE_STRICT: u64 =
    RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH;

/// `RENAME_NOREPLACE`（`linux/fs.h`）：目标已存在时让 `renameat2` 失败而不是覆盖。
pub(crate) const RENAME_NOREPLACE: c_uint = 1;

/// errno 取值（`asm-generic/errno*.h`，这些值在受支持的架构上一致）。
pub(crate) mod errno {
    /// 跨文件系统链接（`renameat2`/`FICLONE` 的真实边界）。
    pub(crate) const EXDEV: i32 = 18;
    /// 目标已存在（`RENAME_NOREPLACE`、`O_EXCL`）。
    pub(crate) const EEXIST: i32 = 17;
    /// 符号链接环（`O_NOFOLLOW` 命中符号链接时的典型返回）。
    pub(crate) const ELOOP: i32 = 40;
    /// 不存在。
    pub(crate) const ENOENT: i32 = 2;
    /// 内核不支持该系统调用（`openat2` 早于 5.6）。
    pub(crate) const ENOSYS: i32 = 38;
    /// 文件系统不支持该 ioctl（reflink 不可用）。
    pub(crate) const EOPNOTSUPP: i32 = 95;
    /// 不是 tty：文件系统不认识该 ioctl。
    pub(crate) const ENOTTY: i32 = 25;
    /// 参数非法（`FICLONE` 对不支持 reflink 的文件系统也用它）。
    pub(crate) const EINVAL: i32 = 22;
}

/// `struct open_how`（`linux/openat2.h`）：24 字节、8 字节对齐。
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

const _: () = assert!(size_of::<OpenHow>() == 24);
const _: () = assert!(offset_of!(OpenHow, mode) == 8);
const _: () = assert!(offset_of!(OpenHow, resolve) == 16);

/// `__NR_openat2`：所有受支持 64 位架构上都是 437（`asm-generic/unistd.h`）。
const SYS_OPENAT2: c_long = 437;
const _: () = assert!(SYS_OPENAT2 == 437);

/// 内核符号声明集中在一个私有模块里，避免与下面的安全包装函数重名。
mod abi {
    use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong};

    unsafe extern "C" {
        /// `syscall(2)`：`openat2` 没有稳定的 glibc 包装（2.43 才加上），用通用入口更保守。
        pub(super) fn syscall(number: c_long, ...) -> c_long;
        /// `openat(2)`：逐段 `O_NOFOLLOW` 打开的回退路径。
        pub(super) fn openat(
            dirfd: c_int,
            pathname: *const c_char,
            flags: c_int,
            mode: c_uint,
        ) -> c_int;
        /// `renameat2(2)`：带 `RENAME_NOREPLACE` 的原子重命名（glibc 2.28 起有包装）。
        pub(super) fn renameat2(
            olddirfd: c_int,
            oldpath: *const c_char,
            newdirfd: c_int,
            newpath: *const c_char,
            flags: c_uint,
        ) -> c_int;
        /// `unlinkat(2)`：删除相对目录句柄的文件。
        pub(super) fn unlinkat(dirfd: c_int, pathname: *const c_char, flags: c_int) -> c_int;
        /// `fsync(2)`：文件与目录刷盘。
        pub(super) fn fsync(fd: c_int) -> c_int;
        /// `fchmod(2)`：设置发布后的文件权限（对象为只读）。
        pub(super) fn fchmod(fd: c_int, mode: c_uint) -> c_int;
        /// `ioctl(2)`：`FICLONE` 的调用出口。
        pub(super) fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }
}

/// 按 `_IOC(dir, type, nr, size)` 组合 ioctl 请求号（`asm-generic/ioctl.h`）。
const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> c_ulong {
    ((dir as c_ulong) << 30) | ((size as c_ulong) << 16) | ((ty as c_ulong) << 8) | (nr as c_ulong)
}

/// `FICLONE` = `_IOW(0x94, 9, int)`：把源文件的内容 extent 克隆到目标文件。
pub(crate) const FICLONE: c_ulong = ioc(1, 0x94, 9, size_of::<c_int>() as u32);

// 自检：请求号必须等于 Linux 头文件里的实际取值。
const _: () = assert!(FICLONE == 0x4004_9409);
const _: () = assert!(RESOLVE_STRICT == 0x0e);

fn take_fd(returned: c_int) -> io::Result<OwnedFd> {
    if returned < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 内核返回非负 fd，交给 OwnedFd 负责关闭；同一个 fd 只封装一次。
    Ok(unsafe { OwnedFd::from_raw_fd(returned) })
}

/// `openat2(2)`：在 `dirfd` 之下按 `resolve` 约束打开 `path`。
pub(crate) fn openat2(
    dirfd: RawFd,
    path: &CStr,
    flags: i32,
    mode: u32,
    resolve: u64,
) -> io::Result<OwnedFd> {
    let how = OpenHow {
        flags: (flags | O_CLOEXEC) as u64,
        mode: u64::from(mode),
        resolve,
    };
    // SAFETY: path 是 NUL 结尾的 C 字符串；how 是本地只读 OpenHow；syscall 第 4 个参数给出
    // 结构体大小，内核据此判断 ABI 版本。
    let returned = unsafe {
        abi::syscall(
            SYS_OPENAT2,
            dirfd,
            path.as_ptr(),
            &raw const how,
            size_of::<OpenHow>(),
        )
    };
    let returned = c_int::try_from(returned).unwrap_or(-1);
    take_fd(returned)
}

/// `openat(2)`：`openat2` 不可用时的逐段回退。
pub(crate) fn openat(dirfd: RawFd, path: &CStr, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    // SAFETY: path 是 NUL 结尾的 C 字符串；返回值为 fd 或 -1。
    let returned = unsafe { abi::openat(dirfd, path.as_ptr(), flags | O_CLOEXEC, mode) };
    take_fd(returned)
}

/// `renameat2(2)`：在两个目录句柄之间重命名，`flags` 可为 0（原子替换）或
/// [`RENAME_NOREPLACE`]。
pub(crate) fn renameat2(
    from_dirfd: RawFd,
    from: &CStr,
    to_dirfd: RawFd,
    to: &CStr,
    flags: u32,
) -> io::Result<()> {
    // SAFETY: 两个路径都是 NUL 结尾的 C 字符串，flags 为文档定义的取值。
    let rc = unsafe { abi::renameat2(from_dirfd, from.as_ptr(), to_dirfd, to.as_ptr(), flags) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `unlinkat(2)`：删除目录句柄下的文件（不用于目录）。
pub(crate) fn unlinkat(dirfd: RawFd, path: &CStr) -> io::Result<()> {
    // SAFETY: path 是 NUL 结尾的 C 字符串；flags 传 0 表示删除文件而非目录。
    let rc = unsafe { abi::unlinkat(dirfd, path.as_ptr(), 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `fsync(2)`：文件或目录刷盘。
pub(crate) fn fsync(fd: RawFd) -> io::Result<()> {
    // SAFETY: fd 由调用方的有效句柄传入。
    let rc = unsafe { abi::fsync(fd) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `fchmod(2)`：设置已打开句柄的权限位。
pub(crate) fn fchmod(fd: RawFd, mode: u32) -> io::Result<()> {
    // SAFETY: fd 由调用方的有效句柄传入；mode 是 `mode_t`（u32）。
    let rc = unsafe { abi::fchmod(fd, mode) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `ioctl(FICLONE)`：把 `source` 的内容克隆到 `target`（两者都必须已打开、可写）。
pub(crate) fn ficlone(target: RawFd, source: RawFd) -> io::Result<()> {
    // SAFETY: FICLONE 是 _IOW(0x94, 9, int)，第三个可变参数是源文件 fd。
    let rc = unsafe { abi::ioctl(target, FICLONE, source) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 内核是否根本不支持 `openat2`（早于 5.6）。只有这种情况允许回退到逐段打开；`EPERM`（被
/// seccomp 拦）之类的错误必须原样上报，不能靠回退掩盖。
pub(crate) fn is_openat2_missing(error: &io::Error) -> bool {
    error.raw_os_error() == Some(errno::ENOSYS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_and_resolve_constants_match_headers() {
        assert_eq!(FICLONE, 0x4004_9409);
        assert_eq!(RESOLVE_NO_MAGICLINKS, 0x02);
        assert_eq!(RESOLVE_NO_SYMLINKS, 0x04);
        assert_eq!(RESOLVE_BENEATH, 0x08);
        assert_eq!(RESOLVE_STRICT, 0x0e);
        assert_eq!(RENAME_NOREPLACE, 1);
        assert_eq!(errno::EXDEV, 18);
        assert_eq!(errno::EEXIST, 17);
        assert_eq!(errno::ENOSYS, 38);
        assert_eq!(errno::EOPNOTSUPP, 95);
    }

    #[test]
    fn only_missing_syscall_triggers_fallback() {
        assert!(is_openat2_missing(&io::Error::from_raw_os_error(
            errno::ENOSYS
        )));
        assert!(!is_openat2_missing(&io::Error::from_raw_os_error(1)));
        assert!(!is_openat2_missing(&io::Error::from_raw_os_error(
            errno::EINVAL
        )));
    }
}
