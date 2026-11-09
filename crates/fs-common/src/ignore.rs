//! 排除规则（`.syncignore`）：注释、通配、根锚定、目录与否定规则，以及“为什么被排除”的解释。
//!
//! 语义以 Git 官方规则为参照（design §10.4、C13）。glob 匹配交给经验证的 `ignore::gitignore`
//! 实现（design §4.1：不自行拼写 glob 正则）；本模块只负责从文本构建规则集、按 Vault 相对
//! 路径给出裁决，并把命中的规则与原因说明白。
//!
//! 不变量（design §10.4）：
//!
//! - **排除表示不同步，不是删除**：被排除的路径不参与此绑定的同步，已下载的本地内容与
//!   服务器内容都保留。本模块只有“参与/不参与”两种裁决，不产生、也无法产生 Delete 事件；
//!   删除只能来自显式删除操作。
//! - **父目录排除后不能重新包含子项**：`secrets/` 排除后，`!secrets/public.txt` 不会让该
//!   子文件重新参与同步（Git 的限制）；解释里同时给出被阻断的否定规则。
//! - **不吞掉最终文件**：[`DEFAULT_RULES`] 只排除临时与缓存文件；编辑器先写 `x.docx.tmp`
//!   再 rename-over 成 `x.docx` 时，最终文件照常参与同步（T27）。
//!
//! 不做 I/O：规则文本来自绑定级配置接口或作为普通文件同步，本模块只接收文本；`ignore`
//! 中会读文件的入口（`Gitignore::new`、`GitignoreBuilder::add`）不使用。

use crate::path::RelativePath;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder, Glob};
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

/// 默认清单在匹配库里的来源标记（只用于区分“这条规则来自哪里”，不是要读取的文件）。
const DEFAULT_RULES_LABEL: &str = "vaultsyncd-default-rules";

/// 绑定规则文本在匹配库里的来源标记（同上）。
const BINDING_RULES_LABEL: &str = ".syncignore";

/// 匹配根：解析后的路径一律是 Vault 相对路径，匹配器按原样使用，不剥离前缀。
const MATCH_ROOT: &str = ".";

/// 内置默认排除清单（design §10.4、C13）。**可编辑**：改这里即改所有绑定的默认规则。
///
/// 只列常见缓存与临时文件；不排除整个 `.git`，也不排除任意隐藏文件。绑定级配置可以在它之后
/// 追加规则（[`IgnoreRules::parse_with_defaults`]，可用 `!` 重新包含），或提供完整清单替换它
/// （[`IgnoreRules::parse`]）。
pub const DEFAULT_RULES: &str = r#"# vaultsyncd 内置默认排除清单（design §10.4、C13）：只排除常见缓存与临时文件。
# 不排除整个 .git，也不排除任意隐藏文件；排除表示不参与同步，不会生成删除。

# 编辑器与办公软件的交换、锁与备份文件。保存时通常先写临时文件再 rename-over 成最终文件，
# 最终文件名不受这些规则影响（T27）。
*~
*.swp
*.swo
*.swx
.#*
\#*\#
~$*
*.tmp
*.temp

# 下载与传输中的中间文件。
*.part
*.partial
*.crdownload

# 桌面与系统的缓存元数据。
.DS_Store
._*
Thumbs.db
ehthumbs.db
desktop.ini
"#;

/// 绑定的一份排除规则集：内置默认清单与/或调用方文本解析后的匹配器。
#[derive(Clone, Debug)]
pub struct IgnoreRules {
    matcher: Gitignore,
}

impl IgnoreRules {
    /// 解析绑定规则文本，并把它叠加在内置默认清单之后。
    ///
    /// 默认清单先加载、调用方文本在后：按 Git 的“后面的规则优先”，绑定配置可以用 `!` 重新
    /// 包含默认清单排除的路径，也可以覆盖某条默认规则的结果。默认清单本身可编辑
    /// （[`DEFAULT_RULES`]）；需要完全替换默认清单时用 [`IgnoreRules::parse`]。
    ///
    /// 支持注释（`#` 开头）、空行、`!` 否定、`*`/`?`/`**`/字符类通配、前导 `/` 根锚定与尾部
    /// `/` 目录规则；首行 UTF-8 BOM 与 CRLF 换行按 Git 的习惯忽略。
    ///
    /// # Errors
    ///
    /// 任一行不是合法 glob 时返回 [`RuleIssue`]；解析错误不会被静默丢弃。行号相对于各自来源
    /// 文本（默认清单或调用方文本），不是两份文本拼接后的行号。
    pub fn parse_with_defaults(text: &str) -> Result<Self, RuleIssue> {
        Self::build(&[
            (DEFAULT_RULES, DEFAULT_RULES_LABEL, RuleSource::Defaults),
            (text, BINDING_RULES_LABEL, RuleSource::Binding),
        ])
    }

