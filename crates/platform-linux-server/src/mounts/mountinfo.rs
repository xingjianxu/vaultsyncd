//! `/proc/self/mountinfo` 解析（`proc(5)` 的 mountinfo 格式，只读）。
//!
//! 读的是**本进程**的 mountinfo，因此检查发生在服务进程实际的 mount namespace 内——
//! 容器里的路径字符串不构成后端类型证明（design §11.4）。
//!
//! 解析出的记录提供三件准入必需的事实：挂载 ID（与 `statx` 的 `STATX_MNT_ID` 对齐）、
//! 设备 `major:minor`（与 `statx`/`statfs` 对齐）、以及 `fstype` 与挂载选项。路径只用于
//! 判断“某个挂载点位于 Root 之下”这类关系，且一律按路径分量比较，不用字符串前缀。

use std::fmt;
use std::path::{Path, PathBuf};

/// `/proc/self/mountinfo` 的路径。
pub const MOUNTINFO_PATH: &str = "/proc/self/mountinfo";

/// 设备标识 `major:minor`（与 `statx` 的 `stx_dev_major`/`stx_dev_minor` 同源）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId {
    /// 主设备号。
    pub major: u32,
    /// 次设备号。
    pub minor: u32,
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.major, self.minor)
    }
}

/// 一条 mountinfo 记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    /// 挂载 ID（`statx` 的 `STATX_MNT_ID` 与之可直接比较）。
    pub mount_id: u64,
    /// 父挂载 ID。
    pub parent_id: u64,
    /// 设备 `major:minor`。
    pub device: DeviceId,
    /// 挂载点在文件系统内的根（btrfs 上体现子卷路径，如 `/@data`）。
    pub root: PathBuf,
    /// 挂载点绝对路径。
    pub mount_point: PathBuf,
    /// 每挂载选项（逗号分隔，未含超级块选项）。
    pub mount_options: Vec<String>,
    /// 可选字段（`shared:N`、`master:N`、`propagate_from:N`、`unbindable`）。
    pub optional_fields: Vec<String>,
    /// 文件系统类型名（`btrfs`、`tmpfs`、`overlay`、`fuse.sshfs` 等）。
    pub fs_type: String,
    /// 挂载源（设备或 `tmpfs`、`overlay` 之类的伪源）。
    pub source: String,
    /// 超级块选项。
    pub super_options: Vec<String>,
}

impl MountEntry {
    /// 该挂载是否带指定选项（同时匹配每挂载选项与超级块选项）。
    ///
    /// `nodatacow`/`nodatasum` 只可能作为 btrfs 的每挂载或超级块选项出现，两侧都要看。
    pub fn has_option(&self, name: &str) -> bool {
        self.mount_options.iter().any(|option| option == name)
            || self.super_options.iter().any(|option| option == name)
    }
}

/// mountinfo 解析失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountInfoParseError {
    /// 1 基行号。
    pub line: usize,
    /// 失败字段或阶段。
    pub field: &'static str,
    /// 原始问题描述。
    pub detail: String,
}

impl fmt::Display for MountInfoParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "mountinfo 第 {} 行 {} 字段无法解析：{}",
            self.line, self.field, self.detail
        )
    }
}

impl std::error::Error for MountInfoParseError {}

/// 读取并解析本进程的 `/proc/self/mountinfo`。
///
/// # Errors
///
/// 文件不可读时返回原始 I/O 错误；格式不符时返回 [`MountInfoParseError`]（解析错误转成
/// `InvalidData`，保留行号与字段）。
pub fn read_self() -> std::io::Result<Vec<MountEntry>> {
    let text = std::fs::read_to_string(MOUNTINFO_PATH)?;
    parse(&text).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// 解析整份 mountinfo 文本。
///
/// # Errors
///
/// 任意一行字段数不足、数字非法或缺少 ` - ` 分隔符时返回该行的
/// [`MountInfoParseError`]；不跳过坏行（跳过会让“挂载点集合不完整”这类问题静默通过）。
pub fn parse(text: &str) -> Result<Vec<MountEntry>, MountInfoParseError> {
    let mut entries = Vec::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        if raw_line.trim().is_empty() {
            continue;
        }
        entries.push(parse_line(raw_line, line_number)?);
    }
    Ok(entries)
}

