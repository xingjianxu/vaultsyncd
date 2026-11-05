//! Root 与内部目录的父子/别名/bind 重叠判定（design §16.1、§11.5）。
//!
//! design §16.1 要求注册时检查“规范路径、父子关系、卷身份、目录身份，以及符号链接、junction、
//! bind mount 等别名关系”，§11.5 补一句“别名/重叠检测不能只比路径或 inode”。因此本模块同时
//! 看三样东西：
//!
//! - **规范路径**：按路径分量判断父子（`/data/a` 不是 `/data/abc` 的父目录，复用
//!   [`crate::mounts::paths`]）；符号链接别名在 `canonicalize` 之后与目标同路径，因此落到
//!   [`OverlapKind::SamePath`]。
//! - **目录身份**：`(设备主号, 设备次号, inode, btime)` 相同而路径不同 ⇒
//!   [`OverlapKind::Alias`]，这正是 bind mount 把同一目录挂到另一个路径的样子（两条路径的
//!   `STATX_MNT_ID` 不同，但目录对象是同一个）。只看路径会漏掉它，只看 inode 又会把不同子卷的
//!   inode 256 当成同一个目录。
//! - **角色**：内部数据库/对象/历史/暂存目录必须位于所有受管 Root 之外（design §16.1），所以
//!   内部目录与 Root 有任何父子关系都要报出来，方向不同原因码也不同。
//!
//! 本模块只判定并报告，不做 I/O（[`DirectoryRecord`] 由调用方用
//! [`crate::mounts::probe::probe_path`] 填好）；也不决定“是否允许”，由注册流程决定。

use crate::mounts::paths::{self, PathRelation};
use crate::mounts::probe::StorageFacts;
use std::fmt;

/// 目录在部署里的角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryRole {
    /// 受管 Root（用户可见的同步目录）。
    Root,
    /// 内部状态目录（SQLite、对象、历史、暂存、备份暂存）。
    Internal,
}

impl DirectoryRole {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DirectoryRole::Root => "root",
            DirectoryRole::Internal => "internal",
        }
    }
}

impl fmt::Display for DirectoryRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一个待检查的目录：角色 + 打开句柄时读到的内核事实。
///
/// 直接持有 [`StorageFacts`]（而不是自己另建一套身份），保证“注册时看到的”与准入、复核用的是
/// 同一份事实，不会出现两套口径。
#[derive(Clone, Debug)]
pub struct DirectoryRecord {
    /// 角色。
    pub role: DirectoryRole,
    /// 探测事实（含规范路径、挂载 ID、设备、inode、btime、btrfs 身份）。
    pub facts: StorageFacts,
}

impl DirectoryRecord {
    /// 用探测事实建一条记录。
    #[must_use]
    pub fn new(role: DirectoryRole, facts: StorageFacts) -> Self {
        Self { role, facts }
    }
}

/// 重叠的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlapKind {
    /// 两个记录指向同一个规范路径。
    SamePath,
    /// 同一目录对象经不同路径出现（bind mount、别名），路径上互不包含。
    Alias,
    /// 内部目录位于 Root 之内（design §16.1 禁止）。
    InternalInsideRoot,
    /// Root 位于内部目录之内（内部状态目录被当成 Root 的祖先，同样禁止）。
    RootInsideInternal,
    /// 一方是另一方的祖先目录（同角色或其它组合）。
    ParentChild,
}

impl OverlapKind {
    /// 稳定原因码。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            OverlapKind::SamePath => "same_path",
            OverlapKind::Alias => "alias",
            OverlapKind::InternalInsideRoot => "internal_inside_root",
            OverlapKind::RootInsideInternal => "root_inside_internal",
            OverlapKind::ParentChild => "parent_child",
        }
    }
}

impl fmt::Display for OverlapKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一处重叠：记录下标（左 < 右）+ 种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overlap {
    /// 左记录下标。
    pub left: usize,
    /// 右记录下标。
    pub right: usize,
    /// 种类。
    pub kind: OverlapKind,
}

impl Overlap {
    /// 渲染成稳定的 `key=value` 行（验收脚本按它断言）。
    #[must_use]
    pub fn render(&self, records: &[DirectoryRecord]) -> String {
        let left = &records[self.left];
        let right = &records[self.right];
        format!(
            "pair={},{} kind={} left={}:{} right={}:{} mount_ids={}:{}",
            self.left,
            self.right,
            self.kind.as_str(),
            left.role.as_str(),
            left.facts.resolved_path.display(),
            right.role.as_str(),
            right.facts.resolved_path.display(),
            left.facts.mount.mount_id,
            right.facts.mount.mount_id,
        )
    }
}

/// 两个目录是否是同一个目录对象（设备、inode、创建时间都相同）。
///
/// 不看路径也不看挂载 ID：bind mount 的两条路径挂载 ID 不同，但目录对象是同一个；反过来同
/// inode 号在不同 btrfs 子卷上完全无关，所以必须带设备号与 btime。
#[must_use]
pub fn same_object(left: &StorageFacts, right: &StorageFacts) -> bool {
    left.device == right.device && left.inode == right.inode && left.btime == right.btime
}

