//! Linux 内核 ABI 的最小 FFI 声明与只读查询调用。
//!
//! 只声明真正用到的部分：`fstatfs(2)`、`statx(2)`、`ioctl(2)` 与三个只读 ioctl 请求
//! （`FS_IOC_GETFLAGS`、`BTRFS_IOC_FS_INFO`、`BTRFS_IOC_INO_LOOKUP`）。本模块**没有**任何
//! 修改状态的调用：不设置或清除 inode 标志（`FS_IOC_SETFLAGS` 未声明）、不改挂载、不写目录。
//!
//! S014 起 `files/` 复用这里的 [`statx_of_fd`] 取文件身份，因此模块对 crate 内可见
//! （`pub(crate)`）；对外仍是私有实现细节。
//!
//! 结构体逐项对应 `linux/statfs.h`、`linux/statx.h`、`linux/fs.h` 与 `btrfs/ioctl.h` 的
//! 64 位（LP64）布局，并用 `size_of`/`offset_of` 的 `const` 断言自检：布局一旦漂移就是编译
//! 期失败，不会在运行时读到错的字段。

use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::unix::io::RawFd;

/// 本模块的结构体布局按 64 位 Linux 声明；32 位目标的 `statfs` 布局不同，必须显式支持。
const _: () = assert!(size_of::<usize>() == 8);

/// `O_DIRECTORY`：句柄必须是目录，避免把普通文件当挂载根。
// arm64 UAPI keeps ARM's flag assignments for AArch32 compatibility:
// arch/arm64/include/uapi/asm/fcntl.h. The x86 value is O_DIRECT on arm64.
#[cfg(target_arch = "aarch64")]
pub(crate) const O_DIRECTORY: i32 = 0o40_000;
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const O_DIRECTORY: i32 = 0o200_000;
/// `O_NOFOLLOW`：末段是符号链接时失败，不静默跟随。
#[cfg(target_arch = "aarch64")]
pub(crate) const O_NOFOLLOW: i32 = 0o100_000;
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const O_NOFOLLOW: i32 = 0o400_000;
/// `O_PATH`：只取句柄，可用于 `fstatfs`/`statx`，但不能 `ioctl`。
pub(crate) const O_PATH: i32 = 0o10_000_000;

/// `AT_EMPTY_PATH`：让 `statx` 作用于已打开 fd 本身（`pathname` 传空串）。
pub(crate) const AT_EMPTY_PATH: c_int = 0x1000;

/// `STATX_MODE`：需要类型与权限位。
pub(crate) const STATX_MODE: c_uint = 0x2;
/// `STATX_NLINK`：需要硬链接数。
pub(crate) const STATX_NLINK: c_uint = 0x4;
/// `STATX_MTIME`：需要内容修改时间。
pub(crate) const STATX_MTIME: c_uint = 0x40;
/// `STATX_CTIME`：需要元数据变更时间。
pub(crate) const STATX_CTIME: c_uint = 0x80;
/// `STATX_INO`：需要 inode 号。
pub(crate) const STATX_INO: c_uint = 0x100;
/// `STATX_SIZE`：需要逻辑大小。
pub(crate) const STATX_SIZE: c_uint = 0x200;
/// `STATX_BTIME`：需要创建时间（btrfs 上有意义；不可用时由内核在 `stx_mask` 里说明）。
pub(crate) const STATX_BTIME: c_uint = 0x800;
/// `STATX_MNT_ID`：需要挂载 ID（内核 5.8 起；更旧内核会忽略该位并在 `stx_mask` 里缺席）。
pub(crate) const STATX_MNT_ID: c_uint = 0x1000;
/// `STATX_SUBVOL`：需要子卷 ID（内核 6.7 起；旧内核忽略该位，此时只能用 ioctl）。
pub(crate) const STATX_SUBVOL: c_uint = 0x8000;

/// `FS_NOCOW_FL`（`linux/fs.h`）：btrfs 上表示 NOCOW/`+C`，破坏数据 CoW 与校验前提。
pub(crate) const FS_NOCOW_FL: u32 = 0x0080_0000;

/// btrfs 的 ioctl 幻数（`BTRFS_IOCTL_MAGIC`，即 `'\x94'`）。
const BTRFS_IOCTL_MAGIC: u32 = 0x94;
/// `BTRFS_IOCTL_MAGIC` 之外，btrfs 的 `subvol` 相关 ioctl 用的是同一个幻数。
const _: () = assert!(BTRFS_IOCTL_MAGIC == b'\x94' as u32);