    /// 只解析给定文本：调用方提供完整清单，内置默认清单不参与。
    ///
    /// # Errors
    ///
    /// 同 [`IgnoreRules::parse_with_defaults`]。
    pub fn parse(text: &str) -> Result<Self, RuleIssue> {
        Self::build(&[(text, BINDING_RULES_LABEL, RuleSource::Binding)])
    }

    fn build(layers: &[(&str, &str, RuleSource)]) -> Result<Self, RuleIssue> {
        let mut builder = GitignoreBuilder::new(MATCH_ROOT);
        for (text, label, source) in layers {
            add_lines(&mut builder, text, label, *source)?;
        }
        let matcher = builder.build().map_err(|err| RuleIssue::Build {
            reason: err.to_string(),
        })?;
        Ok(IgnoreRules { matcher })
    }

    /// 判定一个 Vault 内路径是否参与同步，并给出命中的规则与原因。
    ///
    /// `is_dir` 必须是该路径在 Vault 里的条目类型：带尾部 `/` 的规则只匹配目录。祖先目录按
    /// 从浅到深的顺序检查，因此父目录被排除后，子项的 `!` 规则不会生效（Git 的父目录限制，
    /// design §10.4）；被阻断的否定规则记录在 [`Exclusion::blocked_reinclude`]。
    #[must_use]
    pub fn decide(&self, path: &RelativePath, is_dir: bool) -> Decision {
        let components = path.components();
        let mut candidate = String::new();
        let mut reinclude = None;
        for (index, component) in components.iter().enumerate() {
            if index > 0 {
                candidate.push('/');
            }
            candidate.push_str(component);
            let is_last = index + 1 == components.len();
            // 中间分量一定是目录；只有查询路径自身按 `is_dir` 判定。
            let matched = self
                .matcher
                .matched(Path::new(&candidate), if is_last { is_dir } else { true });
            match matched {
                Match::Ignore(glob) => {
                    if is_last {
                        return Decision::Excluded(Exclusion {
                            hit: hit(glob, MatchedAt::Path),
                            blocked_reinclude: None,
                        });
                    }
                    return Decision::Excluded(Exclusion {
                        hit: hit(glob, MatchedAt::Ancestor(path.prefix(index + 1))),
                        blocked_reinclude: self.path_reinclude(path, is_dir),
                    });
                }
                Match::Whitelist(glob) if is_last => {
                    return Decision::Included {
                        reinclude: Some(hit(glob, MatchedAt::Path)),
                    };
                }
                // 目录被否定规则重新包含：记下这条规则，继续检查更深的分量。
                Match::Whitelist(glob) => {
                    reinclude = Some(hit(glob, MatchedAt::Ancestor(path.prefix(index + 1))));
                }
                Match::None => {}
            }
        }
        Decision::Included { reinclude }
    }

    /// 只需要“是否参与同步”的布尔结果时用它（等价于 [`IgnoreRules::decide`] 的判定）。
    #[must_use]
    pub fn is_excluded(&self, path: &RelativePath, is_dir: bool) -> bool {
        matches!(self.decide(path, is_dir), Decision::Excluded(_))
    }

    /// 查询路径自身命中的否定规则：祖先目录已被排除时它不生效，但值得在解释里给出。
    fn path_reinclude(&self, path: &RelativePath, is_dir: bool) -> Option<Hit> {
        match self.matcher.matched(Path::new(&path.to_string()), is_dir) {
            Match::Whitelist(glob) => Some(hit(glob, MatchedAt::Path)),
            Match::Ignore(_) | Match::None => None,
        }
    }
}

/// 规则来源：内置默认清单，或调用方提供的规则文本（绑定的 `.syncignore`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleSource {
    /// 内置默认清单 [`DEFAULT_RULES`]。
    Defaults,
    /// 调用方传入的规则文本（绑定级配置）。
    Binding,
}