/// 找出所有重叠，按 `(left, right)` 升序返回；每对记录最多报一个原因（按固定优先级）。
#[must_use]
pub fn find_overlaps(records: &[DirectoryRecord]) -> Vec<Overlap> {
    let mut overlaps = Vec::new();
    for left in 0..records.len() {
        for right in left + 1..records.len() {
            if let Some(kind) = classify_pair(&records[left], &records[right]) {
                overlaps.push(Overlap { left, right, kind });
            }
        }
    }
    overlaps
}

fn classify_pair(left: &DirectoryRecord, right: &DirectoryRecord) -> Option<OverlapKind> {
    let relation = paths::relation(&left.facts.resolved_path, &right.facts.resolved_path);
    if relation == PathRelation::Same && left.facts.resolved_path == right.facts.resolved_path {
        return Some(OverlapKind::SamePath);
    }
    if same_object(&left.facts, &right.facts) {
        return Some(OverlapKind::Alias);
    }
    match relation {
        PathRelation::Unrelated => None,
        PathRelation::Same => Some(OverlapKind::SamePath),
        PathRelation::Ancestor | PathRelation::Descendant => {
            // 左是祖先（Ancestor）时右在左内；Descendant 时左在右内。
            let ancestor = if relation == PathRelation::Ancestor {
                left
            } else {
                right
            };
            let descendant = if relation == PathRelation::Ancestor {
                right
            } else {
                left
            };
            match (ancestor.role, descendant.role) {
                (DirectoryRole::Root, DirectoryRole::Internal) => {
                    Some(OverlapKind::InternalInsideRoot)
                }
                (DirectoryRole::Internal, DirectoryRole::Root) => {
                    Some(OverlapKind::RootInsideInternal)
                }
                _ => Some(OverlapKind::ParentChild),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::admission::BTRFS_SUPER_MAGIC;
    use crate::mounts::mountinfo::{DeviceId, MountEntry};
    use crate::mounts::probe::{BtrfsFacts, MountIdSource, SubvolumeIdSource};
    use std::path::PathBuf;

    /// 造一条事实：路径、挂载 ID、设备、inode、btime 都是显式给的，避免测试依赖主机环境。
    fn facts(
        path: &str,
        mount_id: u64,
        device: (u32, u32),
        inode: u64,
        btime: Option<(i64, u32)>,
    ) -> StorageFacts {
        StorageFacts {
            given_path: PathBuf::from(path),
            resolved_path: PathBuf::from(path),
            fs_magic: BTRFS_SUPER_MAGIC,
            fs_type: "btrfs".to_owned(),
            statfs_fsid: [0, 0],
            mount: MountEntry {
                mount_id,
                parent_id: 1,
                device: DeviceId {
                    major: device.0,
                    minor: device.1,
                },
                root: PathBuf::from("/"),
                mount_point: PathBuf::from("/data"),
                mount_options: vec!["rw".to_owned()],
                optional_fields: Vec::new(),
                fs_type: "btrfs".to_owned(),
                source: "/dev/sdc".to_owned(),
                super_options: vec!["rw".to_owned()],
            },
            mount_id_source: MountIdSource::StatxMountId,
            inode,
            device: DeviceId {
                major: device.0,
                minor: device.1,
            },
            btime,
            btrfs: Some(BtrfsFacts {
                fsid: [0x11; 16],
                subvolume_id: Some(256),
                subvolume_id_source: Some(SubvolumeIdSource::StatxSubvol),
                subvolume_id_error: None,
            }),
            inode_flags: Some(0),
            inode_flags_error: None,
        }
    }

    fn record(role: DirectoryRole, facts: StorageFacts) -> DirectoryRecord {
        DirectoryRecord::new(role, facts)
    }

    #[test]
    fn unrelated_siblings_do_not_overlap() {
        let records = [
            record(
                DirectoryRole::Root,
                facts("/data/runs/s032/root", 45, (0, 41), 257, Some((1, 0))),
            ),
            record(
                DirectoryRole::Internal,
                facts("/data/runs/s032/state", 45, (0, 41), 258, Some((1, 0))),
            ),
        ];
        assert!(find_overlaps(&records).is_empty());
    }

    #[test]
    fn path_prefix_is_not_parent_child() {
        // `/data/root` 与 `/data/root-old` 是分量上互不包含的兄弟，不是父子。
        let records = [
            record(
                DirectoryRole::Root,
                facts("/data/root", 45, (0, 41), 257, Some((1, 0))),
            ),
            record(
                DirectoryRole::Root,
                facts("/data/root-old", 45, (0, 41), 258, Some((1, 0))),
            ),
        ];
        assert!(find_overlaps(&records).is_empty());
    }

    #[test]
    fn internal_directory_inside_root_is_reported() {
        let records = [
            record(
                DirectoryRole::Root,
                facts("/data/root", 45, (0, 41), 257, Some((1, 0))),
            ),
            record(
                DirectoryRole::Internal,
                facts("/data/root/state", 45, (0, 41), 258, Some((1, 0))),
            ),
        ];
        let overlaps = find_overlaps(&records);
        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].kind, OverlapKind::InternalInsideRoot);
        assert!(
            overlaps[0]
                .render(&records)
                .contains("kind=internal_inside_root")
        );
        assert!(
            overlaps[0]
                .render(&records)
                .contains("left=root:/data/root right=internal:/data/root/state")
        );
    }

    #[test]
    fn root_inside_internal_directory_is_reported() {
        let records = [
            record(
                DirectoryRole::Internal,
                facts("/data/state", 45, (0, 41), 257, Some((1, 0))),
            ),
            record(
                DirectoryRole::Root,
                facts("/data/state/root", 45, (0, 41), 258, Some((1, 0))),
            ),
        ];
        let overlaps = find_overlaps(&records);
        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].kind, OverlapKind::RootInsideInternal);
    }

    #[test]
    fn two_roots_with_parent_child_are_reported() {
        let records = [
            record(
                DirectoryRole::Root,
                facts("/data/root", 45, (0, 41), 257, Some((1, 0))),
            ),
            record(
                DirectoryRole::Root,
                facts("/data/root/sub", 45, (0, 41), 258, Some((1, 0))),
            ),
        ];
        let overlaps = find_overlaps(&records);
        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].kind, OverlapKind::ParentChild);
    }

    #[test]
    fn same_path_is_reported_once_per_pair() {
        let records = [
            record(
                DirectoryRole::Root,
                facts("/data/root", 45, (0, 41), 257, Some((1, 0))),
            ),
            record(
                DirectoryRole::Root,
                facts("/data/root", 45, (0, 41), 257, Some((1, 0))),
            ),
        ];
        let overlaps = find_overlaps(&records);
        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].kind, OverlapKind::SamePath);
    }

    #[test]
    fn bind_mount_alias_is_detected_by_identity_not_path() {
        // 同一目录 bind mount 到无关路径：路径无关、挂载 ID 不同，但设备/inode/btime 相同。
        let records = [
            record(
                DirectoryRole::Root,
                facts("/data/runs/s032/root", 45, (0, 41), 257, Some((1, 0))),
            ),
            record(
                DirectoryRole::Internal,
                facts("/data/runs/s032/alias", 46, (0, 41), 257, Some((1, 0))),
            ),
        ];
        let overlaps = find_overlaps(&records);
        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].kind, OverlapKind::Alias);
        assert!(overlaps[0].render(&records).contains("mount_ids=45:46"));
        assert!(
            !overlaps[0].render(&records).contains("mount_ids=45:45"),
            "bind mount 的两条路径挂载 ID 必须不同"
        );
    }

    #[test]
    fn same_inode_in_different_subvolumes_is_not_an_alias() {
        // btrfs 每个子卷的根目录 inode 都是 256：只比 inode 会把它们当成同一个目录。
        let records = [
            record(
                DirectoryRole::Root,
                facts("/data/root", 45, (0, 41), 256, Some((1, 0))),
            ),
            record(
                DirectoryRole::Internal,
                facts("/data/state", 45, (0, 42), 256, Some((1, 0))),
            ),
        ];
        assert!(find_overlaps(&records).is_empty());
    }

    #[test]
    fn same_object_requires_device_inode_and_btime() {
        let left = facts("/a", 45, (0, 41), 257, Some((1, 0)));
        let mut right = facts("/b", 46, (0, 41), 257, Some((1, 0)));
        assert!(same_object(&left, &right));
        right.btime = Some((2, 0));
        assert!(!same_object(&left, &right));
        right.btime = Some((1, 0));
        right.device = DeviceId {
            major: 0,
            minor: 42,
        };
        assert!(!same_object(&left, &right));
    }

    #[test]
    fn ordering_of_reported_pairs_is_deterministic() {
        let records = [
            record(
                DirectoryRole::Root,
                facts("/data/root", 45, (0, 41), 257, Some((1, 0))),
            ),
            record(
                DirectoryRole::Internal,
                facts("/data/root/state", 45, (0, 41), 258, Some((1, 0))),
            ),
            record(
                DirectoryRole::Internal,
                facts("/data/root/state/objects", 45, (0, 41), 259, Some((1, 0))),
            ),
        ];
        let overlaps = find_overlaps(&records);
        let pairs: Vec<(usize, usize)> = overlaps
            .iter()
            .map(|overlap| (overlap.left, overlap.right))
            .collect();
        assert_eq!(pairs, vec![(0, 1), (0, 2), (1, 2)]);
        let kinds: Vec<OverlapKind> = overlaps.iter().map(|overlap| overlap.kind).collect();
        assert_eq!(
            kinds,
            vec![
                OverlapKind::InternalInsideRoot,
                OverlapKind::InternalInsideRoot,
                OverlapKind::ParentChild,
            ]
        );
    }
}
