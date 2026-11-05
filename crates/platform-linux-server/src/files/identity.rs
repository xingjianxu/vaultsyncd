//! 文件身份与“发布前提”的纯逻辑。
//!
//! 身份来自 `statx`：`(设备主号, 设备次号, inode, btime)` 回答“还是不是同一个文件”，
//! `(size, mtime, ctime)` 回答“这个文件在我们观察之后有没有被写过”。两者合起来是 design
//! §11.2 第 4 步的“工作目录已观察指纹”。
//!
//! 边界（design §12.2 / C33）：指纹只能降低读到中间状态与覆盖别人写入的概率。特权写入者可以
//! 把 mtime/ctime 设回去，inode 也可能被复用；因此本模块从不声称“核对后替换消除了所有竞态”。
//! 需要绝对原子性的场景由数据库事务与 `fs_intent` 恢复协议承担，不由这里承担。

use crate::mounts::raw;
use std::fmt;
use std::io;
use std::os::fd::RawFd;

/// 文件系统对象类型掩码与目录类型（`linux/stat.h`）。
const S_IFMT: u16 = 0o170_000;
const S_IFDIR: u16 = 0o040_000;
const S_IFREG: u16 = 0o100_000;

/// 一个文件（或目录）在某一时刻的身份与指纹。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    /// `statx` 报告的设备主号（btrfs 上每个子卷是独立匿名设备，不能当卷身份用）。
    pub dev_major: u32,
    /// 所在设备次号。
    pub dev_minor: u32,
    /// inode 号（只在所属子卷内解释）。
    pub inode: u64,
    /// 创建时间（秒、纳秒）；文件系统不提供时为 `None`。
    pub btime: Option<(i64, u32)>,
    /// 逻辑大小（字节）。
    pub size: u64,
    /// 内容修改时间（秒、纳秒）。
    pub mtime: (i64, u32),
    /// 元数据变更时间（秒、纳秒）。
    pub ctime: (i64, u32),
    /// 硬链接数。
    pub nlink: u32,
    /// 类型与权限位（`mode_t`）。
    pub mode: u16,
    /// 挂载 ID（内核不提供时为 `None`）。
    pub mount_id: Option<u64>,
}

impl FileIdentity {
    /// 读取已打开句柄的身份（`statx(AT_EMPTY_PATH)`，不跟随路径）。
    ///
    /// # Errors
    ///
    /// `statx(2)` 失败时返回原始 `io::Error`。
    pub fn of_fd(fd: RawFd) -> io::Result<Self> {
        let facts = raw::statx_of_fd(fd)?;
        Ok(Self {
            dev_major: facts.dev_major,
            dev_minor: facts.dev_minor,
            inode: facts.inode,
            btime: facts.btime,
            size: facts.size,
            mtime: facts.mtime,
            ctime: facts.ctime,
            nlink: facts.nlink,
            mode: facts.mode,
            mount_id: facts.mnt_id,
        })
    }

    /// 是否目录。
    #[must_use]
    pub fn is_directory(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    /// 是否普通文件（暂存与发布只处理普通文件）。
    #[must_use]
    pub fn is_regular_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }

    /// 权限位（不含类型位），例如 `0o644`。
    #[must_use]
    pub fn permission_bits(&self) -> u32 {
        u32::from(self.mode & 0o7777)
    }

    /// 是否同一个文件：设备、inode 与创建时间都相同。仅 `inode` 相等不足以判断（可能在别的
    /// 子卷，也可能被复用）。
    #[must_use]
    pub fn same_object(&self, other: &Self) -> bool {
        self.dev_major == other.dev_major
            && self.dev_minor == other.dev_minor
            && self.inode == other.inode
            && self.btime == other.btime
    }

    /// 列出与 `other` 不同的字段名（顺序固定，便于稳定输出）。
    #[must_use]
    pub fn changed_fields(&self, other: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if (self.dev_major, self.dev_minor) != (other.dev_major, other.dev_minor) {
            changed.push("device");
        }
        if self.inode != other.inode {
            changed.push("inode");
        }
        if self.btime != other.btime {
            changed.push("btime");
        }
        if self.size != other.size {
            changed.push("size");
        }
        if self.mtime != other.mtime {
            changed.push("mtime");
        }
        if self.ctime != other.ctime {
            changed.push("ctime");
        }
        if self.nlink != other.nlink {
            changed.push("nlink");
        }
        if self.mode != other.mode {
            changed.push("mode");
        }
        changed
    }

    /// 指纹完全相同（含大小与时间戳）。
    #[must_use]
    pub fn fingerprint_equal(&self, other: &Self) -> bool {
        self.changed_fields(other).is_empty()
    }