impl fmt::Display for RuleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RuleSource::Defaults => "默认清单",
            RuleSource::Binding => "绑定规则",
        })
    }
}

/// 命中的规则：原文、来源与命中位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// 规则原文，保留 `!` 前缀与目录尾 `/`，直接用于回答“为什么被排除”。
    pub rule: String,
    /// 规则来自内置默认清单还是绑定规则文本。
    pub source: RuleSource,
    /// 命中的是查询路径自身，还是它的某个祖先目录。
    pub at: MatchedAt,
}

/// 规则命中的位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchedAt {
    /// 命中查询路径自身。
    Path,
    /// 命中它的祖先目录；值是该祖先相对 Vault 的路径。
    Ancestor(RelativePath),
}

/// 被排除的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exclusion {
    /// 命中的排除规则。
    pub hit: Hit,
    /// 查询路径自身命中的否定规则，因祖先目录已被排除而不能重新包含（Git 限制，design §10.4）。
    ///
    /// 只在 [`MatchedAt::Ancestor`] 命中时可能非空；它的 `at` 恒为 [`MatchedAt::Path`]。
    pub blocked_reinclude: Option<Hit>,
}

/// 一个 Vault 内路径相对排除规则的裁决。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// 参与同步。
    Included {
        /// 让它重新参与同步的否定规则（`!` 规则）：可能命中查询路径自身，也可能命中被
        /// 重新包含的祖先目录（见 [`MatchedAt`]）。没有否定规则命中时为 `None`，用于解释
        /// “为什么它没被排除”。
        reinclude: Option<Hit>,
    },
    /// 被排除：不参与此绑定的同步。**这不是删除**（见模块文档）。
    Excluded(Exclusion),
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Decision::Included { reinclude: None } => f.write_str("参与同步：没有排除规则命中"),
            Decision::Included {
                reinclude: Some(hit),
            } => match &hit.at {
                MatchedAt::Path => write!(
                    f,
                    "参与同步：{} 的否定规则 {} 重新包含",
                    hit.source, hit.rule
                ),
                MatchedAt::Ancestor(ancestor) => write!(
                    f,
                    "参与同步：祖先目录 {ancestor} 命中 {} 的否定规则 {}",
                    hit.source, hit.rule
                ),
            },
            Decision::Excluded(exclusion) => {
                let hit = &exclusion.hit;
                match &hit.at {
                    MatchedAt::Path => write!(
                        f,
                        "不参与同步：{} 的规则 {} 命中该路径",
                        hit.source, hit.rule
                    )?,
                    MatchedAt::Ancestor(ancestor) => write!(
                        f,
                        "不参与同步：祖先目录 {ancestor} 命中 {} 的规则 {}",
                        hit.source, hit.rule
                    )?,
                }
                if let Some(blocked) = &exclusion.blocked_reinclude {
                    write!(
                        f,
                        "；否定规则 {} 命中该路径，但父目录已被排除，不能重新包含（Git 限制）",
                        blocked.rule
                    )?;
                }
                Ok(())
            }
        }
    }
}

/// 规则文本无法解析。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleIssue {
    /// 某一行不是合法 glob。
    Line {
        /// 该行来自哪个来源。
        source: RuleSource,
        /// 在该来源文本里的行号（1 基）。
        line: usize,
        /// 该行原文。
        text: String,
        /// 匹配库给出的失败原因。
        reason: String,
    },
    /// 每一行都合法，但整份规则集构建失败。
    Build {
        /// 匹配库给出的失败原因。
        reason: String,
    },
}

impl fmt::Display for RuleIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuleIssue::Line {
                source,
                line,
                text,
                reason,
            } => write!(f, "{source} 第 {line} 行的规则 `{text}` 无法解析：{reason}"),
            RuleIssue::Build { reason } => write!(f, "规则集无法构建：{reason}"),
        }
    }
}

impl Error for RuleIssue {}

