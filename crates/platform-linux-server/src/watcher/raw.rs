//! `inotify(7)` 的最小 FFI 声明与事件解析。
//!
//! 只声明用到的符号：`inotify_init1`、`inotify_add_watch`、`inotify_rm_watch`、`read`、`poll`。
//! 常量取自 `linux/inotify.h`，用 `const` 断言自检；事件解析是不做 I/O 的纯函数，可单元测试。

use std::ffi::{CStr, c_char, c_int, c_short, c_uint, c_ulong};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

const _: () = assert!(size_of::<usize>() == 8);

/// 属性变化（含权限、时间戳、链接数）。
pub(crate) const IN_ATTRIB: u32 = 0x0000_0004;
/// 可写打开后关闭。
pub(crate) const IN_CLOSE_WRITE: u32 = 0x0000_0008;
/// 内容被写入。
pub(crate) const IN_MODIFY: u32 = 0x0000_0002;
/// 离开被监视目录的改名。
pub(crate) const IN_MOVED_FROM: u32 = 0x0000_0040;
/// 进入被监视目录的改名。
pub(crate) const IN_MOVED_TO: u32 = 0x0000_0080;
/// 在被监视目录内创建。
pub(crate) const IN_CREATE: u32 = 0x0000_0100;
/// 在被监视目录内删除。
pub(crate) const IN_DELETE: u32 = 0x0000_0200;
/// 被监视目录自身被删除。
pub(crate) const IN_DELETE_SELF: u32 = 0x0000_0400;
/// 被监视目录自身被改名/移走。
pub(crate) const IN_MOVE_SELF: u32 = 0x0000_0800;
/// 所在文件系统被卸载。
pub(crate) const IN_UNMOUNT: u32 = 0x0000_2000;
/// 内核事件队列溢出：**已有事件被丢弃**。
pub(crate) const IN_Q_OVERFLOW: u32 = 0x0000_4000;
/// 监视被移除（目录删除、卸载或显式移除）。
pub(crate) const IN_IGNORED: u32 = 0x0000_8000;
/// 只监视目录。
pub(crate) const IN_ONLYDIR: u32 = 0x0100_0000;
/// 事件对象是目录。
pub(crate) const IN_ISDIR: u32 = 0x4000_0000;

/// 本 crate 对每个目录登记的事件集合。
pub(crate) const WATCH_MASK: u32 = IN_ATTRIB
    | IN_CLOSE_WRITE
    | IN_MODIFY
    | IN_MOVED_FROM
    | IN_MOVED_TO
    | IN_CREATE
    | IN_DELETE
    | IN_DELETE_SELF
    | IN_MOVE_SELF
    | IN_ONLYDIR;

const IN_NONBLOCK: c_int = 0o4000;
const IN_CLOEXEC: c_int = 0o2000000;
const POLLIN: c_short = 0x0001;

const _: () = assert!(IN_Q_OVERFLOW == 0x4000 && IN_ISDIR == 0x4000_0000);

/// `errno` 取值。
pub(crate) mod errno {
    /// 非阻塞读取没有数据。
    pub(crate) const EAGAIN: i32 = 11;
    /// 被信号打断。
    pub(crate) const EINTR: i32 = 4;
    /// 没有权限。
    pub(crate) const EACCES: i32 = 13;
    /// 目录不存在（并发删除）。
    pub(crate) const ENOENT: i32 = 2;
    /// 监视数达到 `max_user_watches`（或内核内存不足）。
    pub(crate) const ENOSPC: i32 = 28;
    /// 不是目录（并发被替换）。
    pub(crate) const ENOTDIR: i32 = 20;
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

const _: () = assert!(size_of::<PollFd>() == 8);
const _: () = assert!(offset_of!(PollFd, revents) == 6);

mod abi {
    use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void};

