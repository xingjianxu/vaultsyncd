//! 运行期挂载健康复核（design §12.3、T19/T35）：每次挂载变化后核对卷身份、根目录身份、
//! 访问权限与子挂载。
//!
//! 与准入（[`crate::mounts::admission`]）的分工：准入只在注册时判定一次；本模块在运行期回答
//! “Root 还是启动时那个存储单元吗”。两者都用内核事实（打开句柄的 `fstatfs`/`statx` 与本进程
//! `/proc/self/mountinfo`），不按路径字符串推断。
//!
//! 四条检查，任一不满足都让 Root 进入 `ROOT_UNAVAILABLE`，**不推导任何删除**：
//!
//! 1. 卷身份：后端类型、btrfs FSID 与子卷 ID（[`RootSnapshot::compare`]）；
//! 2. 根目录身份与挂载 ID：目录被替换、路径被 bind mount 覆盖（`DirectoryChanged`/`MountChanged`）；
//! 3. 子挂载：Root 路径下出现其他挂载点（嵌套挂载绕过了根句柄的边界假设）；
//! 4. 权限：根目录仍可打开并列出。
//!
//! “卸载后路径下露出空目录”正好命中 2（底层目录的 inode/设备/挂载 ID 都变了）或 1：这是 Root
//! 不可用，不是“用户删除了所有文件”（T19）。

use std::fmt;
use std::fs;
use std::io;
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::files::root::{RootDrift, RootSnapshot};
use crate::mounts::mountinfo::{self, MountEntry};
use crate::mounts::paths::{self, PathRelation};
use crate::mounts::probe::{self, ProbeError, StorageFacts};

/// 运行期挂载复核的结论。
#[derive(Clone, Debug)]
pub struct MountHealth {
    /// 本次实测的存储事实。
    pub facts: StorageFacts,
    /// Root 路径下的其他挂载点（正常为空）。
    pub submounts: Vec<PathBuf>,
    /// 根目录第一层读到的目录项数（最多读一个，用于确认可列目录）。
    pub entries_readable: usize,
}

/// 挂载健康复核失败的原因。
#[derive(Debug)]
pub enum MountChange {
    /// 重新探测失败（“读不到”不是“没变化”）。
    Probe(ProbeError),
    /// 身份漂移（后端、FSID、子卷、挂载 ID 或目录身份）。
    Drift(RootDrift),
    /// Root 路径下出现了子挂载。
    Submount {
        /// 第一个发现的子挂载点。
        path: PathBuf,
        /// 子挂载的 fstype。
        fs_type: String,
    },
    /// 根目录没有读取权限。
    PermissionDenied {
        /// 根目录路径。
        path: PathBuf,
        /// 原始错误文本。
        detail: String,
    },
    /// 根目录读不了（IO 等，不含权限）。
    Unreadable {
        /// 根目录路径。
        path: PathBuf,
        /// 原始错误文本。
        detail: String,
    },
    /// 读 `/proc/self/mountinfo` 失败。
    MountInfo(io::Error),
}

impl MountChange {
    /// 稳定原因码。运行期一律 `ROOT_UNAVAILABLE`（design §11.4—11.5），细分原因见 [`Self::detail_code`]。
    #[must_use]
    pub fn design_error_code(&self) -> &'static str {
        "ROOT_UNAVAILABLE"
    }

    /// 细分原因码（诊断与验收脚本用）。
    #[must_use]
    pub fn detail_code(&self) -> &'static str {
        match self {
            MountChange::Probe(_) => "probe_failed",
            MountChange::Drift(drift) => drift.code(),
            MountChange::Submount { .. } => "submount_detected",
            MountChange::PermissionDenied { .. } => "permission_denied",
            MountChange::Unreadable { .. } => "root_unreadable",
            MountChange::MountInfo(_) => "mountinfo_unreadable",
        }
    }
}

impl fmt::Display for MountChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MountChange::Probe(error) => write!(f, "重新探测 Root 失败：{error}"),
            MountChange::Drift(drift) => write!(f, "Root 身份已漂移：{}", drift.code()),
            MountChange::Submount { path, fs_type } => write!(
                f,
                "Root 内出现子挂载 {}（{fs_type}）：挂载边界不再可信",
                path.display()
            ),
            MountChange::PermissionDenied { path, detail } => {
                write!(f, "根目录 {} 没有读取权限：{detail}", path.display())
            }
            MountChange::Unreadable { path, detail } => {
                write!(f, "根目录 {} 不可读：{detail}", path.display())
            }
            MountChange::MountInfo(error) => write!(f, "读取 mountinfo 失败：{error}"),
        }
    }
}

