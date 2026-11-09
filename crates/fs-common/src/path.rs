//! 跨端相对路径规则：传输中的路径只能是由 `/` 分隔、已校验的普通分量序列（design §16.1）。
//!
//! Vault 内部条目的路径一律用相对路径表达：不含盘符、不以分隔符开头、不含 `.`/`..`、不含空
//! 分量、不含 NUL、不含反斜杠，每个分量都通过 [`crate::name::validate_name`]。这样网络传输与
//! 持久化的路径不会携带逃逸形式，也不会因平台分隔符差异出现两种写法。
//!
//! 路径关系（重叠判定）一律按分量比较，**不用字符串前缀**：`a` 不是 `ab` 的父目录。

use crate::name::{self, NameIssue};
use std::error::Error;
use std::fmt;
use std::str::FromStr;

/// 相对路径整体允许的最大 UTF-8 字节数（Linux `PATH_MAX` 留出一个字节给 NUL）。
///
/// Windows 的绝对路径上限（`MAX_PATH`、长路径前缀计数）取决于 Vault 的挂载前缀，属于平台层
/// 判定，不在这里重复。
pub const MAX_RELATIVE_PATH_UTF8_BYTES: usize = 4095;

/// 已校验的相对路径：非空分量序列，文本形式是用 `/` 连接的分量。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelativePath {
    components: Vec<String>,
}

impl RelativePath {
    /// 解析并校验一个相对路径文本。
    ///
    /// 不做任何规范化或修复：`a//b`、`a/`、`./a`、`a/../b` 都按原因报错，而不是被折叠成
    /// 另一个路径，避免调用方以为自己传的路径被安全地“修正”过。
    ///
    /// # Errors
    ///
    /// 见 [`PathIssue`]。
    pub fn parse(text: &str) -> Result<Self, PathIssue> {
        if text.is_empty() {
            return Err(PathIssue::Empty);
        }
        if text.contains('\0') {
            return Err(PathIssue::Nul);
        }
        if text.starts_with(['/', '\\']) {
            return Err(PathIssue::Absolute);
        }
        let mut chars = text.chars();
        if let Some(drive) = chars.next().filter(char::is_ascii_alphabetic) {
            if chars.next() == Some(':') {
                return Err(PathIssue::DriveLetter(drive));
            }
        }
        if text.contains('\\') {
            return Err(PathIssue::Backslash);
        }
        if text.len() > MAX_RELATIVE_PATH_UTF8_BYTES {
            return Err(PathIssue::TooLong {
                utf8_bytes: text.len(),
            });
        }
        let mut components = Vec::new();
        for (index, component) in text.split('/').enumerate() {
            if component.is_empty() {
                return Err(PathIssue::EmptyComponent { index });
            }
            name::validate_name(component)
                .map_err(|issue| PathIssue::Component { index, issue })?;
            components.push(component.to_owned());
        }
        Ok(RelativePath { components })
    }

    /// 已校验的分量（至少一个）。
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.components
    }

    /// 前 `len` 个分量构成的祖先/自身路径。
    ///
    /// 调用方保证 `1 <= len <= components().len()`；`0`（Vault 自身）不是 `RelativePath`，
    /// 越界是调用方 bug。排除规则用它报告“命中的是哪个祖先目录”（`ignore::decide`）。
    pub(crate) fn prefix(&self, len: usize) -> RelativePath {
        debug_assert!(len > 0 && len <= self.components.len(), "前缀长度越界");
        RelativePath {
            components: self.components[..len].to_vec(),
        }
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.components.join("/"))
    }
}

impl FromStr for RelativePath {
    type Err = PathIssue;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        RelativePath::parse(text)
    }
}

/// 相对路径不合规的明确原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathIssue {
    /// 空文本；Vault 自身不是可传输路径。
    Empty,
    /// 以 `/` 或 `\` 开头：绝对路径、UNC 或 Windows 设备路径（`\\?\`、`\\.\`）。
    Absolute,
    /// 以盘符开头，例如 `C:…`。
    DriveLetter(char),
    /// 含 NUL 字节。
    Nul,
    /// 含反斜杠：分隔符混用或 Windows 原生写法。
    Backslash,
    /// 出现空分量，例如 `a//b` 或尾部 `/`；`index` 是空分量的下标。
    EmptyComponent {
        /// 空分量的下标（0 基）。
        index: usize,
    },
    /// 分量不合规；`index` 是分量下标，`issue` 是名称层的原因。
    Component {
        /// 分量下标（0 基）。
        index: usize,
        /// 名称层原因。
        issue: NameIssue,
    },
    /// 整体长度超过 [`MAX_RELATIVE_PATH_UTF8_BYTES`]。
    TooLong {
        /// 实际 UTF-8 字节数。
        utf8_bytes: usize,
    },
}