fn parse_line(raw_line: &str, line: usize) -> Result<MountEntry, MountInfoParseError> {
    let fields: Vec<&str> = raw_line.split(' ').filter(|f| !f.is_empty()).collect();
    let separator = fields
        .iter()
        .position(|field| *field == "-")
        .ok_or_else(|| MountInfoParseError {
            line,
            field: "separator",
            detail: "缺少与超级块字段之间的 ` - ` 分隔符".to_owned(),
        })?;
    if separator < 6 {
        return Err(MountInfoParseError {
            line,
            field: "prefix",
            detail: format!("` - ` 之前只有 {separator} 个字段，至少需要 6 个"),
        });
    }
    let super_fields = &fields[separator + 1..];
    if super_fields.len() < 3 {
        return Err(MountInfoParseError {
            line,
            field: "super",
            detail: "` - ` 之后需要 fstype、源与超级块选项三个字段".to_owned(),
        });
    }

    let mount_id = parse_u64(fields[0], line, "mount_id")?;
    let parent_id = parse_u64(fields[1], line, "parent_id")?;
    let device = parse_device(fields[2], line)?;
    let root = PathBuf::from(unescape(fields[3]));
    let mount_point = PathBuf::from(unescape(fields[4]));
    let mount_options = split_options(fields[5]);
    let optional_fields: Vec<String> = fields[6..separator]
        .iter()
        .map(|field| (*field).to_owned())
        .collect();
    let fs_type = unescape(super_fields[0]);
    let source = unescape(super_fields[1]);
    let super_options = split_options(super_fields[2]);

    Ok(MountEntry {
        mount_id,
        parent_id,
        device,
        root,
        mount_point,
        mount_options,
        optional_fields,
        fs_type,
        source,
        super_options,
    })
}

fn parse_u64(value: &str, line: usize, field: &'static str) -> Result<u64, MountInfoParseError> {
    value.parse::<u64>().map_err(|error| MountInfoParseError {
        line,
        field,
        detail: format!("'{value}' 不是无符号整数：{error}"),
    })
}

fn parse_device(value: &str, line: usize) -> Result<DeviceId, MountInfoParseError> {
    let (major, minor) = value.split_once(':').ok_or_else(|| MountInfoParseError {
        line,
        field: "device",
        detail: format!("'{value}' 不是 major:minor 形式"),
    })?;
    Ok(DeviceId {
        major: major.parse::<u32>().map_err(|error| MountInfoParseError {
            line,
            field: "device.major",
            detail: format!("'{major}' 不是整数：{error}"),
        })?,
        minor: minor.parse::<u32>().map_err(|error| MountInfoParseError {
            line,
            field: "device.minor",
            detail: format!("'{minor}' 不是整数：{error}"),
        })?,
    })
}

fn split_options(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter(|option| !option.is_empty())
        .map(unescape)
        .collect()
}

