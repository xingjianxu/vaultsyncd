//! 备份清单与发布前提（design §19.1）。
use crate::objects::{ContentDigest, ObjectLayout};
use std::collections::BTreeSet;
use vaultsyncd_domain::{ids::VaultId, version::VaultSeq};

/// 一份被冻结的Vault序号。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenVault {
    /// Vault身份。
    pub vault: VaultId,
    /// 提交屏障内选定的已确认序号。
    pub seq: VaultSeq,
}
/// 导出数据库引用的不可变对象。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenObject {
    /// 所属Vault。
    pub vault: VaultId,
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
    /// 各Vault的序号。
    pub vaults: Vec<FrozenVault>,
    /// 完整对象清单（也包含仍在DB中的历史/回收站对象）。
    pub objects: Vec<FrozenObject>,
}
/// 任何一个证据不成立都不能发布ready。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// 身份重复或对象不属于清单Vault。
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
    /// 检查稳定身份/Vault隔离/规范对象路径；空服务器可有空清单。
    pub fn validate(&self) -> Result<(), Error> {
        let vaults: BTreeSet<_> = self.vaults.iter().map(|r| r.vault).collect();
        if vaults.len() != self.vaults.len() {
            return Err(Error::InvalidIdentity);
        }
        let mut keys = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for o in &self.objects {
            if !vaults.contains(&o.vault) || !keys.insert((o.vault, o.hash)) {
                return Err(Error::InvalidIdentity);
            }
            if !ObjectLayout::valid_storage_path(
                o.vault,
                &ContentDigest::from_bytes(o.hash),
                &o.storage_path,
            ) || !paths.insert(&o.storage_path)
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
            vaults: vec![],
            objects: vec![],
        };
        assert_eq!(ready(&p, 1, true, true, true), Err(Error::UnfinishedIntent));
        assert_eq!(ready(&p, 0, false, true, true), Err(Error::Incomplete));
        assert_eq!(ready(&p, 0, true, false, true), Err(Error::NotReadOnly));
        assert_eq!(ready(&p, 0, true, true, false), Err(Error::Unprotected));
        assert_eq!(ready(&p, 0, true, true, true), Ok(()));
    }

    #[test]
    fn same_digest_is_vault_isolated_and_only_canonical_paths_validate() {
        let vaults: Vec<_> = [1, 2]
            .into_iter()
            .map(|n| FrozenVault {
                vault: VaultId::from_bytes([n; 16]),
                seq: VaultSeq::ZERO,
            })
            .collect();
        let digest = ContentDigest::from_bytes([0xab; 32]);
        let mut plan = Plan {
            objects: vaults
                .iter()
                .map(|r| FrozenObject {
                    vault: r.vault,
                    hash: *digest.as_bytes(),
                    size: 1,
                    storage_path: ObjectLayout::relative_object_path(r.vault, &digest)
                        .to_str()
                        .unwrap()
                        .to_owned(),
                })
                .collect(),
            vaults,
        };
        assert_eq!(plan.validate(), Ok(()));
        let scoped = plan.objects[1].storage_path.clone();
        plan.objects[1].storage_path = plan.objects[0].storage_path.clone();
        assert_eq!(plan.validate(), Err(Error::InvalidPath));
        plan.objects[1].storage_path = scoped.clone();
        // 旧全局布局与旧的 roots/ 前缀都不再被接受。
        let hash_text = digest.to_hex();
        plan.objects[0].storage_path = format!("objects/{}/{}", &hash_text[..2], hash_text);
        assert_eq!(plan.validate(), Err(Error::InvalidPath));
        plan.objects[0].storage_path = format!(
            "roots/{}/objects/{}/{}",
            plan.objects[0].vault.to_hex(),
            &hash_text[..2],
            hash_text
        );
        assert_eq!(plan.validate(), Err(Error::InvalidPath));
        plan.objects[0].storage_path =
            ObjectLayout::relative_object_path(plan.objects[0].vault, &digest)
                .to_str()
                .unwrap()
                .to_owned();
        plan.objects.pop();
        assert_eq!(plan.validate(), Ok(()));
        let canonical = plan.objects[0].storage_path.clone();
        for invalid in [
            format!("./{canonical}"),
            format!("{canonical}/"),
            canonical.replacen('/', "//", 1),
            canonical.to_uppercase(),
        ] {
            plan.objects[0].storage_path = invalid;
            assert_eq!(plan.validate(), Err(Error::InvalidPath));
        }
    }

    #[test]
    fn manifest_rejects_foreign_vaults_and_path_escape() {
        let vault = VaultId::from_bytes([1; 16]);
        let hash = [2; 32];
        let object = FrozenObject {
            vault,
            hash,
            size: 1,
            storage_path: ObjectLayout::relative_object_path(
                vault,
                &ContentDigest::from_bytes(hash),
            )
            .display()
            .to_string(),
        };
        let mut plan = Plan {
            vaults: vec![],
            objects: vec![object],
        };
        assert_eq!(plan.validate(), Err(Error::InvalidIdentity));
        plan.vaults.push(FrozenVault {
            vault,
            seq: VaultSeq::ZERO,
        });
        assert_eq!(plan.validate(), Ok(()));
        plan.objects[0].storage_path = "../../user-file".into();
        assert_eq!(plan.validate(), Err(Error::InvalidPath));
    }
}
