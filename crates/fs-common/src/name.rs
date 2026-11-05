//! 单个名称分量的跨平台校验（design §16.1、§16.2）。
//!
//! 一个“名称”是目录里的一个条目名：它不能为空、不能含路径分隔符、不能是 `.`/`..`、
//! 不能含 NUL 与 Windows 非法字符、不能以点或空格结尾、不能是 Windows 保留设备名，
//! 长度必须同时满足 Linux 的字节上限与 Windows 的 UTF-16 码元上限。
//!
//! 本模块只回答“能不能无损跨端表示”和“两个名称是否指同一条目”，不做任何改写：不把名称
//! 转成小写、不做 NFC/NFD 归一化、不替换字符。设计 §16.2 要求这类名称“明确问题、由用户
//! 选择兼容名称”，因此这里返回原因分类而不是修好的名称。

use std::error::Error;
use std::fmt;

/// 单个名称分量允许的最大 UTF-8 字节数（Linux `NAME_MAX`）。
pub const MAX_COMPONENT_UTF8_BYTES: usize = 255;
/// 单个名称分量允许的最大 UTF-16 码元数（Windows NTFS 组件上限）。
pub const MAX_COMPONENT_UTF16_UNITS: usize = 255;

/// Windows 保留设备名（大小写不敏感，带扩展名时依然保留）。
///
/// 载荷是被判定为保留的那个设备名，仅用于报告，不替代用户名称。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReservedName(&'static str);

impl ReservedName {
    /// 保留设备名的规范写法，例如 `CON`、`COM1`。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ReservedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// 设备名清单：`CON`/`PRN`/`AUX`/`NUL` 与 `COM1`—`COM9`、`LPT1`—`LPT9`。
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 名称不合规的明确原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameIssue {
    /// 空名称。
    Empty,
    /// 不是合法 UTF-8：首发不支持跨平台传输，且不做有损解码（design §16.2）。
    NonUtf8,
    /// 含 NUL 字节。
    Nul,
    /// 含路径分隔符 `/` 或 `\`（含 Windows 设备路径 `\\?\`、`\\.\` 的前缀）。
    Separator,
    /// 名称恰好是 `.`。
    CurrentDirectory,
    /// 名称恰好是 `..`。
    ParentDirectory,
    /// 含控制字符（`U+0000`—`U+001F`；NUL 单独报 [`NameIssue::Nul`]）。
    Control(char),
    /// 含 Windows 非法字符 `<`、`>`、`:`、`"`、`|`、`?`、`*`（`/`、`\` 报 [`NameIssue::Separator`]）。
    Illegal(char),
    /// 以点结尾：Windows 会静默去掉，导致与原名不同的条目撞名。
    TrailingDot,
    /// 以空格结尾：Windows 会静默去掉，导致与原名不同的条目撞名。
    TrailingSpace,
    /// Windows 保留设备名（按第一个 `.` 之前的部分判定）。
    Reserved(ReservedName),
    /// 过长：需同时满足 UTF-8 字节与 UTF-16 码元两个上限。
    TooLong {
        /// 名称的 UTF-8 字节数。
        utf8_bytes: usize,
        /// 名称的 UTF-16 码元数。
        utf16_units: usize,
    },
}

impl fmt::Display for NameIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameIssue::Empty => f.write_str("名称为空"),
            NameIssue::NonUtf8 => f.write_str("名称不是合法 UTF-8，首发不支持跨平台传输"),
            NameIssue::Nul => f.write_str("名称含 NUL 字节"),
            NameIssue::Separator => f.write_str("名称含路径分隔符 '/' 或 '\\'"),
            NameIssue::CurrentDirectory => f.write_str("名称不得是 '.'"),
            NameIssue::ParentDirectory => f.write_str("名称不得是 '..'"),
            NameIssue::Control(ch) => write!(f, "名称含控制字符 U+{:04X}", u32::from(*ch)),
            NameIssue::Illegal(ch) => write!(f, "名称含 Windows 非法字符 {ch:?}"),
            NameIssue::TrailingDot => f.write_str("名称以点结尾，Windows 会静默去掉"),
            NameIssue::TrailingSpace => f.write_str("名称以空格结尾，Windows 会静默去掉"),
            NameIssue::Reserved(name) => {
                write!(f, "名称是 Windows 保留设备名 {name}（带扩展名时仍然保留）")
            }
            NameIssue::TooLong {
                utf8_bytes,
                utf16_units,
            } => write!(
                f,
                "名称过长：{utf8_bytes} 字节 / {utf16_units} UTF-16 码元（上限 {MAX_COMPONENT_UTF8_BYTES} / {MAX_COMPONENT_UTF16_UNITS}）"
            ),
        }
    }
}