/// 按 `_IOC(dir, type, nr, size)` 组合 ioctl 请求号（`asm-generic/ioctl.h`）。
const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> c_ulong {
    (((dir as c_ulong) << 30)
        | ((size as c_ulong) << 16)
        | ((ty as c_ulong) << 8)
        | (nr as c_ulong)) as c_ulong
}

/// `FS_IOC_GETFLAGS` = `_IOR('f', 1, long)`：只读读取 inode 标志。
pub(crate) const FS_IOC_GETFLAGS: c_ulong = ioc(2, b'f' as u32, 1, size_of::<c_long>() as u32);
/// `BTRFS_IOC_FS_INFO` = `_IOR(0x94, 31, struct btrfs_ioctl_fs_info_args)`：读文件系统 FSID。
pub(crate) const BTRFS_IOC_FS_INFO: c_ulong = ioc(2, BTRFS_IOCTL_MAGIC, 31, 1024);
/// `BTRFS_IOC_INO_LOOKUP` = `_IOWR(0x94, 18, struct btrfs_ioctl_ino_lookup_args)`：读子卷 ID。
pub(crate) const BTRFS_IOC_INO_LOOKUP: c_ulong = ioc(3, BTRFS_IOCTL_MAGIC, 18, 4096);

// 自检：三个请求号必须等于 Linux 头文件里的实际取值（`unsigned long` 常量）。
const _: () = assert!(FS_IOC_GETFLAGS == 0x8008_6601);
const _: () = assert!(BTRFS_IOC_FS_INFO == 0x8400_941f);
const _: () = assert!(BTRFS_IOC_INO_LOOKUP == 0xd000_9412);

/// `struct statfs`（LP64）：只读使用 `f_type` 与 `f_fsid`。
#[repr(C)]
#[allow(dead_code, reason = "内核 ABI 布局：未读字段也必须保留，否则偏移全错")]
struct StatFs {
    /// 文件系统类型幻数（`btrfs` = 0x9123683E、`tmpfs` = 0x01021994 等）。
    f_type: i64,
    /// 最优传输块大小。
    f_bsize: i64,
    /// 总块数。
    f_blocks: u64,
    /// 空闲块数。
    f_bfree: u64,
    /// 非特权用户可用块数。
    f_bavail: u64,
    /// 总 inode 数。
    f_files: u64,
    /// 空闲 inode 数。
    f_ffree: u64,
    /// 文件系统标识（两个 32 位字）。
    f_fsid: [i32; 2],
    /// 文件名长度上限。
    f_namelen: i64,
    /// 片段大小。
    f_frsize: i64,
    /// 挂载标志。
    f_flags: i64,
    /// 保留字段。
    f_spare: [i64; 4],
}

const _: () = assert!(size_of::<StatFs>() == 120);
const _: () = assert!(offset_of!(StatFs, f_type) == 0);
const _: () = assert!(offset_of!(StatFs, f_fsid) == 56);

/// `struct statx_timestamp`。
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code, reason = "内核 ABI 布局：未读字段也必须保留，否则偏移全错")]
struct StatxTimestamp {
    /// 秒。
    tv_sec: i64,
    /// 纳秒。
    tv_nsec: u32,
    /// 保留字段。
    reserved: i32,
}

/// `struct statx`：只读使用 `stx_mask`、`stx_ino`、`stx_dev_*`、`stx_mnt_id`、`stx_btime`。
///
/// 6.7 起内核在该结构尾部新增 `stx_subvol`，位置与旧内核的 `__spare3[0]` 重合，因此
/// 256 字节布局对两侧一致；本卡不读该字段（子卷 ID 走 `BTRFS_IOC_INO_LOOKUP`）。
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code, reason = "内核 ABI 布局：未读字段也必须保留，否则偏移全错")]
struct Statx {
    /// 实际填充的字段掩码。
    stx_mask: u32,
    /// 文件系统块大小。
    stx_blksize: u32,
    /// 属性。
    stx_attributes: u64,
    /// 硬链接数。
    stx_nlink: u32,
    /// 属主。
    stx_uid: u32,
    /// 属组。
    stx_gid: u32,
    /// 类型与权限。
    stx_mode: u16,
    /// 保留字段。
    spare0: u16,
    /// inode 号（只在所属子卷内解释）。
    stx_ino: u64,
    /// 逻辑大小。
    stx_size: u64,
    /// 已分配块数（512 B 单位）。
    stx_blocks: u64,
    /// 支持的属性掩码。
    stx_attributes_mask: u64,
    /// 访问时间。
    stx_atime: StatxTimestamp,
    /// 创建时间（btrfs 支持，非所有文件系统都填充）。
    stx_btime: StatxTimestamp,
    /// 元数据变更时间。
    stx_ctime: StatxTimestamp,
    /// 内容修改时间。
    stx_mtime: StatxTimestamp,
    /// 特殊设备主号。
    stx_rdev_major: u32,
    /// 特殊设备次号。
    stx_rdev_minor: u32,
    /// 所在设备主号。
    stx_dev_major: u32,
    /// 所在设备次号。
    stx_dev_minor: u32,
    /// 挂载 ID（`STATX_MNT_ID`）。
    stx_mnt_id: u64,
    /// 直接 I/O 对齐（未使用）。
    stx_dio_mem_align: u32,
    /// 直接 I/O 偏移对齐（未使用）。
    stx_dio_offset_align: u32,
    /// 子卷 ID（`STATX_SUBVOL`，内核 6.7 起）。
    stx_subvol: u64,
    /// 保留字段。
    spare3: [u64; 11],
}