    /// 渲染成稳定的 `key=value` 行，供原型命令输出与前提文件复用。
    #[must_use]
    pub fn to_spec(&self) -> String {
        format!(
            "dev_major={}\ndev_minor={}\ninode={}\nbtime={}\nsize={}\nmtime={}\nctime={}\nnlink={}\nmode={:06o}\nmount_id={}\n",
            self.dev_major,
            self.dev_minor,
            self.inode,
            format_timestamp(self.btime),
            self.size,
            format_timestamp(Some(self.mtime)),
            format_timestamp(Some(self.ctime)),
            self.nlink,
            self.mode,
            self.mount_id
                .map_or_else(|| "none".to_owned(), |id| id.to_string()),
        )
    }
}

impl fmt::Display for FileIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "dev={}:{} inode={} btime={} size={} mtime={} ctime={} nlink={} mode={:06o}",
            self.dev_major,
            self.dev_minor,
            self.inode,
            format_timestamp(self.btime),
            self.size,
            format_timestamp(Some(self.mtime)),
            format_timestamp(Some(self.ctime)),
            self.nlink,
            self.mode
        )
    }
}

fn format_timestamp(stamp: Option<(i64, u32)>) -> String {
    match stamp {
        Some((seconds, nanos)) => format!("{seconds}.{nanos:09}"),
        None => "none".to_owned(),
    }
}

fn parse_timestamp(key: &str, value: &str) -> Result<Option<(i64, u32)>, IdentitySpecError> {
    if value == "none" {
        return Ok(None);
    }
    let invalid = || IdentitySpecError::InvalidValue {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    let (seconds, nanos) = value.split_once('.').ok_or_else(invalid)?;
    let seconds = seconds.parse::<i64>().map_err(|_| invalid())?;
    let nanos = nanos.parse::<u32>().map_err(|_| invalid())?;
    if nanos >= 1_000_000_000 {
        return Err(invalid());
    }
    Ok(Some((seconds, nanos)))
}

/// 身份规格解析错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentitySpecError {
    /// 缺少必需字段（前提文件不完整就不能当作“同一个文件”）。
    MissingKey(&'static str),
    /// 字段值无法解析。
    InvalidValue {
        /// 字段名。
        key: String,
        /// 原始取值。
        value: String,
    },
    /// 字段重复出现（避免“后一行覆盖前一行”掩盖手写错误）。
    DuplicateKey(&'static str),
}

impl fmt::Display for IdentitySpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdentitySpecError::MissingKey(key) => write!(f, "身份规格缺少字段 {key}"),
            IdentitySpecError::InvalidValue { key, value } => {
                write!(f, "身份规格字段 {key} 的取值 '{value}' 无法解析")
            }
            IdentitySpecError::DuplicateKey(key) => write!(f, "身份规格字段 {key} 重复出现"),
        }
    }
}

impl std::error::Error for IdentitySpecError {}

/// 解析 [`FileIdentity::to_spec`] 产生的文本（未知字段忽略，缺失字段报错）。
///
/// # Errors
///
/// 缺字段、字段重复或取值无法解析时返回 [`IdentitySpecError`]，调用方必须当作用法错误。
pub fn parse_identity_spec(text: &str) -> Result<FileIdentity, IdentitySpecError> {
    const KEYS: [&str; 10] = [
        "dev_major",
        "dev_minor",
        "inode",
        "btime",
        "size",
        "mtime",
        "ctime",
        "nlink",
        "mode",
        "mount_id",
    ];
    let mut seen: std::collections::BTreeMap<&'static str, String> =
        std::collections::BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if let Some(known) = KEYS.iter().find(|candidate| **candidate == key) {
            if seen.insert(known, value.to_owned()).is_some() {
                return Err(IdentitySpecError::DuplicateKey(known));
            }
        }
    }
    let required = |key: &'static str| -> Result<String, IdentitySpecError> {
        seen.get(key)
            .cloned()
            .ok_or(IdentitySpecError::MissingKey(key))
    };
    let parse_u32 = |key: &'static str, value: String| -> Result<u32, IdentitySpecError> {
        value
            .parse::<u32>()
            .map_err(|_| IdentitySpecError::InvalidValue {
                key: key.to_owned(),
                value,
            })
    };
    let dev_major = parse_u32("dev_major", required("dev_major")?)?;
    let dev_minor = parse_u32("dev_minor", required("dev_minor")?)?;
    let inode = {
        let raw = required("inode")?;
        raw.parse::<u64>()
            .map_err(|_| IdentitySpecError::InvalidValue {
                key: "inode".to_owned(),
                value: raw,
            })?
    };
    let btime = parse_timestamp("btime", &required("btime")?)?;
    let size = {
        let raw = required("size")?;
        raw.parse::<u64>()
            .map_err(|_| IdentitySpecError::InvalidValue {
                key: "size".to_owned(),
                value: raw,
            })?
    };
    let mtime = parse_timestamp("mtime", &required("mtime")?)?
        .ok_or(IdentitySpecError::MissingKey("mtime"))?;
    let ctime = parse_timestamp("ctime", &required("ctime")?)?
        .ok_or(IdentitySpecError::MissingKey("ctime"))?;
    let nlink = parse_u32("nlink", required("nlink")?)?;
    let mode = u16::from_str_radix(required("mode")?.trim(), 8).map_err(|error| {
        IdentitySpecError::InvalidValue {
            key: "mode".to_owned(),
            value: error.to_string(),
        }
    })?;
    let mount_id = match seen
        .get("mount_id")
        .filter(|value| value.as_str() != "none")
    {
        Some(value) => Some(
            value
                .parse::<u64>()
                .map_err(|_| IdentitySpecError::InvalidValue {
                    key: "mount_id".to_owned(),
                    value: value.clone(),
                })?,
        ),
        None => None,
    };
    Ok(FileIdentity {
        dev_major,
        dev_minor,
        inode,
        btime,
        size,
        mtime,
        ctime,
        nlink,
        mode,
        mount_id,
    })
}