impl fmt::Display for PathIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathIssue::Empty => f.write_str("相对路径为空"),
            PathIssue::Absolute => f.write_str("不接受绝对路径、UNC 或设备路径"),
            PathIssue::DriveLetter(drive) => write!(f, "不接受盘符前缀 {drive}:"),
            PathIssue::Nul => f.write_str("相对路径含 NUL 字节"),
            PathIssue::Backslash => f.write_str("相对路径只能使用 '/' 分隔符，不接受反斜杠"),
            PathIssue::EmptyComponent { index } => write!(f, "路径第 {index} 个分量为空"),
            PathIssue::Component { index, issue } => {
                write!(f, "路径第 {index} 个分量不合规：{issue}")
            }
            PathIssue::TooLong { utf8_bytes } => write!(
                f,
                "相对路径过长：{utf8_bytes} 字节（上限 {MAX_RELATIVE_PATH_UTF8_BYTES}）"
            ),
        }
    }
}

impl Error for PathIssue {}

/// 两个相对路径的分量关系（`Ancestor` 表示前者是后者的祖先目录）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRelation {
    /// 完全相同。
    Same,
    /// 前者是后者的祖先目录。
    Ancestor,
    /// 后者是前者的祖先目录。
    Descendant,
    /// 互不包含。
    Unrelated,
}

/// 按路径分量（而不是字符串前缀）判定 `a` 与 `b` 的关系。
///
/// `a` 与 `ab` 的关系是 [`PathRelation::Unrelated`]（design §16.1）；分隔符差异与末尾分隔符
/// 在解析阶段已经被拒绝，因此这里只比较分量。
#[must_use]
pub fn relation(a: &RelativePath, b: &RelativePath) -> PathRelation {
    let common = a
        .components
        .iter()
        .zip(b.components.iter())
        .take_while(|(left, right)| left == right)
        .count();
    if common == a.components.len() && common == b.components.len() {
        PathRelation::Same
    } else if common == a.components.len() {
        PathRelation::Ancestor
    } else if common == b.components.len() {
        PathRelation::Descendant
    } else {
        PathRelation::Unrelated
    }
}