const _: () = assert!(size_of::<Statx>() == 256);
const _: () = assert!(offset_of!(Statx, stx_ino) == 0x20);
const _: () = assert!(offset_of!(Statx, stx_dev_major) == 0x88);
const _: () = assert!(offset_of!(Statx, stx_mnt_id) == 0x90);

impl StatxTimestamp {
    /// 全零时间戳（`statx` 的输出缓冲区初值）。
    const fn zeroed() -> Self {
        Self {
            tv_sec: 0,
            tv_nsec: 0,
            reserved: 0,
        }
    }
}

impl Statx {
    /// 全零 `statx` 输出缓冲区（内核只写它认识的字段）。
    const fn zeroed() -> Self {
        Self {
            stx_mask: 0,
            stx_blksize: 0,
            stx_attributes: 0,
            stx_nlink: 0,
            stx_uid: 0,
            stx_gid: 0,
            stx_mode: 0,
            spare0: 0,
            stx_ino: 0,
            stx_size: 0,
            stx_blocks: 0,
            stx_attributes_mask: 0,
            stx_atime: StatxTimestamp::zeroed(),
            stx_btime: StatxTimestamp::zeroed(),
            stx_ctime: StatxTimestamp::zeroed(),
            stx_mtime: StatxTimestamp::zeroed(),
            stx_rdev_major: 0,
            stx_rdev_minor: 0,
            stx_dev_major: 0,
            stx_dev_minor: 0,
            stx_mnt_id: 0,
            stx_dio_mem_align: 0,
            stx_dio_offset_align: 0,
            stx_subvol: 0,
            spare3: [0; 11],
        }
    }
}

/// `struct btrfs_ioctl_fs_info_args`：只用 `fsid`（文件系统 UUID）。
#[repr(C)]
#[allow(dead_code, reason = "内核 ABI 布局：未读字段也必须保留，否则偏移全错")]
struct BtrfsIoFsInfo {
    /// 最大子卷 ID。
    max_id: u64,
    /// 设备数。
    num_devices: u64,
    /// 文件系统 UUID（16 字节）。
    fsid: [u8; 16],
    /// 节点大小。
    nodesize: u32,
    /// 扇区大小。
    sectorsize: u32,
    /// 克隆对齐。
    clone_alignment: u32,
    /// 特性标志。
    flags: u32,
    /// 保留字段（补齐到 1 KiB）。
    reserved: [u64; 122],
}

const _: () = assert!(size_of::<BtrfsIoFsInfo>() == 1024);

/// `BTRFS_INO_LOOKUP_PATH_MAX`。
const INO_LOOKUP_PATH_MAX: usize = 4080;

/// `struct btrfs_ioctl_ino_lookup_args`：`treeid`/`objectid` 传 0 时返回句柄所在子卷的 ID。
#[repr(C)]
#[allow(dead_code, reason = "内核 ABI 布局：未读字段也必须保留，否则偏移全错")]
struct BtrfsIoInoLookup {
    /// 子卷 ID（输出）。
    treeid: u64,
    /// inode 号（输出）。
    objectid: u64,
    /// 子卷路径名（本卡不使用）。
    name: [u8; INO_LOOKUP_PATH_MAX],
}

const _: () = assert!(size_of::<BtrfsIoInoLookup>() == 4096);