/// 逐行加入一份规则文本；`str::lines` 已去掉行尾 `\r`（CRLF 文本）。
fn add_lines(
    builder: &mut GitignoreBuilder,
    text: &str,
    label: &str,
    source: RuleSource,
) -> Result<(), RuleIssue> {
    // Windows 编辑器常给 UTF-8 文件加 BOM；Git 也把首行 BOM 当格式标记，而不是规则内容。
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let from = PathBuf::from(label);
    for (index, line) in text.lines().enumerate() {
        builder
            .add_line(Some(from.clone()), line)
            .map_err(|err| RuleIssue::Line {
                source,
                line: index + 1,
                text: line.to_owned(),
                reason: err.to_string(),
            })?;
    }
    Ok(())
}

/// 把匹配库命中的 glob 转成本模块的解释结构。
fn hit(glob: &Glob, at: MatchedAt) -> Hit {
    Hit {
        rule: glob.original().to_owned(),
        source: match glob.from() {
            Some(path) if path == Path::new(DEFAULT_RULES_LABEL) => RuleSource::Defaults,
            _ => RuleSource::Binding,
        },
        at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> RelativePath {
        RelativePath::parse(text).expect("路径应通过校验")
    }

    fn rules(text: &str) -> IgnoreRules {
        IgnoreRules::parse(text).expect("规则文本应可解析")
    }

    fn parse_with_defaults(text: &str) -> IgnoreRules {
        IgnoreRules::parse_with_defaults(text).expect("默认清单与规则文本应可解析")
    }

    fn defaults() -> IgnoreRules {
        parse_with_defaults("")
    }

    fn decide(rules: &IgnoreRules, text: &str, is_dir: bool) -> Decision {
        rules.decide(&path(text), is_dir)
    }

    fn excluded(rules: &IgnoreRules, text: &str, is_dir: bool) -> Exclusion {
        match decide(rules, text, is_dir) {
            Decision::Excluded(exclusion) => exclusion,
            other => panic!("{text} 应被排除，实际是 {other}"),
        }
    }

    fn included(rules: &IgnoreRules, text: &str, is_dir: bool) -> Option<Hit> {
        match decide(rules, text, is_dir) {
            Decision::Included { reinclude } => reinclude,
            other => panic!("{text} 应参与同步，实际是 {other}"),
        }
    }

    #[test]
    fn parses_comments_blank_lines_and_escaped_prefix() {
        let rules = rules("# 注释行不是规则\n\n#ignored.txt 也仍然是注释\n\\#notes#\n");
        assert_eq!(excluded(&rules, "#notes#", false).hit.rule, r"\#notes#");
        // 注释不生效：两个长得像规则的文件照常参与同步。
        included(&rules, "ignored.txt", false);
        included(&rules, "a/b#notes#", false);
    }

    #[test]
    fn wildcards_cover_names_and_paths() {
        let rules = rules("*.log\n?.txt\ndocs/**/draft.md\n[0-9][0-9]-report.md\n");
        assert_eq!(excluded(&rules, "a.log", false).hit.rule, "*.log");
        assert_eq!(excluded(&rules, "a/b.log", false).hit.rule, "*.log");
        assert_eq!(excluded(&rules, "x.txt", false).hit.rule, "?.txt");
        included(&rules, "xy.txt", false);
        // `**` 匹配零层或多层目录。
        assert_eq!(
            excluded(&rules, "docs/draft.md", false).hit.rule,
            "docs/**/draft.md"
        );
        excluded(&rules, "docs/a/b/draft.md", false);
        included(&rules, "docs/draft.md.bak", false);
        excluded(&rules, "12-report.md", false);
        included(&rules, "x2-report.md", false);
    }

    #[test]
    fn root_anchored_rules_only_match_at_the_root() {
        let rules = rules("/root-only.txt\nsub/anchored.txt\n");
        assert_eq!(
            excluded(&rules, "root-only.txt", false).hit.rule,
            "/root-only.txt"
        );
        included(&rules, "sub/root-only.txt", false);
        // 含 `/` 的规则按 Git 规则锚定在 Vault，而不是任意深度。
        excluded(&rules, "sub/anchored.txt", false);
        included(&rules, "x/sub/anchored.txt", false);
    }

    #[test]
    fn directory_rules_only_match_directories() {
        let rules = rules("build/\n");
        let exclusion = excluded(&rules, "build", true);
        assert_eq!(exclusion.hit.rule, "build/");
        assert_eq!(exclusion.hit.at, MatchedAt::Path);
        // 同名文件不受目录规则影响。
        included(&rules, "build", false);
        excluded(&rules, "src/build", true);
        // 目录被排除后，子项命中同一个祖先规则。
        let child = excluded(&rules, "build/x.txt", false);
        assert_eq!(child.hit.rule, "build/");
        assert_eq!(child.hit.at, MatchedAt::Ancestor(path("build")));
        assert_eq!(child.blocked_reinclude, None);
    }

    #[test]
    fn negation_reincludes_paths() {
        let rules = rules("*.log\n!keep.log\n");
        excluded(&rules, "a.log", false);
        let reinclude = included(&rules, "keep.log", false).expect("否定规则应命中");
        assert_eq!(reinclude.rule, "!keep.log");
        assert_eq!(reinclude.source, RuleSource::Binding);
        assert_eq!(reinclude.at, MatchedAt::Path);
    }

    /// 验收项：父目录排除 / 子项否定。Git 不允许在被排除的父目录里重新包含子项。
    #[test]
    fn excluded_parent_directory_blocks_child_reinclude() {
        let secrets = rules("secrets/\n!secrets/public.txt\n");
        let child = excluded(&secrets, "secrets/public.txt", false);
        assert_eq!(child.hit.rule, "secrets/");
        assert_eq!(child.hit.at, MatchedAt::Ancestor(path("secrets")));
        let blocked = child.blocked_reinclude.expect("应报告被阻断的否定规则");
        assert_eq!(blocked.rule, "!secrets/public.txt");
        assert_eq!(blocked.at, MatchedAt::Path);

        // 更深一层的子项同样被祖先目录排除，且没有自己的否定规则。
        let deep = excluded(&secrets, "secrets/a/b.txt", false);
        assert_eq!(deep.hit.at, MatchedAt::Ancestor(path("secrets")));
        assert_eq!(deep.blocked_reinclude, None);

        // 重新包含父目录本身后，子项就能按规则参与同步，解释里记录的是祖先目录的否定规则。
        let reopened = rules("build/\n!build/\n");
        let dir_hit = included(&reopened, "build", true).expect("目录自身的否定规则");
        assert_eq!(dir_hit.rule, "!build/");
        let child_hit = included(&reopened, "build/x.txt", false).expect("祖先目录的否定规则");
        assert_eq!(child_hit.rule, "!build/");
        assert_eq!(child_hit.at, MatchedAt::Ancestor(path("build")));
    }

    #[test]
    fn reports_matched_rule_and_source() {
        let with_defaults = parse_with_defaults("!keep.tmp\n");
        let exclusion = excluded(&with_defaults, "a.tmp", false);
        assert_eq!(exclusion.hit.rule, "*.tmp");
        assert_eq!(exclusion.hit.source, RuleSource::Defaults);
        let reinclude = included(&with_defaults, "keep.tmp", false).expect("绑定规则应重新包含");
        assert_eq!(reinclude.rule, "!keep.tmp");
        assert_eq!(reinclude.source, RuleSource::Binding);

        // 只用绑定文本时默认清单不参与。
        let only_binding = rules("a/\n");
        included(&only_binding, "a.tmp", false);
    }

    /// 验收项：临时文件与 rename-over 的最终文件（T27）。默认规则只认临时文件名。
    #[test]
    fn default_rules_skip_temp_files_but_keep_the_final_file() {
        let rules = defaults();
        assert_eq!(excluded(&rules, "report.docx.tmp", false).hit.rule, "*.tmp");
        assert_eq!(excluded(&rules, "~$report.docx", false).hit.rule, "~$*");
        assert_eq!(excluded(&rules, "report.docx~", false).hit.rule, "*~");
        assert_eq!(excluded(&rules, "#report.docx#", false).hit.rule, r"\#*\#");
        assert_eq!(excluded(&rules, ".DS_Store", false).hit.rule, ".DS_Store");
        assert_eq!(excluded(&rules, "Thumbs.db", false).hit.rule, "Thumbs.db");
        assert_eq!(excluded(&rules, "big.iso.part", false).hit.rule, "*.part");

        // rename-over 之后最终文件必须参与同步：默认规则不能把最终内容吞掉。
        for is_dir in [false, true] {
            assert_eq!(
                decide(&rules, "report.docx", is_dir),
                Decision::Included { reinclude: None }
            );
        }
        included(&rules, "notes.txt", false);
        // 规则文件本身不被默认清单排除：避免“排除规则被自己排除而无法更新”的循环（design §10.4）。
        included(&rules, ".syncignore", false);
        // 不排除任意隐藏文件，也不排除整个 `.git`。
        included(&rules, ".hidden", false);
        included(&rules, ".git/config", false);
        assert!(!rules.is_excluded(&path(".git"), true));
    }

    #[test]
    fn accepts_bom_and_crlf_rule_text() {
        let rules = rules("\u{feff}# 注释\r\n*.tmp\r\n!keep.tmp\r\n");
        assert_eq!(excluded(&rules, "a.tmp", false).hit.rule, "*.tmp");
        included(&rules, "keep.tmp", false);
    }

    #[test]
    fn rejects_unparsable_rule_lines_with_source_line_numbers() {
        assert_eq!(
            IgnoreRules::parse("*.log\n[z-a]\n").expect_err("非法规则应被拒绝"),
            RuleIssue::Line {
                source: RuleSource::Binding,
                line: 2,
                text: "[z-a]".to_owned(),
                reason: "error parsing glob '[z-a]': invalid range; 'z' > 'a'".to_owned(),
            }
        );
        // 行号相对各自来源：叠加默认清单时用户文本仍从第 1 行开始计数。
        let issue = IgnoreRules::parse_with_defaults("[z-a]").expect_err("非法规则应被拒绝");
        let message = issue.to_string();
        match &issue {
            RuleIssue::Line {
                source,
                line,
                text,
                reason,
            } => {
                assert_eq!(*source, RuleSource::Binding);
                assert_eq!(*line, 1);
                assert_eq!(text, "[z-a]");
                assert!(reason.contains("invalid range"), "原因是 {reason}");
                assert!(message.contains("绑定规则 第 1 行"), "文案是 {message}");
            }
            other => panic!("应报行级问题，实际是 {other}"),
        }
    }

    #[test]
    fn empty_rules_include_everything() {
        for text in ["", "\n\n", "# 只有注释\n"] {
            let rules = rules(text);
            assert_eq!(
                decide(&rules, "a/b.txt", false),
                Decision::Included { reinclude: None }
            );
            assert!(matches!(
                decide(&rules, "a", true),
                Decision::Included { reinclude: None }
            ));
        }
    }

    /// 验收项：相同规则结果确定（重复解析、重复查询、查询顺序都不影响裁决）。
    #[test]
    fn decisions_are_deterministic() {
        let text = "build/\n*.log\n!keep.log\nsecrets/\n!secrets/public.txt\n";
        let first = rules(text);
        let second = rules(text);
        let cases = [
            ("build", true),
            ("build/x.txt", false),
            ("a.log", false),
            ("keep.log", false),
            ("secrets/public.txt", false),
            ("notes.txt", false),
        ];
        let forward: Vec<Decision> = cases
            .iter()
            .map(|(text, is_dir)| decide(&first, text, *is_dir))
            .collect();
        let backward: Vec<Decision> = cases
            .iter()
            .rev()
            .map(|(text, is_dir)| decide(&first, text, *is_dir))
            .rev()
            .collect();
        let other: Vec<Decision> = cases
            .iter()
            .map(|(text, is_dir)| decide(&second, text, *is_dir))
            .collect();
        assert_eq!(forward, backward);
        assert_eq!(forward, other);
        // 同一个查询重复执行也得到同一结论。
        assert_eq!(decide(&first, "build/x.txt", false), forward[1]);
    }

    #[test]
    fn display_explains_the_reason() {
        let secrets = rules("secrets/\n!secrets/public.txt\n");
        let text = decide(&secrets, "secrets/public.txt", false).to_string();
        assert!(text.contains("不参与同步"), "文案是 {text}");
        assert!(text.contains("祖先目录 secrets"), "文案是 {text}");
        assert!(text.contains("secrets/"), "文案是 {text}");
        assert!(text.contains("!secrets/public.txt"), "文案是 {text}");
        assert!(text.contains("Git 限制"), "文案是 {text}");

        let included_text = decide(&secrets, "notes.txt", false).to_string();
        assert!(included_text.contains("参与同步"), "文案是 {included_text}");
        let reinclude_text = decide(&rules("*.log\n!keep.log\n"), "keep.log", false).to_string();
        assert!(
            reinclude_text.contains("!keep.log"),
            "文案是 {reinclude_text}"
        );
    }
}