    unsafe extern "C" {
        pub(super) fn inotify_init1(flags: c_int) -> c_int;
        pub(super) fn inotify_add_watch(fd: c_int, path: *const c_char, mask: c_uint) -> c_int;
        pub(super) fn inotify_rm_watch(fd: c_int, wd: c_int) -> c_int;
        pub(super) fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
        pub(super) fn poll(fds: *mut super::PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
    }
}

/// 建立非阻塞、`CLOEXEC` 的 inotify 实例。
pub(crate) fn init() -> io::Result<OwnedFd> {
    // SAFETY: 无指针参数；返回非负 fd 时交给 OwnedFd 管理，只封装一次。
    let fd = unsafe { abi::inotify_init1(IN_NONBLOCK | IN_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd 刚由内核返回且非负。
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// 登记目录监视，返回 watch 描述符。
pub(crate) fn add_watch(fd: RawFd, path: &CStr, mask: u32) -> io::Result<i32> {
    // SAFETY: path 是 NUL 结尾的 C 字符串；fd 为有效 inotify fd。
    let wd = unsafe { abi::inotify_add_watch(fd, path.as_ptr().cast::<c_char>(), mask as c_uint) };
    if wd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(wd)
}

/// 移除监视（目录已消失时内核可能返回 `EINVAL`，调用方忽略）。
pub(crate) fn rm_watch(fd: RawFd, wd: i32) -> io::Result<()> {
    // SAFETY: 两个参数都是整数。
    let rc = unsafe { abi::inotify_rm_watch(fd, wd) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 非阻塞读取事件字节；没有数据返回 `Ok(0)`。
pub(crate) fn read_events(fd: RawFd, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: buffer 在调用期间有效，长度与传入一致。
        let n = unsafe { abi::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(errno::EAGAIN) => return Ok(0),
            Some(errno::EINTR) => {}
            _ => return Err(error),
        }
    }
}

/// 等待 fd 可读，最多 `timeout_ms` 毫秒；返回是否可读。
pub(crate) fn wait_readable(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut pollfd = PollFd {
        fd,
        events: POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: 单个本地 PollFd，nfds 为 1。
        let rc = unsafe { abi::poll(&raw mut pollfd, 1 as c_ulong, timeout_ms) };
        if rc >= 0 {
            return Ok(rc > 0 && pollfd.revents & POLLIN != 0);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(errno::EINTR) {
            return Err(error);
        }
    }
}

/// 一条解析后的内核事件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RawEvent {
    /// watch 描述符（溢出事件为 -1）。
    pub(crate) wd: i32,
    /// 事件掩码。
    pub(crate) mask: u32,
    /// 目录内的名字（目录自身事件为空）。
    pub(crate) name: Vec<u8>,
}

/// 事件头：`wd(4) + mask(4) + cookie(4) + len(4)`。
const HEADER: usize = 16;

/// 事件字节无法解析。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParseError(pub(crate) String);

/// 解析 `read(2)` 返回的缓冲区。不完整的记录是错误，不静默丢弃。
pub(crate) fn parse_events(buffer: &[u8]) -> Result<Vec<RawEvent>, ParseError> {
    let mut events = Vec::new();
    let mut offset = 0usize;
    while offset < buffer.len() {
        if buffer.len() - offset < HEADER {
            return Err(ParseError(format!("事件头不完整（偏移 {offset}）")));
        }
        let field = |at: usize| {
            u32::from_ne_bytes([
                buffer[offset + at],
                buffer[offset + at + 1],
                buffer[offset + at + 2],
                buffer[offset + at + 3],
            ])
        };
        let wd = field(0) as i32;
        let mask = field(4);
        let len = field(12) as usize;
        let end = offset + HEADER + len;
        if end > buffer.len() {
            return Err(ParseError(format!(
                "事件名长度 {len} 超出缓冲区（偏移 {offset}）"
            )));
        }
        let raw_name = &buffer[offset + HEADER..end];
        // 名字以 NUL 补齐到对齐长度。
        let name_len = raw_name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(raw_name.len());
        events.push(RawEvent {
            wd,
            mask,
            name: raw_name[..name_len].to_vec(),
        });
        offset = end;
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(wd: i32, mask: u32, name: &[u8]) -> Vec<u8> {
        let padded = if name.is_empty() {
            0
        } else {
            (name.len() + 1).div_ceil(4) * 4
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&wd.to_ne_bytes());
        bytes.extend_from_slice(&mask.to_ne_bytes());
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        bytes.extend_from_slice(&(padded as u32).to_ne_bytes());
        bytes.extend_from_slice(name);
        bytes.resize(HEADER + padded, 0);
        bytes
    }

    #[test]
    fn parses_named_unnamed_and_overflow_records() {
        let mut buffer = record(3, IN_CREATE, b"a.txt");
        buffer.extend(record(3, IN_ATTRIB, b""));
        buffer.extend(record(-1, IN_Q_OVERFLOW, b""));
        let events = parse_events(&buffer).expect("合法记录");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].name, b"a.txt");
        assert_eq!(events[0].wd, 3);
        assert!(events[1].name.is_empty());
        assert_eq!(events[2].wd, -1);
        assert_eq!(events[2].mask & IN_Q_OVERFLOW, IN_Q_OVERFLOW);
    }

    #[test]
    fn truncated_records_are_errors_not_silently_dropped() {
        let full = record(1, IN_CREATE, b"file");
        assert!(parse_events(&full[..full.len() - 1]).is_err());
        assert!(parse_events(&full[..7]).is_err());
    }
}