impl std::error::Error for MountChange {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MountChange::Probe(error) => Some(error),
            MountChange::MountInfo(error) => Some(error),
            MountChange::Drift(_)
            | MountChange::Submount { .. }
            | MountChange::PermissionDenied { .. }
            | MountChange::Unreadable { .. } => None,
        }
    }
}

/// 找出 Root 路径**之下**的挂载点（不含 Root 自身）。
///
/// 只看路径分量，不做字符串前缀比较（`/data/root` 与 `/data/root2` 不是父子）；排序保证结果稳定。
#[must_use]
pub fn submounts_under<'a>(entries: &'a [MountEntry], root: &Path) -> Vec<&'a MountEntry> {
    let mut found: Vec<&MountEntry> = entries
        .iter()
        .filter(|entry| paths::relation(&entry.mount_point, root) == PathRelation::Descendant)
        .collect();
    found.sort_by(|left, right| left.mount_point.cmp(&right.mount_point));
    found
}

/// 复核 Root 的挂载健康：卷身份、根目录身份、子挂载与读取权限。
///
/// # Errors
///
/// 见 [`MountChange`]。任何失败都意味着 Root 不可用（`ROOT_UNAVAILABLE`），调用方必须暂停该 Root，
/// 并且**不得**把此间的扫描结果当成删除证据。
pub fn verify_root_mount(path: &Path, expected: &RootSnapshot) -> Result<MountHealth, MountChange> {
    let facts = probe::probe_path(path).map_err(|error| probe_change(path, error))?;
    expected.compare(&facts).map_err(MountChange::Drift)?;

    let entries = mountinfo::read_self().map_err(MountChange::MountInfo)?;
    let submounts = submounts_under(&entries, &facts.resolved_path);
    if let Some(first) = submounts.first() {
        return Err(MountChange::Submount {
            path: first.mount_point.clone(),
            fs_type: first.fs_type.clone(),
        });
    }

    let entries_readable = check_readable_dir(&facts.resolved_path)?;
    Ok(MountHealth {
        facts,
        submounts: Vec::new(),
        entries_readable,
    })
}

/// 探测失败时把权限不足单独分出来：扫描健康结论要能区分 `permission_denied` 与其它读失败。
fn probe_change(path: &Path, error: ProbeError) -> MountChange {
    const EACCES: i32 = 13;
    const EPERM: i32 = 1;
    let denied = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<io::Error>())
        .and_then(io::Error::raw_os_error)
        .is_some_and(|code| code == EACCES || code == EPERM);
    if denied {
        MountChange::PermissionDenied {
            path: path.to_path_buf(),
            detail: error.to_string(),
        }
    } else {
        MountChange::Probe(error)
    }
}

/// 确认目录可以打开并列出第一项：权限不足与 IO 失败分开上报。
///
/// # Errors
///
/// 权限不足为 [`MountChange::PermissionDenied`]，其他失败为 [`MountChange::Unreadable`]。
pub fn check_readable_dir(path: &Path) -> Result<usize, MountChange> {
    let mut read = fs::read_dir(path).map_err(|error| classify(path, &error))?;
    let mut count = 0usize;
    if let Some(entry) = read.next() {
        // 只读一项：足够触发一次 `getdents`，又不因大目录付出代价。
        entry.map_err(|error| classify(path, &error))?;
        count = 1;
    }
    Ok(count)
}