/// 替换工作文件的前提：目标路径必须不存在，或必须仍是调用方观察到的那个文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpectedTarget {
    /// 目标必须不存在（新建）。用 `RENAME_NOREPLACE` 让内核兜住竞争。
    Absent,
    /// 目标必须仍是这个身份（`fs_intent.expected_before`）。
    Present(FileIdentity),
}

impl ExpectedTarget {
    /// 报告用的短名。
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            ExpectedTarget::Absent => "absent",
            ExpectedTarget::Present(_) => "identity",
        }
    }
}

/// 前提核对结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreconditionOutcome {
    /// 目标不存在：用 `RENAME_NOREPLACE` 创建。
    Create,
    /// 目标仍是期望身份：原子替换。
    Replace,
    /// 拒绝继续（暂停并报告），不修改目标。
    Refused(Box<PreconditionRefusal>),
}

/// 拒绝替换的原因。`code()` 是稳定字符串，测试按它断言，不按人类可读句子断言。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreconditionRefusal {
    /// 稳定原因码。
    pub code: &'static str,
    /// 期望的目标状态。
    pub expected: String,
    /// 实际观察到的目标状态（不存在时为 `none`）。
    pub observed: String,
    /// 期望身份（`Absent` 时为 `None`）。
    pub expected_identity: Option<FileIdentity>,
    /// 实际观察到的身份。
    pub observed_identity: Option<FileIdentity>,
    /// 指纹不同的字段名。
    pub changed_fields: Vec<&'static str>,
}

impl PreconditionRefusal {
    /// 目标从“不存在”变成“存在”。
    #[must_use]
    pub fn target_appeared(observed: &FileIdentity) -> Self {
        Self {
            code: "target_appeared",
            expected: ExpectedTarget::Absent.as_str().to_owned(),
            observed: observed.to_string(),
            expected_identity: None,
            observed_identity: Some(observed.clone()),
            changed_fields: Vec::new(),
        }
    }

    /// 期望存在的目标现在不存在或被删除。
    #[must_use]
    pub fn target_missing(expected: &FileIdentity) -> Self {
        Self {
            code: "target_missing",
            expected: expected.to_string(),
            observed: "none".to_owned(),
            expected_identity: Some(expected.clone()),
            observed_identity: None,
            changed_fields: Vec::new(),
        }
    }

    /// 目标还是存在，但已经不是我们观察到的那个文件/指纹。
    #[must_use]
    pub fn target_changed(expected: &FileIdentity, observed: &FileIdentity) -> Self {
        Self {
            code: "target_changed",
            expected: expected.to_string(),
            observed: observed.to_string(),
            expected_identity: Some(expected.clone()),
            observed_identity: Some(observed.clone()),
            changed_fields: expected.changed_fields(observed),
        }
    }
}

