//! 来自同一数据库快照的引用事实。时钟由调用方注入，不读取对象 mtime。
use std::collections::{BTreeMap, BTreeSet};
use vaultsyncd_domain::{ids::VaultId, version::UtcTimestamp};

/// C07 的最低保留期，不能随磁盘压力缩短。
pub const RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;

/// 内容地址按 Vault 隔离。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectKey {
    /// 所属 Vault。
    pub vault: VaultId,
    /// 内容 SHA-256。
    pub hash: [u8; 32],
}

/// 同一对象可有多种保护原因；未出现的原因不能抵消已有保护。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// 活跃条目的当前版本。
    Current,
    /// 保留期内历史。
    History,
    /// 可恢复回收站内容。
    Trash,
    /// 未完成操作或文件意图。
    UnfinishedOperation,
    /// 上传会话。
    Upload,
    /// 固定版本下载租约。
    Download,
    /// 对象提交租约。
    Commit,
    /// 备份租约。
    Backup,
}

/// 事实中的截止点使用数据库业务时间；缺失截止点表示持续保护。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reference {
    /// 引用的内容地址。
    pub object: ObjectKey,
    /// 保护来源。
    pub reason: Reason,
    /// 排他截止时间；None 表示持续保护。
    pub until: Option<UtcTimestamp>,
}

/// 从取代或删除的时刻起算，溢出时永不提前到期。
pub fn retained_until(at: UtcTimestamp) -> UtcTimestamp {
    UtcTimestamp::from_unix_seconds(at.as_unix_seconds().saturating_add(RETENTION_SECONDS))
        .expect("非负时间加保留期")
}

/// 标记仅代表此快照此时刻的保护集合，不是可删除清单。
pub fn mark(
    now: UtcTimestamp,
    references: impl IntoIterator<Item = Reference>,
) -> BTreeMap<ObjectKey, BTreeSet<Reason>> {
    let mut marked: BTreeMap<ObjectKey, BTreeSet<Reason>> = BTreeMap::new();
    for reference in references {
        if reference.until.is_none_or(|until| until > now) {
            marked
                .entry(reference.object)
                .or_default()
                .insert(reference.reason);
        }
    }
    marked
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(t: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_seconds(t).unwrap()
    }
    fn object() -> ObjectKey {
        ObjectKey {
            vault: VaultId::from_bytes([1; 16]),
            hash: [2; 32],
        }
    }
    #[test]
    fn current_is_ageless_and_expired_references_cannot_cancel_it() {
        let refs = [
            Reference {
                object: object(),
                reason: Reason::Current,
                until: None,
            },
            Reference {
                object: object(),
                reason: Reason::History,
                until: Some(at(1)),
            },
        ];
        assert_eq!(
            mark(at(i64::MAX), refs)[&object()],
            BTreeSet::from([Reason::Current])
        );
    }
    #[test]
    fn retention_starts_at_supersede_or_delete_and_expiry_is_exclusive() {
        let expiry = retained_until(at(100));
        let r = Reference {
            object: object(),
            reason: Reason::History,
            until: Some(expiry),
        };
        assert!(mark(at(expiry.as_unix_seconds() - 1), [r]).contains_key(&object()));
        assert!(mark(expiry, [r]).is_empty());
        assert_eq!(retained_until(at(i64::MAX)), at(i64::MAX));
    }
    #[test]
    fn vault_scope_and_all_lease_reasons_are_preserved() {
        let reasons = [
            Reason::Trash,
            Reason::UnfinishedOperation,
            Reason::Upload,
            Reason::Download,
            Reason::Commit,
            Reason::Backup,
        ];
        let refs = reasons.map(|reason| Reference {
            object: object(),
            reason,
            until: Some(at(20)),
        });
        assert_eq!(mark(at(19), refs)[&object()].len(), 6);
        assert!(mark(at(20), refs).is_empty());
        let mut other = object();
        other.vault = VaultId::from_bytes([3; 16]);
        assert!(!mark(at(1), refs).contains_key(&other));
    }
}
