//! 路径关系判定：一律按路径分量比较，不用字符串前缀（design §16.1）。
//!
//! `/data/a` 不是 `/data/abc` 的父目录，`/data/a/` 与 `/data/a` 相同。调用方负责先规范化
//! （本包的探针用 `canonicalize`）；本模块不解析符号链接，也不词法消解 `..`。

use std::path::{Component, Path, PathBuf};

/// 两个路径的分量关系（`Ancestor` 表示前者是后者的祖先）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRelation {
    /// 完全相同的路径。
    Same,
    /// 前者是后者的祖先目录。
    Ancestor,
    /// 后者是前者的祖先目录。
    Descendant,
    /// 两者互不包含。
    Unrelated,
}

/// 按路径分量返回 `a` 相对 `b` 的关系。
#[must_use]
pub fn relation(a: &Path, b: &Path) -> PathRelation {
    let a: Vec<Component<'_>> = a.components().collect();
    let b: Vec<Component<'_>> = b.components().collect();
    let common = a
        .iter()
        .zip(b.iter())
        .take_while(|(left, right)| left == right)
        .count();
    if common == a.len() && common == b.len() {
        PathRelation::Same
    } else if common == a.len() {
        PathRelation::Ancestor
    } else if common == b.len() {
        PathRelation::Descendant
    } else {
        PathRelation::Unrelated
    }
}

/// `path` 是否等于 `base` 或位于 `base` 之下。
#[must_use]
pub fn is_within(path: &Path, base: &Path) -> bool {
    matches!(
        relation(path, base),
        PathRelation::Same | PathRelation::Descendant
    )
}

/// 取 `path` 相对 `base` 的路径分量；不在其下或与之相同时返回 `None`。
///
/// 用 `?` 语义保持“只接受严格后代”，让调用方能区分“Vault 自己”和“Vault 内的东西”。
#[must_use]
pub fn strip_prefix(path: &Path, base: &Path) -> Option<PathBuf> {
    path.strip_prefix(base)
        .ok()
        .filter(|rest| !rest.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_wise_comparison_is_not_string_prefix() {
        assert_eq!(
            relation(Path::new("/data/a"), Path::new("/data/abc")),
            PathRelation::Unrelated
        );
        assert_eq!(
            relation(Path::new("/data"), Path::new("/data/a")),
            PathRelation::Ancestor
        );
        assert_eq!(
            relation(Path::new("/data/a/b"), Path::new("/data")),
            PathRelation::Descendant
        );
        assert_eq!(
            relation(Path::new("/data/a/"), Path::new("/data/a")),
            PathRelation::Same
        );
        assert_eq!(
            relation(Path::new("/"), Path::new("/data")),
            PathRelation::Ancestor
        );
        assert_eq!(
            relation(Path::new("/data"), Path::new("/data")),
            PathRelation::Same
        );
    }

    #[test]
    fn strip_prefix_only_returns_strict_descendants() {
        assert_eq!(
            strip_prefix(Path::new("/data/vault/sub/dir"), Path::new("/data/vault")),
            Some(PathBuf::from("sub/dir"))
        );
        assert_eq!(
            strip_prefix(Path::new("/data/vault"), Path::new("/data/vault")),
            None
        );
        assert_eq!(
            strip_prefix(Path::new("/data/vaultx"), Path::new("/data/vault")),
            None
        );
        assert!(is_within(
            Path::new("/data/vault"),
            Path::new("/data/vault")
        ));
        assert!(!is_within(Path::new("/data"), Path::new("/data/vault")));
    }
}