/// 核对前提：`observed` 是替换前从目标目录句柄读到的当前状态。
#[must_use]
pub fn check_target(
    expected: &ExpectedTarget,
    observed: Option<&FileIdentity>,
) -> PreconditionOutcome {
    match (expected, observed) {
        (ExpectedTarget::Absent, None) => PreconditionOutcome::Create,
        (ExpectedTarget::Absent, Some(current)) => {
            PreconditionOutcome::Refused(Box::new(PreconditionRefusal::target_appeared(current)))
        }
        (ExpectedTarget::Present(expected_identity), None) => PreconditionOutcome::Refused(
            Box::new(PreconditionRefusal::target_missing(expected_identity)),
        ),
        (ExpectedTarget::Present(expected_identity), Some(current)) => {
            if expected_identity.fingerprint_equal(current) {
                PreconditionOutcome::Replace
            } else {
                PreconditionOutcome::Refused(Box::new(PreconditionRefusal::target_changed(
                    expected_identity,
                    current,
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(inode: u64, size: u64) -> FileIdentity {
        FileIdentity {
            dev_major: 0,
            dev_minor: 121,
            inode,
            btime: Some((1_793_881_188, 822_826_519)),
            size,
            mtime: (1_793_881_200, 1),
            ctime: (1_793_881_201, 2),
            nlink: 1,
            mode: S_IFREG | 0o644,
            mount_id: Some(68),
        }
    }

    #[test]
    fn spec_round_trips_through_text() {
        let original = identity(257, 4_194_304);
        let parsed = parse_identity_spec(&original.to_spec()).unwrap();
        assert_eq!(parsed, original);
        assert!(parsed.is_regular_file());
        assert_eq!(parsed.permission_bits(), 0o644);
    }

    #[test]
    fn spec_parsing_rejects_incomplete_or_ambiguous_input() {
        let text = identity(257, 10).to_spec();
        let without_size: String = text
            .lines()
            .filter(|line| !line.starts_with("size="))
            .map(|line| format!("{line}\n"))
            .collect();
        assert_eq!(
            parse_identity_spec(&without_size),
            Err(IdentitySpecError::MissingKey("size"))
        );
        let duplicated = format!("{text}inode=999\n");
        assert_eq!(
            parse_identity_spec(&duplicated),
            Err(IdentitySpecError::DuplicateKey("inode"))
        );
        let bad_nanos = text.replace("mtime=1793881200.000000001", "mtime=1793881200.1000000000");
        assert!(matches!(
            parse_identity_spec(&bad_nanos),
            Err(IdentitySpecError::InvalidValue { .. })
        ));
        // 未知字段（例如原型命令输出的 resolve=）被忽略，不影响解析。
        let with_extra = format!("{text}resolve=openat2\n");
        assert_eq!(parse_identity_spec(&with_extra).unwrap(), identity(257, 10));
    }

    #[test]
    fn changed_fields_are_stable_and_ordered() {
        let before = identity(257, 10);
        let after = FileIdentity {
            inode: 258,
            size: 11,
            ..before.clone()
        };
        assert_eq!(after.changed_fields(&before), vec!["inode", "size"]);
        assert!(!after.same_object(&before));
        assert!(!before.fingerprint_equal(&after));

        let written_in_place = FileIdentity {
            size: 11,
            mtime: (1_793_881_300, 5),
            ctime: (1_793_881_300, 5),
            ..before.clone()
        };
        assert!(before.same_object(&written_in_place));
        assert_eq!(
            before.changed_fields(&written_in_place),
            vec!["size", "mtime", "ctime"]
        );
    }

    #[test]
    fn precondition_matrix() {
        let expected = identity(257, 10);
        assert_eq!(
            check_target(&ExpectedTarget::Absent, None),
            PreconditionOutcome::Create
        );
        assert_eq!(
            check_target(&ExpectedTarget::Present(expected.clone()), Some(&expected)),
            PreconditionOutcome::Replace
        );

        match check_target(&ExpectedTarget::Absent, Some(&expected)) {
            PreconditionOutcome::Refused(refusal) => {
                assert_eq!(refusal.code, "target_appeared");
                assert_eq!(refusal.observed_identity, Some(expected.clone()));
            }
            other => panic!("期望拒绝，实际 {other:?}"),
        }
        match check_target(&ExpectedTarget::Present(expected.clone()), None) {
            PreconditionOutcome::Refused(refusal) => {
                assert_eq!(refusal.code, "target_missing");
                assert_eq!(refusal.observed_identity, None);
            }
            other => panic!("期望拒绝，实际 {other:?}"),
        }
        let rewritten = FileIdentity {
            mtime: (1_793_881_400, 9),
            ..expected.clone()
        };
        match check_target(&ExpectedTarget::Present(expected.clone()), Some(&rewritten)) {
            PreconditionOutcome::Refused(refusal) => {
                assert_eq!(refusal.code, "target_changed");
                assert_eq!(refusal.changed_fields, vec!["mtime"]);
            }
            other => panic!("期望拒绝，实际 {other:?}"),
        }
    }
}
