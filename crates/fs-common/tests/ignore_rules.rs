//! 排除规则的固定样本验收（S018）：`fixtures/paths/` 里的绑定级 `.syncignore` 与用例表。
//!
//! 用例表每行一条断言：`路径 | 类型 | 期望 | 命中的规则 | 命中位置 | 被阻断的否定规则`
//! （列含义见 `fixtures/paths/syncignore-cases.txt` 头部）。失败时报出行号与原始用例。
//! 改动样本规则必须同时改用例表，否则这里会直接失败。
//!
//! 这里只用 `IgnoreRules::parse`：样本是绑定级配置，内置默认清单不参与；默认清单自己的
//! 临时文件/最终文件边界由 `src/ignore.rs` 的单测覆盖（T27）。

use vaultsyncd_fs_common::ignore::{Decision, IgnoreRules, MatchedAt, RuleSource};
use vaultsyncd_fs_common::path::RelativePath;

const RULES_FIXTURE: &str = include_str!("../../../fixtures/paths/syncignore-binding.syncignore");
const CASES_FIXTURE: &str = include_str!("../../../fixtures/paths/syncignore-cases.txt");

/// 用例表的一行。
struct Case {
    /// 用例在用例表里的行号（1 基），用于失败报告。
    line_number: usize,
    /// Root 相对路径。
    path: &'static str,
    /// 条目类型：目录为 `true`。目录规则只匹配目录。
    is_dir: bool,
    /// 期望是否被排除。
    excluded: bool,
    /// 期望命中的规则（`include` 时是重新包含的否定规则）。
    rule: Option<&'static str>,
    /// 期望的命中位置：`self` 或 `ancestor:<路径>`。
    at: Option<&'static str>,
    /// 期望被阻断的否定规则。
    blocked: Option<&'static str>,
}

fn parse_cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for (index, raw) in CASES_FIXTURE.lines().enumerate() {
        let line_number = index + 1;
        let line = raw.trim();
        // 注释行以 `# ` 开头或整行只有 `#`；`#notes#` 这类以 `#` 开头的路径仍是用例。
        if line.is_empty() || line == "#" || line.starts_with("# ") {
            continue;
        }
        let fields: Vec<&str> = line.split('|').map(str::trim).collect();
        assert_eq!(
            fields.len(),
            6,
            "用例表第 {line_number} 行应有 6 列，实际 {}：{line}",
            fields.len()
        );
        let is_dir = match fields[1] {
            "file" => false,
            "dir" => true,
            other => panic!("用例表第 {line_number} 行的类型 `{other}` 不是 file/dir"),
        };
        let excluded = match fields[2] {
            "include" => false,
            "exclude" => true,
            other => panic!("用例表第 {line_number} 行的期望 `{other}` 不是 include/exclude"),
        };
        cases.push(Case {
            line_number,
            path: fields[0],
            is_dir,
            excluded,
            rule: dash(fields[3]),
            at: dash(fields[4]),
            blocked: dash(fields[5]),
        });
    }
    cases
}

/// `-` 表示“没有”，其余原样返回。
fn dash(field: &'static str) -> Option<&'static str> {
    if field == "-" { None } else { Some(field) }
}

/// 裁决里的命中位置是否与用例表的 `self` / `ancestor:<路径>` 一致。
fn at_matches(at: &MatchedAt, expected: &str) -> bool {
    match (at, expected) {
        (MatchedAt::Path, "self") => true,
        (MatchedAt::Ancestor(ancestor), expected) => expected
            .strip_prefix("ancestor:")
            .is_some_and(|expected| ancestor.to_string() == expected),
        (MatchedAt::Path, _) => false,
    }
}

#[test]
fn fixture_cases_match_the_binding_rules() {
    let rules = IgnoreRules::parse(RULES_FIXTURE).expect("样本规则应可解析");
    let cases = parse_cases();
    assert!(cases.len() >= 18, "用例太少：{} 条", cases.len());
    for case in &cases {
        let context = format!("用例表第 {} 行：{}", case.line_number, case.path);
        let path = RelativePath::parse(case.path)
            .unwrap_or_else(|issue| panic!("{context} 的路径不合法：{issue}"));
        let decision = rules.decide(&path, case.is_dir);
        match (&decision, case.excluded) {
            (Decision::Excluded(exclusion), true) => {
                let rule = case
                    .rule
                    .unwrap_or_else(|| panic!("{context} 期望被排除，却没给出命中的规则"));
                assert_eq!(exclusion.hit.rule, rule, "{context}");
                assert_eq!(exclusion.hit.source, RuleSource::Binding, "{context}");
                let at = case.at.expect("被排除的用例必须给出命中位置");
                assert!(at_matches(&exclusion.hit.at, at), "{context} 命中位置不符");
                assert_eq!(
                    exclusion
                        .blocked_reinclude
                        .as_ref()
                        .map(|hit| hit.rule.as_str()),
                    case.blocked,
                    "{context} 被阻断的否定规则不符"
                );
            }
            (Decision::Included { reinclude }, false) => {
                assert_eq!(
                    reinclude.as_ref().map(|hit| hit.rule.as_str()),
                    case.rule,
                    "{context} 的重新包含规则不符"
                );
                if let Some(hit) = reinclude {
                    assert_eq!(hit.at, MatchedAt::Path, "{context} 的命中位置不符");
                }
            }
            (Decision::Excluded(exclusion), false) => {
                panic!(
                    "{context} 期望参与同步，实际被规则 {} 排除",
                    exclusion.hit.rule
                )
            }
            (Decision::Included { .. }, true) => panic!("{context} 期望被排除，实际参与同步"),
        }
        // “是否排除”的布尔入口与解释入口必须一致。
        assert_eq!(
            rules.is_excluded(&path, case.is_dir),
            case.excluded,
            "{context} 的 is_excluded 与 decide 不一致"
        );
    }
}