fn classify(path: &Path, error: &io::Error) -> MountChange {
    const EACCES: i32 = 13;
    const EPERM: i32 = 1;
    match error.raw_os_error() {
        Some(EACCES | EPERM) => MountChange::PermissionDenied {
            path: path.to_path_buf(),
            detail: error.to_string(),
        },
        _ => MountChange::Unreadable {
            path: path.to_path_buf(),
            detail: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::mountinfo::DeviceId;

    fn entry(mount_point: &str, fs_type: &str) -> MountEntry {
        MountEntry {
            mount_id: 40,
            parent_id: 22,
            device: DeviceId {
                major: 0,
                minor: 40,
            },
            root: PathBuf::from("/"),
            mount_point: PathBuf::from(mount_point),
            mount_options: vec!["rw".to_owned()],
            optional_fields: Vec::new(),
            fs_type: fs_type.to_owned(),
            source: "/dev/sdc".to_owned(),
            super_options: vec!["rw".to_owned()],
        }
    }

    #[test]
    fn submounts_are_found_by_path_components_not_by_string_prefix() {
        let entries = vec![
            entry("/data", "btrfs"),
            entry("/data/root", "btrfs"),
            entry("/data/root2", "btrfs"),
            entry("/data/root/nested", "tmpfs"),
            entry("/data/root/deep/bind", "btrfs"),
        ];
        let found = submounts_under(&entries, Path::new("/data/root"));
        let paths: Vec<&Path> = found
            .iter()
            .map(|entry| entry.mount_point.as_path())
            .collect();
        assert_eq!(
            paths,
            vec![
                Path::new("/data/root/deep/bind"),
                Path::new("/data/root/nested")
            ],
            "兄弟目录 /data/root2 不算子挂载"
        );
        assert_eq!(found[1].fs_type, "tmpfs");
    }

    #[test]
    fn a_clean_root_has_no_submounts() {
        let entries = vec![entry("/data", "btrfs"), entry("/data/root", "btrfs")];
        assert!(submounts_under(&entries, Path::new("/data/root")).is_empty());
    }

    #[test]
    fn mount_change_reports_root_unavailable_and_a_detail_code() {
        let change = MountChange::Drift(RootDrift::MountChanged);
        assert_eq!(change.design_error_code(), "ROOT_UNAVAILABLE");
        assert_eq!(change.detail_code(), "mount_changed");
        assert_eq!(
            MountChange::Submount {
                path: PathBuf::from("/data/root/nested"),
                fs_type: "tmpfs".to_owned()
            }
            .detail_code(),
            "submount_detected"
        );
        assert_eq!(
            MountChange::PermissionDenied {
                path: PathBuf::from("/data/root"),
                detail: "EACCES".to_owned()
            }
            .detail_code(),
            "permission_denied"
        );
        assert!(
            MountChange::Drift(RootDrift::DirectoryChanged)
                .to_string()
                .contains("directory_changed")
        );
        assert_eq!(
            MountChange::Drift(RootDrift::MountChanged).design_error_code(),
            "ROOT_UNAVAILABLE"
        );
    }

    /// 真实目录：健康 → 目录被换掉（卸载后露出另一个目录）→ 权限被收回。
    #[test]
    fn verify_root_mount_notices_a_swapped_directory_and_lost_permissions() {
        use crate::files::root::RootSnapshot;

        let token = format!("{}-{}", std::process::id(), line!());
        let base = std::env::temp_dir().join(format!("vaultsyncd-health-{token}"));
        let moved = std::env::temp_dir().join(format!("vaultsyncd-health-{token}-moved"));
        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&moved);
        fs::create_dir_all(base.join("inner")).expect("建目录");
        fs::write(base.join("inner/file.txt"), b"x").expect("写文件");

        let facts = probe::probe_path(&base).expect("探测根目录");
        let snapshot = RootSnapshot::of_facts(&facts);
        let healthy = verify_root_mount(&base, &snapshot).expect("刚建立的目录必须健康");
        assert!(healthy.submounts.is_empty());
        assert_eq!(healthy.entries_readable, 1);

        // 卸载后路径下露出另一个（空）目录：身份与挂载 ID 都不再是注册时那个。
        fs::rename(&base, &moved).expect("移走原目录");
        fs::create_dir(&base).expect("在原路径建空目录");
        let error = verify_root_mount(&base, &snapshot).expect_err("空目录不是原来的 Root");
        assert!(
            matches!(error, MountChange::Drift(RootDrift::DirectoryChanged)),
            "实际：{error}"
        );
        assert_eq!(error.design_error_code(), "ROOT_UNAVAILABLE");
        fs::remove_dir(&base).expect("删掉空目录");
        fs::rename(&moved, &base).expect("还原目录");

        // 权限被收回：不能当成“目录为空”。
        fs::set_permissions(&base, fs::Permissions::from_mode(0o000)).expect("收回权限");
        let denied = verify_root_mount(&base, &snapshot).expect_err("无权限必须失败");
        assert!(
            matches!(denied, MountChange::PermissionDenied { .. }),
            "实际：{denied}"
        );
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).expect("恢复权限");

        fs::remove_dir_all(&base).expect("清理");
    }

    #[test]
    fn readable_directory_check_reports_a_missing_path_as_unreadable() {
        let error = check_readable_dir(Path::new("/definitely/not/here/vaultsyncd"))
            .expect_err("不存在的路径必须失败");
        assert!(matches!(error, MountChange::Unreadable { .. }));
    }
}