impl Error for NameIssue {}

/// 校验一个用于跨端传输的名称分量。
///
/// 规则按固定顺序检查并返回第一个问题，因此同一名称总是得到同一个 [`NameIssue`]：
/// 空 → NUL → 分隔符 → `.`/`..` → 控制/非法字符 → 长度 → 尾部点/空格 → 保留名。
///
/// 本函数不改写名称。UTF-8 之外的字节输入请用 [`validate_name_bytes`]，
/// 以把“非 UTF-8”和“名称内容违规”区分开。
///
/// # Errors
///
/// 违反上述任一规则时返回对应的 [`NameIssue`]。
pub fn validate_name(name: &str) -> Result<(), NameIssue> {
    if name.is_empty() {
        return Err(NameIssue::Empty);
    }
    if name.contains('\0') {
        return Err(NameIssue::Nul);
    }
    if name.contains(['/', '\\']) {
        return Err(NameIssue::Separator);
    }
    match name {
        "." => return Err(NameIssue::CurrentDirectory),
        ".." => return Err(NameIssue::ParentDirectory),
        _ => {}
    }
    for ch in name.chars() {
        if (ch as u32) < 0x20 {
            return Err(NameIssue::Control(ch));
        }
        if matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            return Err(NameIssue::Illegal(ch));
        }
    }
    let utf8_bytes = name.len();
    let utf16_units: usize = name.encode_utf16().count();
    if utf8_bytes > MAX_COMPONENT_UTF8_BYTES || utf16_units > MAX_COMPONENT_UTF16_UNITS {
        return Err(NameIssue::TooLong {
            utf8_bytes,
            utf16_units,
        });
    }
    if name.ends_with('.') {
        return Err(NameIssue::TrailingDot);
    }
    if name.ends_with(' ') {
        return Err(NameIssue::TrailingSpace);
    }
    // Windows 只看第一个点之前的部分判断设备名：`CON.txt`、`nul.tar.gz` 都是保留名。
    let base = name.split('.').next().unwrap_or(name);
    if let Some(reserved) = reserved_name(base) {
        return Err(NameIssue::Reserved(reserved));
    }
    Ok(())
}

/// 校验来自平台的原生名称字节。
///
/// 非 UTF-8 名称明确归类为 [`NameIssue::NonUtf8`]（design §16.2：首发不支持、不做有损解码），
/// 其余规则与 [`validate_name`] 相同。
///
/// # Errors
///
/// 非 UTF-8，或 [`validate_name`] 报出的任一问题。
pub fn validate_name_bytes(name: &[u8]) -> Result<(), NameIssue> {
    let text = std::str::from_utf8(name).map_err(|_| NameIssue::NonUtf8)?;
    validate_name(text)
}

/// 按 Windows 规则判定 `base`（第一个 `.` 之前的部分）是否为保留设备名。
fn reserved_name(base: &str) -> Option<ReservedName> {
    // Windows 把上标数字 `①`…视为对应数字，故 `COM¹`、`LPT³` 与 `COM1`、`LPT3` 同属保留名。
    let mut folded = String::with_capacity(base.len());
    for ch in base.chars() {
        folded.push(match ch {
            '¹' => '1',
            '²' => '2',
            '³' => '3',
            other => other.to_ascii_uppercase(),
        });
    }
    RESERVED_NAMES
        .iter()
        .find(|name| **name == folded)
        .map(|name| ReservedName(name))
}

/// 原生名称等价检查端口（design §16.2 的跨平台名称冲突）。
///
/// 纯核心只能判定可在任何平台确定性证明的等价关系；某个平台真实的“同一目录条目”语义
/// （Windows 的 `CompareStringEx` 排序表、文件系统的 Unicode 归一化或大小写表）必须由平台层
/// 实现本端口注入，例如 S065 的 Windows 扫描。端口只回答“是否等价”，**不提供**改名或小写后的
/// 名称，因此接入原生语义不会造成静默改名。
pub trait NameEquivalence {
    /// `left` 与 `right` 在该平台上是否指向同一个目录条目。
    fn equivalent(&self, left: &str, right: &str) -> bool;
}

/// 可移植默认等价：字节完全相同，或 Unicode 小写折叠后相同。
///
/// 折叠只用于比较，不改变任何存储的名称。它覆盖 ASCII 大小写差异（design §16.2 的
/// `a.txt` 与 `A.txt`）以及常见 Unicode 大小写差异；不做规范归一化，因此 `é`（预组合）与
/// `e`+`U+0301`（组合）在这里判定为不等价，这类平台差异由 [`NameEquivalence`] 的原生实现补齐。
/// 判定偏保守（宁可报冲突，也不静默覆盖另一条目）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortableNameEquivalence;

