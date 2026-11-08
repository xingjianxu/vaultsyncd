//! 离线恢复的纯规则：兼容、Root新映射、命名空间与空间预算；无平台I/O。
use super::export::Index;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};
use vaultsyncd_domain::ids::{EntryId, RootId};
use vaultsyncd_fs_common::name::validate_name;

/// 不满足条件时不得写入目标或声明成功。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// 缺字段、类型/身份/命名空间错误。
    Invalid,
    /// 不支持的DB或协议契约版本。
    Incompatible,
    /// 非规范绝对路径、映射缺失/重复、路径重叠。
    Mapping,
    /// 空间不足或预算算术溢出。
    Space,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
/// 恢复程序实际支持的版本范围；产品版本不代替DB/协议版本。
pub struct Compatibility<'a> {
    /// 最早支持的备份DB。
    pub minimum_schema: i64,
    /// 本程序迁移目标。
    pub maximum_schema: i64,
    /// 稳定的协议契约版本，独立于契约内容摘要。
    pub protocol: &'a str,
}
/// 兼容检查不将备份所属产品patch或契约摘要变化直接当成schema变化。
pub fn compatible(
    manifest: &Value,
    index: &Index,
    supported: Compatibility<'_>,
) -> Result<(), Error> {
    if index.schema_version < supported.minimum_schema
        || index.schema_version > supported.maximum_schema
        || manifest["protocol_contract_version"].as_str() != Some(supported.protocol)
    {
        return Err(Error::Incompatible);
    }
    if manifest["product_version"]
        .as_str()
        .is_none_or(str::is_empty)
        || manifest["protocol_contract_sha256"]
            .as_str()
            .is_none_or(|s| {
                s.len() != 64
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            })
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
/// 请求的新Root绝对路径（内部参数，不另造wire DTO）。
#[derive(Debug, Clone)]
pub struct Mapping {
    /// 固定Root身份。
    pub root: RootId,
    /// 经过平台解析后仍需纯路径关系复核的新路径。
    pub path: PathBuf,
}
/// 规范绝对路径不含别名分量、控制字符；平台还必须拒绝符号链接和目录身份别名。
pub fn canonical_path(path: &Path) -> bool {
    // 这是Linux服务端路径语义；便携核心在Windows上也不能把POSIX根误判成相对盘路径。
    matches!(path.components().next(), Some(Component::RootDir))
        && path.parent().is_some()
        && path
            .to_str()
            .is_some_and(|s| !s.chars().any(char::is_control))
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
/// 所有Root恰好映射一次，互不重叠，也不能覆盖状态、归档或原工作目录。
pub fn mappings(
    index: &Index,
    manifest: &Value,
    state: &Path,
    archive: &Path,
    requested: &[Mapping],
) -> Result<BTreeMap<RootId, PathBuf>, Error> {
    if !canonical_path(state) || !canonical_path(archive) || overlaps(state, archive) {
        return Err(Error::Mapping);
    }
    let originals: Vec<PathBuf> = manifest["roots"]
        .as_array()
        .ok_or(Error::Invalid)?
        .iter()
        .map(|r| {
            r["original_path"]
                .as_str()
                .map(PathBuf::from)
                .ok_or(Error::Invalid)
        })
        .collect::<Result<_, _>>()?;
    let mut result = BTreeMap::new();
    for m in requested {
        if !canonical_path(&m.path)
            || !index.root_seqs.contains_key(&m.root)
            || result.insert(m.root, m.path.clone()).is_some()
            || overlaps(&m.path, state)
            || overlaps(&m.path, archive)
            || originals.iter().any(|p| overlaps(p, &m.path))
        {
            return Err(Error::Mapping);
        }
    }
    if result.len() != index.root_seqs.len() {
        return Err(Error::Mapping);
    }
    let paths: Vec<_> = result.values().collect();
    for (i, p) in paths.iter().enumerate() {
        if paths.iter().skip(i + 1).any(|q| overlaps(p, q)) {
            return Err(Error::Mapping);
        }
    }
    Ok(result)
}
/// 活跃命名空间条目，已删除条目由历史/回收站保留，不写回工作目录。
#[derive(Debug, Clone)]
pub struct Entry {
    /// 条目身份。
    pub id: EntryId,
    /// 所属Root。
    pub root: RootId,
    /// 仅合成Root条目无父。
    pub parent: Option<EntryId>,
    /// 原始安全名称。
    pub name: String,
    /// 目录没有内容。
    pub directory: bool,
    /// 普通文件必须有整份内容摘要和大小。
    pub content: Option<([u8; 32], u64)>,
}
/// 深度优先级已排序的普通工作项。
#[derive(Debug, Clone)]
pub struct Work {
    /// 所属Root。
    pub root: RootId,
    /// Root内安全相对路径。
    pub path: PathBuf,
    /// None为目录，Some为完整普通文件。
    pub content: Option<([u8; 32], u64)>,
}
/// 检查唯一合成根、父子Root一致、环/缺父/不安全名和清单对象完备，输出父在前的工作计划。
pub fn namespace(index: &Index, entries: Vec<Entry>) -> Result<Vec<Work>, Error> {
    let mut nodes = BTreeMap::new();
    let mut roots = BTreeSet::new();
    for e in entries {
        if !index.root_seqs.contains_key(&e.root) || e.directory == e.content.is_some() {
            return Err(Error::Invalid);
        }
        if e.parent.is_none() {
            if !e.directory || !e.name.is_empty() || !roots.insert(e.root) {
                return Err(Error::Invalid);
            }
        } else if validate_name(&e.name).is_err() {
            return Err(Error::Invalid);
        }
        if nodes.insert(e.id, e).is_some() {
            return Err(Error::Invalid);
        }
    }
    if roots.len() != index.root_seqs.len() {
        return Err(Error::Invalid);
    }
    let mut work = Vec::new();
    let mut unique = BTreeSet::new();
    for e in nodes.values().filter(|e| e.parent.is_some()) {
        let mut components = vec![e.name.clone()];
        let mut seen = BTreeSet::new();
        seen.insert(e.id);
        let mut node = e;
        while let Some(parent) = node.parent {
            if !seen.insert(parent) {
                return Err(Error::Invalid);
            }
            let p = nodes.get(&parent).ok_or(Error::Invalid)?;
            if p.root != e.root || !p.directory {
                return Err(Error::Invalid);
            }
            if p.parent.is_some() {
                components.push(p.name.clone())
            }
            node = p;
        }
        components.reverse();
        let path: PathBuf = components.iter().collect();
        if !unique.insert((e.root, path.clone())) {
            return Err(Error::Invalid);
        }
        if let Some((hash, size)) = e.content {
            let object = format!(
                "objects/{}/{}",
                e.root.to_hex(),
                crate::objects::ContentDigest::from_bytes(hash).to_hex()
            );
            if index
                .item(&object)
                .is_none_or(|i| i.hash != hash || i.size != size)
            {
                return Err(Error::Invalid);
            }
        }
        work.push(Work {
            root: e.root,
            path,
            content: e.content,
        });
    }
    work.sort_by(|a, b| {
        a.path
            .components()
            .count()
            .cmp(&b.path.components().count())
            .then(a.root.cmp(&b.root))
            .then(a.path.cmp(&b.path))
    });
    Ok(work)
}
/// 状态与工作字节的保守容量预算（复制余量两倍，另留64MiB用于DB/WAL和元数据）。
pub fn required(index: &Index, work: &[Work]) -> Result<u64, Error> {
    let imported = index
        .items
        .iter()
        .try_fold(0u64, |n, i| n.checked_add(i.size).ok_or(Error::Space))?;
    let active = work.iter().try_fold(0u64, |n, w| {
        n.checked_add(w.content.map_or(0, |(_, n)| n))
            .ok_or(Error::Space)
    })?;
    imported
        .checked_add(active)
        .and_then(|n| n.checked_mul(2))
        .and_then(|n| n.checked_add(64 * 1024 * 1024))
        .ok_or(Error::Space)
}
/// 同一合规btrfs文件系统的可用字节预算不足即拒绝，实际写入失败也不能发布成功。
pub fn space(required: u64, available: u64) -> Result<(), Error> {
    if required > available {
        Err(Error::Space)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::export::Item;
    use vaultsyncd_domain::ids::{BackupLeaseId, ServerId};
    fn index() -> Index {
        let root = RootId::from_bytes([1; 16]);
        Index {
            id: BackupLeaseId::from_bytes([2; 16]),
            server: ServerId::from_bytes([3; 16]),
            manifest_hash: [0; 32],
            items: vec![Item {
                path: format!("objects/{}/{}", root.to_hex(), "04".repeat(32)),
                hash: [4; 32],
                size: 5,
            }],
            root_seqs: BTreeMap::from([(root, 1)]),
            schema_version: 13,
            table_counts: BTreeMap::new(),
            object_storage_paths: BTreeMap::from([(
                format!("objects/{}/{}", root.to_hex(), "04".repeat(32)),
                format!("objects/04/{}", "04".repeat(32)),
            )]),
        }
    }
    fn root() -> Entry {
        Entry {
            id: EntryId::from_bytes([1; 16]),
            root: RootId::from_bytes([1; 16]),
            parent: None,
            name: String::new(),
            directory: true,
            content: None,
        }
    }
    fn file() -> Entry {
        Entry {
            id: EntryId::from_bytes([2; 16]),
            parent: Some(root().id),
            name: "note".into(),
            directory: false,
            content: Some(([4; 32], 5)),
            ..root()
        }
    }
    #[test]
    fn namespace_needs_real_complete_objects_and_safe_acyclic_parents() {
        let i = index();
        let work = namespace(&i, vec![file(), root()]).unwrap();
        assert_eq!(work[0].path, Path::new("note"));
        let mut bad = file();
        bad.name = "../escape".into();
        assert_eq!(
            namespace(&i, vec![root(), bad]).unwrap_err(),
            Error::Invalid
        );
        let mut bad = file();
        bad.parent = Some(bad.id);
        assert!(namespace(&i, vec![root(), bad]).is_err());
        let mut i = index();
        i.items.clear();
        assert!(namespace(&i, vec![root(), file()]).is_err());
    }
    #[test]
    fn mapping_requires_all_roots_and_rejects_source_state_and_target_overlap() {
        let i = index();
        let v = serde_json::json!({"roots":[{"original_path":"/old/work"}]});
        let m = Mapping {
            root: root().root,
            path: "/new/work".into(),
        };
        assert!(
            mappings(
                &i,
                &v,
                Path::new("/new/state"),
                Path::new("/backup/a.vsbk"),
                std::slice::from_ref(&m)
            )
            .is_ok()
        );
        assert!(mappings(&i, &v, Path::new("/new"), Path::new("/backup/a.vsbk"), &[m]).is_err());
        assert!(
            mappings(
                &i,
                &v,
                Path::new("/new/state"),
                Path::new("/backup/a.vsbk"),
                &[]
            )
            .is_err()
        );
        assert!(!canonical_path(Path::new("/new/../old")));
    }
    #[test]
    fn compatibility_and_capacity_are_independent_explicit_gates() {
        let i = index();
        let v = serde_json::json!({"protocol_contract_version":"1","protocol_contract_sha256":"ab".repeat(32),"product_version":"0.2.3"});
        assert!(
            compatible(
                &v,
                &i,
                Compatibility {
                    minimum_schema: 12,
                    maximum_schema: 13,
                    protocol: "1"
                }
            )
            .is_ok()
        );
        assert_eq!(
            compatible(
                &v,
                &i,
                Compatibility {
                    minimum_schema: 12,
                    maximum_schema: 12,
                    protocol: "1"
                }
            ),
            Err(Error::Incompatible)
        );
        let n = required(&i, &namespace(&i, vec![root(), file()]).unwrap()).unwrap();
        assert_eq!(space(n, n - 1), Err(Error::Space));
        assert_eq!(space(n, n), Ok(()));
    }
}
