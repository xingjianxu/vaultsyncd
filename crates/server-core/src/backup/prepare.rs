//! 备份清单与发布前提（design §19.1）。
use crate::objects::{ContentDigest, ObjectLayout};
use std::collections::BTreeSet;
use vaultsyncd_domain::{ids::RootId, version::RootSeq};

/// 一份被冻结的Root序号。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenRoot {
    /// Root身份。
    pub root: RootId,
    /// 提交屏障内选定的已确认序号。
    pub seq: RootSeq,
}
/// 导出数据库引用的不可变对象。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenObject {
    /// 所属Root。
    pub root: RootId,
    /// SHA-256。
    pub hash: [u8; 32],
    /// 字节数。
    pub size: u64,
    /// 内部规范相对路径。
    pub storage_path: String,
}
/// 从同一SQLite读取快照得到的准备计划。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// 各Root的序号。
    pub roots: Vec<FrozenRoot>,
    /// 完整对象清单（也包含仍在DB中的历史/回收站对象）。
    pub objects: Vec<FrozenObject>,
}
/// 任何一个证据不成立都不能发布ready。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// 身份重复或对象不属于清单Root。
    InvalidIdentity,
    /// 内容路径不规范或重用同一个物理位置。
    InvalidPath,
    /// 意图尚未收敛，不能声称一致性快照。
    UnfinishedIntent,
    /// 缺对象、摘要不符或数据库自检失败。
    Incomplete,
    /// 没有固定的只读快照。
    NotReadOnly,
    /// 清单对象未获有效租约保护。
    Unprotected,
}
impl Plan {
    /// 检查稳定身份/Root隔离/规范对象路径；空服务器可有空清单。
    pub fn validate(&self) -> Result<(), Error> {
        let roots: BTreeSet<_> = self.roots.iter().map(|r| r.root).collect();
        if roots.len() != self.roots.len() {
            return Err(Error::InvalidIdentity);
        }
        let mut keys = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for o in &self.objects {
            if !roots.contains(&o.root) || !keys.insert((o.root, o.hash)) {
                return Err(Error::InvalidIdentity);
            }
            if ObjectLayout::relative_object_path(&ContentDigest::from_bytes(o.hash)).to_str()
                != Some(o.storage_path.as_str())
                || !paths.insert(&o.storage_path)
            {
                return Err(Error::InvalidPath);
            }
        }
        Ok(())
    }
}
/// 发布ready必须有全部独立证据；ready只代表prepare完成，导出verify另行执行。
pub fn ready(
    plan: &Plan,
    unfinished: usize,
    contents_verified: bool,
    read_only: bool,
    lease_active: bool,
) -> Result<(), Error> {
    plan.validate()?;
    if unfinished != 0 {
        return Err(Error::UnfinishedIntent);
    }
    if !contents_verified {
        return Err(Error::Incomplete);
    }
    if !read_only {
        return Err(Error::NotReadOnly);
    }
    if !lease_active {
        return Err(Error::Unprotected);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_publication_evidence_is_required() {
        let p = Plan {
            roots: vec![],
            objects: vec![],
        };
        assert_eq!(ready(&p, 1, true, true, true), Err(Error::UnfinishedIntent));
        assert_eq!(ready(&p, 0, false, true, true), Err(Error::Incomplete));
        assert_eq!(ready(&p, 0, true, false, true), Err(Error::NotReadOnly));
        assert_eq!(ready(&p, 0, true, true, false), Err(Error::Unprotected));
        assert_eq!(ready(&p, 0, true, true, true), Ok(()));
    }
}