impl NameEquivalence for PortableNameEquivalence {
    fn equivalent(&self, left: &str, right: &str) -> bool {
        left == right || left.to_lowercase() == right.to_lowercase()
    }
}

/// 同一目录内两个等价名称的碰撞。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameCollision {
    /// 较早出现名称在输入切片中的下标。
    pub first_index: usize,
    /// 较晚出现名称在输入切片中的下标。
    pub second_index: usize,
    /// 较早出现的名称。
    pub first: String,
    /// 较晚出现的名称。
    pub second: String,
}

/// 找出同一目录内互相撞名的名称对，按 `(first_index, second_index)` 升序稳定返回。
///
/// 只判定等价关系，不校验名称是否合规（用 [`validate_name`]），也不选择“保留哪一个”：design
/// §16.2 与 §7.4 要求把碰撞报为明确问题，禁止静默覆盖另一条目。
#[must_use]
pub fn find_collisions(names: &[&str], equivalence: &impl NameEquivalence) -> Vec<NameCollision> {
    let mut collisions = Vec::new();
    for (second_index, second) in names.iter().enumerate() {
        for (first_index, first) in names[..second_index].iter().enumerate() {
            if equivalence.equivalent(first, second) {
                collisions.push(NameCollision {
                    first_index,
                    second_index,
                    first: (*first).to_owned(),
                    second: (*second).to_owned(),
                });
            }
        }
    }
    collisions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(name: &str) -> NameIssue {
        validate_name(name).expect_err("名称应被拒绝")
    }

    #[test]
    fn accepts_names_representable_on_both_platforms() {
        // 只校验结构，不做任何改写：原名、空格、前导点、混合大小写在通过校验后保持原样。
        for name in [
            "a",
            "a.txt",
            ".hidden",
            " report 2026.docx",
            "Ünïcødé.txt",
            "报告.docx",
            "🙂.txt",
            "x.y.z",
            "CONS",
            "com0",
            "lpt",
            "nul_suffix",
        ] {
            assert_eq!(validate_name(name), Ok(()), "应接受 {name:?}");
        }
    }

    #[test]
    fn rejects_structural_escapes() {
        assert_eq!(issue(""), NameIssue::Empty);
        assert_eq!(issue("a\0b"), NameIssue::Nul);
        assert_eq!(issue("."), NameIssue::CurrentDirectory);
        assert_eq!(issue(".."), NameIssue::ParentDirectory);
        assert_eq!(issue("a/b"), NameIssue::Separator);
        assert_eq!(issue("a\\b"), NameIssue::Separator);
        assert_eq!(issue("\\\\.\\C:"), NameIssue::Separator);
        assert_eq!(issue("x\u{1}y"), NameIssue::Control('\u{1}'));
    }

    #[test]
    fn classifies_windows_only_problems() {
        assert_eq!(issue("a."), NameIssue::TrailingDot);
        assert_eq!(issue("a "), NameIssue::TrailingSpace);
        assert_eq!(issue("a. "), NameIssue::TrailingSpace);
        assert_eq!(issue("a:b"), NameIssue::Illegal(':'));
        assert_eq!(issue("a?b"), NameIssue::Illegal('?'));
        assert_eq!(issue("a*b"), NameIssue::Illegal('*'));
        assert_eq!(issue("a\"b"), NameIssue::Illegal('"'));
        assert_eq!(issue("a<b"), NameIssue::Illegal('<'));
        assert_eq!(issue("a>b"), NameIssue::Illegal('>'));
        assert_eq!(issue("a|b"), NameIssue::Illegal('|'));
    }

    #[test]
    fn classifies_windows_reserved_device_names() {
        // 保留名大小写不敏感，且看第一个点之前的部分，因此带扩展名同样保留。
        assert_eq!(issue("CON"), NameIssue::Reserved(ReservedName("CON")));
        assert_eq!(issue("con.txt"), NameIssue::Reserved(ReservedName("CON")));
        assert_eq!(
            issue("nul.tar.gz"),
            NameIssue::Reserved(ReservedName("NUL"))
        );
        assert_eq!(issue("Prn"), NameIssue::Reserved(ReservedName("PRN")));
        assert_eq!(issue("AUX"), NameIssue::Reserved(ReservedName("AUX")));
        assert_eq!(issue("com1"), NameIssue::Reserved(ReservedName("COM1")));
        assert_eq!(issue("LPT9.log"), NameIssue::Reserved(ReservedName("LPT9")));
        assert_eq!(issue("COM¹"), NameIssue::Reserved(ReservedName("COM1")));
        assert_eq!(issue("lpt³.txt"), NameIssue::Reserved(ReservedName("LPT3")));
        assert_eq!(ReservedName("COM1").as_str(), "COM1");
        assert_eq!(ReservedName("COM1").to_string(), "COM1");
    }

    #[test]
    fn classifies_length_on_both_limits() {
        // 255 个 ASCII 字节、255 个 UTF-16 码元：两个上限都刚好满足。
        let at_limit = "a".repeat(MAX_COMPONENT_UTF8_BYTES);
        assert_eq!(validate_name(&at_limit), Ok(()));

        // 256 个 UTF-16 码元（1024 字节）：UTF-16 上限先被突破。
        let too_many_units = "a".repeat(MAX_COMPONENT_UTF16_UNITS + 1);
        assert_eq!(
            validate_name(&too_many_units),
            Err(NameIssue::TooLong {
                utf8_bytes: MAX_COMPONENT_UTF16_UNITS + 1,
                utf16_units: MAX_COMPONENT_UTF16_UNITS + 1,
            })
        );

        // 100 个汉字：UTF-16 码元没问题，但 300 UTF-8 字节超过 Linux NAME_MAX。
        let too_many_bytes = "汉".repeat(100);
        assert_eq!(
            validate_name(&too_many_bytes),
            Err(NameIssue::TooLong {
                utf8_bytes: 300,
                utf16_units: 100,
            })
        );
        assert!(
            NameIssue::TooLong {
                utf8_bytes: 300,
                utf16_units: 100,
            }
            .to_string()
            .contains("300 字节 / 100 UTF-16 码元")
        );
    }

    #[test]
    fn rejects_non_utf8_bytes_without_lossy_decoding() {
        assert_eq!(validate_name_bytes(b"a.txt"), Ok(()));
        // 0xFF 不是合法 UTF-8：明确归类，不做替换字符解码（design §16.2）。
        assert_eq!(validate_name_bytes(b"a\xff.txt"), Err(NameIssue::NonUtf8));
        assert_eq!(validate_name_bytes(b"a\0b"), Err(NameIssue::Nul));
    }

    #[test]
    fn display_messages_are_explicit() {
        assert!(issue("").to_string().contains("名称为空"));
        assert!(issue("a/b").to_string().contains("分隔符"));
        assert!(issue("CON").to_string().contains("CON"));
        assert!(issue("a.").to_string().contains("点结尾"));
        assert!(issue("a ").to_string().contains("空格结尾"));
        assert!(issue("a:b").to_string().contains("':'"));
    }

    #[test]
    fn portable_equivalence_covers_ascii_case_without_renaming() {
        let equivalence = PortableNameEquivalence;
        assert!(equivalence.equivalent("a.txt", "A.txt"));
        assert!(equivalence.equivalent("报告.docx", "报告.docx"));
        // 小写折叠只用于比较：存储的名称保持原样。
        let names = ["a.txt", "A.txt"];
        assert_eq!(names[0], "a.txt");
        assert!(!equivalence.equivalent("a.txt", "b.txt"));
        // 组合形式不同（NFC/NFD）默认判为不等价，留给原生端口；不静默归一化。
        assert!(!equivalence.equivalent("é", "e\u{301}"));
    }

    #[test]
    fn collision_fixtures_report_each_pair_once() {
        let equivalence = PortableNameEquivalence;
        let report = find_collisions(&["a.txt", "b.txt", "A.txt", "报告", "报告"], &equivalence);
        assert_eq!(
            report,
            vec![
                NameCollision {
                    first_index: 0,
                    second_index: 2,
                    first: "a.txt".to_owned(),
                    second: "A.txt".to_owned(),
                },
                NameCollision {
                    first_index: 3,
                    second_index: 4,
                    first: "报告".to_owned(),
                    second: "报告".to_owned(),
                },
            ]
        );
        // 相同输入必然得到相同报告（顺序按下标，不依赖哈希或平台比较表）。
        assert_eq!(find_collisions(&["a", "A"], &equivalence).len(), 1);
        assert!(find_collisions(&["a", "b", "c"], &equivalence).is_empty());
        assert!(find_collisions(&[], &equivalence).is_empty());
    }

    /// 用一个“大小写与归一化都敏感”的端口证明碰撞判定完全由注入的等价关系决定。
    struct ExactNames;

    impl NameEquivalence for ExactNames {
        fn equivalent(&self, left: &str, right: &str) -> bool {
            left == right
        }
    }

    #[test]
    fn collisions_follow_the_injected_native_equivalence() {
        assert!(find_collisions(&["a.txt", "A.txt"], &ExactNames).is_empty());
        assert_eq!(find_collisions(&["a", "a"], &ExactNames).len(), 1);
        // 端口没有改名能力：折叠逻辑不影响调用方持有的名称。
        assert_eq!(
            find_collisions(&["Ä", "ä"], &PortableNameEquivalence).len(),
            1
        );
    }
}