unsafe extern "C" {
    /// `fstatfs(2)`：取已打开 fd 所在文件系统的 `statfs` 信息。
    fn fstatfs(fd: c_int, buf: *mut StatFs) -> c_int;
    /// `statx(2)`：带 `AT_EMPTY_PATH` 时查询 fd 本身。
    fn statx(
        dirfd: c_int,
        pathname: *const c_char,
        flags: c_int,
        mask: c_uint,
        statxbuf: *mut Statx,
    ) -> c_int;
    /// `ioctl(2)`：四个只读请求的调用出口。
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

/// 一次 `fstatfs` 结果中本卡使用的字段。
pub(crate) struct StatFsFacts {
    /// 文件系统类型幻数。
    pub f_type: i64,
    /// `statfs` 自己的文件系统标识（只作辅助证据，btrfs 上随子卷变化）。
    pub statfs_fsid: [i32; 2],
    /// 非特权调用者可用字节（饱和计算）。
    pub available_bytes: u64,
}

/// 一次 `statx(AT_EMPTY_PATH)` 结果中本 crate 使用的字段。
pub(crate) struct StatxFacts {
    /// inode 号。
    pub inode: u64,
    /// 所在设备主号（`major:minor`，与 `/proc/self/mountinfo` 的设备字段可直接比较）。
    pub dev_major: u32,
    /// 所在设备次号。
    pub dev_minor: u32,
    /// 类型与权限位（`stx_mode`）。
    pub mode: u16,
    /// 硬链接数（多链接的可变文件并发覆写语义不一致，见 design §16.2）。
    pub nlink: u32,
    /// 逻辑大小（字节）。
    pub size: u64,
    /// 内容修改时间（秒、纳秒）。
    pub mtime: (i64, u32),
    /// 元数据变更时间（秒、纳秒）。
    pub ctime: (i64, u32),
    /// 挂载 ID；内核未提供该字段时为 `None`。
    pub mnt_id: Option<u64>,
    /// 创建时间（秒、纳秒）；文件系统未提供时为 `None`。
    pub btime: Option<(i64, u32)>,
    /// 子卷 ID（`STATX_SUBVOL`）；内核或文件系统不支持时为 `None`。
    pub subvol_id: Option<u64>,
}

/// 取已打开 fd 的 `fstatfs` 类型幻数与 `statfs` 标识。
pub(crate) fn statfs_of_fd(fd: RawFd) -> io::Result<StatFsFacts> {
    let mut buf = StatFs {
        f_type: 0,
        f_bsize: 0,
        f_blocks: 0,
        f_bfree: 0,
        f_bavail: 0,
        f_files: 0,
        f_ffree: 0,
        f_fsid: [0; 2],
        f_namelen: 0,
        f_frsize: 0,
        f_flags: 0,
        f_spare: [0; 4],
    };
    // SAFETY: buf 是本地可变 StatFs，fd 由调用方持有的有效句柄传入；fstatfs 只写 buf。
    let rc = unsafe { fstatfs(fd, &raw mut buf) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(StatFsFacts {
        f_type: buf.f_type,
        statfs_fsid: buf.f_fsid,
        available_bytes: buf.f_bavail.saturating_mul(buf.f_bsize.max(0) as u64),
    })
}

/// 查询 fd 本身的 `statx` 事实（`AT_EMPTY_PATH`，不跟随路径上的符号链接）。
pub(crate) fn statx_of_fd(fd: RawFd) -> io::Result<StatxFacts> {
    let mut buf = Statx::zeroed();
    // SAFETY: 空 C 字符串指向静态 NUL；AT_EMPTY_PATH 让内核忽略路径改用 fd；
    // buf 是本地可变 Statx。
    let rc = unsafe {
        statx(
            fd,
            c"".as_ptr(),
            AT_EMPTY_PATH,
            STATX_MODE
                | STATX_NLINK
                | STATX_MTIME
                | STATX_CTIME
                | STATX_INO
                | STATX_SIZE
                | STATX_BTIME
                | STATX_MNT_ID
                | STATX_SUBVOL,
            &raw mut buf,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let mnt_id = if buf.stx_mask & STATX_MNT_ID != 0 {
        Some(buf.stx_mnt_id)
    } else {
        None
    };
    let btime = if buf.stx_mask & STATX_BTIME != 0 {
        Some((buf.stx_btime.tv_sec, buf.stx_btime.tv_nsec))
    } else {
        None
    };
    let subvol_id = if buf.stx_mask & STATX_SUBVOL != 0 {
        Some(buf.stx_subvol)
    } else {
        None
    };
    Ok(StatxFacts {
        inode: buf.stx_ino,
        dev_major: buf.stx_dev_major,
        dev_minor: buf.stx_dev_minor,
        mode: buf.stx_mode,
        nlink: buf.stx_nlink,
        size: buf.stx_size,
        mtime: (buf.stx_mtime.tv_sec, buf.stx_mtime.tv_nsec),
        ctime: (buf.stx_ctime.tv_sec, buf.stx_ctime.tv_nsec),
        mnt_id,
        btime,
        subvol_id,
    })
}

/// 读取 inode 标志（`FS_IOC_GETFLAGS`），只读。
///
/// 需要可读句柄（`O_PATH` 句柄上调用 ioctl 会返回 `EBADF`）。文件系统不支持该 ioctl 时
/// 返回原始 `ENOTTY`/`EOPNOTSUPP`，由调用方决定如何呈现，不在这里吞掉。
pub(crate) fn inode_flags(fd: RawFd) -> io::Result<u32> {
    let mut flags: c_long = 0;
    // SAFETY: FS_IOC_GETFLAGS 是 _IOR('f', 1, long)，只写 8 字节 c_long 到本地变量。
    let rc = unsafe { ioctl(fd, FS_IOC_GETFLAGS, &raw mut flags) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    u32::try_from(flags & 0xffff_ffff).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "FS_IOC_GETFLAGS 返回了超出 32 位的标志",
        )
    })
}

/// 读取 btrfs 文件系统 UUID（`BTRFS_IOC_FS_INFO` 的 `fsid` 字段），只读。
pub(crate) fn btrfs_fsid(fd: RawFd) -> io::Result<[u8; 16]> {
    let mut args = BtrfsIoFsInfo {
        max_id: 0,
        num_devices: 0,
        fsid: [0; 16],
        nodesize: 0,
        sectorsize: 0,
        clone_alignment: 0,
        flags: 0,
        reserved: [0; 122],
    };
    // SAFETY: BTRFS_IOC_FS_INFO 是 _IOR(0x94, 31, 1024)，只写本地 1 KiB 结构体。
    let rc = unsafe { ioctl(fd, BTRFS_IOC_FS_INFO, &raw mut args) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(args.fsid)
}

/// 读取 fd 所在 btrfs 子卷的 ID（`BTRFS_IOC_INO_LOOKUP`，`treeid`/`objectid` 传 0），只读。
pub(crate) fn btrfs_subvolume_id(fd: RawFd) -> io::Result<u64> {
    let mut args = BtrfsIoInoLookup {
        treeid: 0,
        objectid: 0,
        name: [0; INO_LOOKUP_PATH_MAX],
    };
    // SAFETY: BTRFS_IOC_INO_LOOKUP 是 _IOWR(0x94, 18, 4096)，读写本地 4 KiB 结构体；
    // treeid/objectid 传 0 时内核用句柄所在子卷与 inode 填充。
    let rc = unsafe { ioctl(fd, BTRFS_IOC_INO_LOOKUP, &raw mut args) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(args.treeid)
}

/// 判断标志里是否含 NOCOW（`FS_NOCOW_FL`）。
pub(crate) fn has_nocow(flags: u32) -> bool {
    flags & FS_NOCOW_FL != 0
}

/// 把 16 字节 btrfs FSID 渲染成与 `btrfs filesystem show` 一致的 UUID 文本。
pub(crate) fn fsid_to_string(fsid: &[u8; 16]) -> String {
    let hex: String = fsid.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

const _: () = assert!(size_of::<StatxTimestamp>() == 16);
const _: () = assert!(offset_of!(BtrfsIoInoLookup, name) == 16);

/// 请求号与标志位的自检（常量被误改时测试直接失败）。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_requests_match_linux_headers() {
        assert_eq!(FS_IOC_GETFLAGS, 0x8008_6601);
        assert_eq!(BTRFS_IOC_FS_INFO, 0x8400_941f);
        assert_eq!(BTRFS_IOC_INO_LOOKUP, 0xd000_9412);
        assert_eq!(O_PATH, 0o10_000_000);
        assert_eq!(AT_EMPTY_PATH, 0x1000);
        assert_eq!(STATX_SUBVOL, 0x8000);
        assert_eq!(STATX_MODE, 0x2);
        assert_eq!(STATX_NLINK, 0x4);
        assert_eq!(STATX_MTIME, 0x40);
        assert_eq!(STATX_CTIME, 0x80);
        assert_eq!(STATX_SIZE, 0x200);
    }

    #[test]
    fn fsid_renders_like_btrfs_uuid() {
        let mut fsid = [0u8; 16];
        for (index, byte) in fsid.iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap() + 1;
        }
        assert_eq!(
            fsid_to_string(&fsid),
            "01020304-0506-0708-090a-0b0c0d0e0f10"
        );
    }

    #[test]
    fn nocow_flag_detection() {
        assert!(has_nocow(FS_NOCOW_FL));
        assert!(has_nocow(FS_NOCOW_FL | 0x0000_0008));
        assert!(!has_nocow(0));
        assert!(!has_nocow(0x0000_0008));
    }
}