/// `path` 是否等于 `base` 或位于 `base` 之下（按分量判定）。
#[must_use]
pub fn is_within(path: &RelativePath, base: &RelativePath) -> bool {
    matches!(
        relation(path, base),
        PathRelation::Same | PathRelation::Descendant
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> RelativePath {
        RelativePath::parse(text).expect("路径应通过校验")
    }

    fn issue(text: &str) -> PathIssue {
        RelativePath::parse(text).expect_err("路径应被拒绝")
    }

    #[test]
    fn accepts_canonical_relative_paths() {
        assert_eq!(path("a").components(), ["a"]);
        assert_eq!(path("a/b").components(), ["a", "b"]);
        assert_eq!(path(".hidden/x.txt").components(), [".hidden", "x.txt"]);
        // 显示与解析往返不改变文本：不做大小写或 Unicode 归一化。
        for text in ["a", "dir/sub/file.txt", "报告/2026.docx", "Ünïcødé/x"] {
            assert_eq!(path(text).to_string(), text);
            assert_eq!(text.parse::<RelativePath>(), Ok(path(text)));
        }
    }

    #[test]
    fn rejects_traversal_forms() {
        assert_eq!(
            issue(".."),
            PathIssue::Component {
                index: 0,
                issue: NameIssue::ParentDirectory,
            }
        );
        assert_eq!(
            issue("../secret"),
            PathIssue::Component {
                index: 0,
                issue: NameIssue::ParentDirectory,
            }
        );
        assert_eq!(
            issue("a/../b"),
            PathIssue::Component {
                index: 1,
                issue: NameIssue::ParentDirectory,
            }
        );
        assert_eq!(
            issue("a/./b"),
            PathIssue::Component {
                index: 1,
                issue: NameIssue::CurrentDirectory,
            }
        );
    }

    #[test]
    fn rejects_absolute_drive_and_device_forms() {
        assert_eq!(issue("/etc/passwd"), PathIssue::Absolute);
        assert_eq!(issue("\\\\server\\share"), PathIssue::Absolute);
        assert_eq!(issue("\\\\?\\C:\\x"), PathIssue::Absolute);
        assert_eq!(issue(r"\.\pipe\x"), PathIssue::Absolute);
        assert_eq!(issue("C:/x"), PathIssue::DriveLetter('C'));
        assert_eq!(issue("c:"), PathIssue::DriveLetter('c'));
        // 非盘符的冒号留给名称层，报 Windows 非法字符。
        assert_eq!(
            issue("1:/x"),
            PathIssue::Component {
                index: 0,
                issue: NameIssue::Illegal(':'),
            }
        );
    }

    #[test]
    fn rejects_nul_and_mixed_separators() {
        assert_eq!(issue("a\0b"), PathIssue::Nul);
        assert_eq!(issue("a\0"), PathIssue::Nul);
        assert_eq!(issue("a\\b"), PathIssue::Backslash);
        assert_eq!(issue("dir\\file.txt"), PathIssue::Backslash);
    }

    #[test]
    fn rejects_empty_components() {
        assert_eq!(issue(""), PathIssue::Empty);
        assert_eq!(issue("a//b"), PathIssue::EmptyComponent { index: 1 });
        assert_eq!(issue("a/"), PathIssue::EmptyComponent { index: 1 });
        assert_eq!(issue("a/b//"), PathIssue::EmptyComponent { index: 2 });
    }

    #[test]
    fn propagates_name_level_problems_with_component_index() {
        assert!(matches!(
            issue("x/CON"),
            PathIssue::Component {
                index: 1,
                issue: NameIssue::Reserved(reserved),
            } if reserved.as_str() == "CON"
        ));
        assert_eq!(
            issue("a/b."),
            PathIssue::Component {
                index: 1,
                issue: NameIssue::TrailingDot,
            }
        );
        assert_eq!(
            issue("a/b: c"),
            PathIssue::Component {
                index: 1,
                issue: NameIssue::Illegal(':'),
            }
        );
        assert!(issue("a/CON").to_string().contains("第 1 个分量"));
    }

    #[test]
    fn rejects_paths_beyond_the_portable_length_budget() {
        let component = "a".repeat(name::MAX_COMPONENT_UTF8_BYTES);
        let at_limit = vec![component.clone(); 16].join("/"); // 16*255 + 15 = 4095
        assert_eq!(at_limit.len(), MAX_RELATIVE_PATH_UTF8_BYTES);
        assert_eq!(path(&at_limit).components().len(), 16);

        let too_long = format!("{at_limit}/{component}");
        assert_eq!(
            issue(&too_long),
            PathIssue::TooLong {
                utf8_bytes: too_long.len(),
            }
        );
        // 组件超长时报组件层原因，而不是整体长度。
        let oversize_component = "a".repeat(name::MAX_COMPONENT_UTF8_BYTES + 1);
        assert!(matches!(
            issue(&oversize_component),
            PathIssue::Component {
                index: 0,
                issue: NameIssue::TooLong { .. },
            }
        ));
    }

    #[test]
    fn prefix_returns_the_leading_components() {
        let path = path("a/b/c");
        assert_eq!(path.prefix(1).to_string(), "a");
        assert_eq!(path.prefix(2).to_string(), "a/b");
        assert_eq!(path.prefix(3), path);
    }

    #[test]
    fn relation_uses_components_not_string_prefixes() {
        assert_eq!(relation(&path("a"), &path("ab")), PathRelation::Unrelated);
        assert_eq!(
            relation(&path("a/b"), &path("a/bc")),
            PathRelation::Unrelated
        );
        assert_eq!(relation(&path("a"), &path("a")), PathRelation::Same);
        assert_eq!(relation(&path("a"), &path("a/b")), PathRelation::Ancestor);
        assert_eq!(
            relation(&path("a/b/c"), &path("a")),
            PathRelation::Descendant
        );
        assert_eq!(
            relation(&path("a/b"), &path("c/d")),
            PathRelation::Unrelated
        );
        assert_eq!(
            relation(&path("a/b"), &path("a/c")),
            PathRelation::Unrelated
        );
        assert!(is_within(&path("a"), &path("a")));
        assert!(is_within(&path("a/b"), &path("a")));
        assert!(!is_within(&path("a"), &path("a/b")));
        assert!(!is_within(&path("ab"), &path("a")));
    }

    /// 同一批输入重复校验必须给出同一结论（不依赖哈希、目录顺序或平台比较表）。
    #[test]
    fn verdicts_are_deterministic() {
        for text in ["a/b", "a/../b", "/x", "C:x", "a\\b", "a//b", "a/CON"] {
            assert_eq!(RelativePath::parse(text), RelativePath::parse(text));
        }
    }
}