/// 还原 mountinfo 的八进制转义（空格 `\040`、制表 `\011`、换行 `\012`、反斜杠 `\134`）。
///
/// 非法转义原样保留：mountinfo 只对这四个字节转义，其它 `\` 属于路径内容本身。
fn unescape(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let octal = &bytes[index + 1..index + 4];
            if octal.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
                let code = (octal[0] - b'0') * 64 + (octal[1] - b'0') * 8 + (octal[2] - b'0');
                out.push(code);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 在挂载表中按挂载 ID 查找记录。
#[must_use]
pub fn find_by_mount_id(entries: &[MountEntry], mount_id: u64) -> Option<&MountEntry> {
    entries.iter().find(|entry| entry.mount_id == mount_id)
}

/// 在挂载表中按设备号与挂载根查找记录（`statx` 未提供 `STATX_MNT_ID` 时的兜底依据）。
#[must_use]
pub fn find_by_device_and_root<'a>(
    entries: &'a [MountEntry],
    device: DeviceId,
    root: &Path,
    mount_point: &Path,
) -> Option<&'a MountEntry> {
    entries.iter().find(|entry| {
        entry.device == device && entry.root == root && entry.mount_point == mount_point
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
22 1 8:1 / / rw,relatime shared:1 - btrfs /dev/sdc1 rw,ssd,space_cache=v2,subvolid=256,subvol=/@
36 22 0:32 / /tmp rw,nosuid,nodev shared:9 - tmpfs tmpfs rw,size=4096k,nr_inodes=1048576
41 22 0:36 / /run/credentials/x rw,nosuid master:3 - tmpfs tmpfs rw
42 22 0:40 / /data/vaultsyncd-lab/runs/s013/with\\040space rw,relatime unbindable - btrfs /dev/sdc1 rw,subvolid=2506,subvol=/@data
";

    #[test]
    fn parses_records_with_and_without_optional_fields() {
        let entries = parse(SAMPLE).expect("sample 应可解析");
        assert_eq!(entries.len(), 4);

        let root = &entries[0];
        assert_eq!(root.mount_id, 22);
        assert_eq!(root.parent_id, 1);
        assert_eq!(root.device, DeviceId { major: 8, minor: 1 });
        assert_eq!(root.root, PathBuf::from("/"));
        assert_eq!(root.mount_point, PathBuf::from("/"));
        assert_eq!(root.fs_type, "btrfs");
        assert_eq!(root.source, "/dev/sdc1");
        assert!(root.mount_options.contains(&"rw".to_owned()));
        assert!(root.optional_fields.contains(&"shared:1".to_owned()));
        assert_eq!(root.super_options[0], "rw");
        assert!(root.has_option("subvolid=256"), "超级块选项应可查询");

        let credentials = &entries[2];
        assert_eq!(credentials.mount_point, PathBuf::from("/run/credentials/x"));
        assert_eq!(credentials.optional_fields, vec!["master:3".to_owned()]);

        let unbindable = &entries[3];
        assert_eq!(
            unbindable.mount_point,
            PathBuf::from("/data/vaultsyncd-lab/runs/s013/with space")
        );
        assert_eq!(unbindable.optional_fields, vec!["unbindable".to_owned()]);
        assert_eq!(unbindable.root, PathBuf::from("/"));
        assert!(!unbindable.has_option("nodatacow"));
    }

    #[test]
    fn rejects_truncated_lines_instead_of_skipping_them() {
        let broken = parse("1 1 8:1 / / rw - btrfs /dev/sdc1\n");
        let error = broken.expect_err("缺少超级块选项必须报错");
        assert_eq!(error.line, 1);
        assert_eq!(error.field, "super");

        let no_separator = parse("1 1 8:1 / / rw,relatime\n");
        let error = no_separator.expect_err("缺少 ` - ` 必须报错");
        assert_eq!(error.field, "separator");

        let bad_device = parse("1 1 eight-one / / rw - btrfs /dev/sdc1 rw\n");
        let error = bad_device.expect_err("设备号非法必须报错");
        assert_eq!(error.field, "device");
    }

    #[test]
    fn unescapes_only_octal_path_escapes() {
        assert_eq!(unescape("with\\040space"), "with space");
        assert_eq!(unescape("tab\\011here"), "tab\there");
        assert_eq!(unescape("back\\134slash"), "back\\slash");
        assert_eq!(unescape("keep\\x41"), "keep\\x41");
        assert_eq!(unescape("trailing\\04"), "trailing\\04");
    }

    #[test]
    fn finds_entries_by_mount_id_and_device() {
        let entries = parse(SAMPLE).expect("sample 应可解析");
        assert_eq!(
            find_by_mount_id(&entries, 36).map(|entry| entry.fs_type.as_str()),
            Some("tmpfs")
        );
        assert!(find_by_mount_id(&entries, 999).is_none());
        assert_eq!(
            find_by_device_and_root(
                &entries,
                entries[1].device,
                Path::new("/"),
                Path::new("/tmp")
            )
            .map(|entry| entry.mount_id),
            Some(36)
        );
    }
}
