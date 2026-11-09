//! 离线恢复目录能力：内核存储准入、无别名空目录、安全创建和空间预算。
use crate::{
    files::{ResolveMode, RestrictedDir, vault::VaultSnapshot},
    mounts::{
        admission::{self, AdmissionDecision, AdmissionInput},
        mountinfo,
        probe::{self, ScanPolicy, StorageFacts},
        raw,
    },
};
use std::{io, os::fd::AsRawFd, path::Path};
fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
/// 已锚定且通过btrfs准入的目标目录，调用者须在修改前获得状态独占锁。
pub struct Target {
    /// 不跟随符号链接、禁止跨挂载的目录能力。
    pub dir: RestrictedDir,
    /// 当次内核事实，之后仍需复核身份。
    pub facts: StorageFacts,
}
impl Target {
    /// 仅已有规范目录；拒绝符号链接/别名、NoCoW、子挂载/子卷或不完整扫描。
    pub fn open(path: &Path) -> io::Result<Self> {
        let resolved = std::fs::canonicalize(path)?;
        if resolved.as_os_str() != path.as_os_str() {
            return Err(err(
                "restore target must use canonical path without symlinks",
            ));
        }
        let dir = RestrictedDir::open(path, ResolveMode::Auto).map_err(err)?;
        let facts = probe::probe_path(path).map_err(err)?;
        let identity = admission::storage_identity(&facts)
            .ok_or_else(|| err("restore needs btrfs identity"))?;
        let scan =
            probe::scan_vault(path, identity.subvolume_id, &ScanPolicy::default()).map_err(err)?;
        let mounts = mountinfo::read_self().map_err(err)?;
        let outcome = admission::evaluate(&AdmissionInput {
            vault: &facts,
            scan: &scan,
            mounts: &mounts,
            internal_state_dirs: &[],
            registered_vaults: &[],
        });
        if !matches!(outcome.decision, AdmissionDecision::Accepted) {
            return Err(err(format!(
                "restore backend rejected: {:?}",
                outcome.decision
            )));
        }
        let handle = dir.identity().map_err(err)?;
        if handle.inode != facts.inode {
            return Err(err("restore target changed while opening"));
        }
        Ok(Self { dir, facts })
    }
    /// 仅允许明确列出的既有条目；未知数据必须保留且拒绝恢复。
    pub fn empty_except(&self, allowed: &[&str]) -> io::Result<()> {
        for e in self.dir.list_entries().map_err(err)? {
            if !allowed
                .iter()
                .any(|name| e.name == std::ffi::OsStr::new(name))
            {
                return Err(err("restore target is not empty"));
            }
        }
        Ok(())
    }
    /// 按已打开目录句柄查询非特权可用空间；不猜测同子卷容量独立。
    pub fn available(&self) -> io::Result<u64> {
        let file = std::fs::File::open(self.dir.proc_fd_path())?;
        Ok(raw::statfs_of_fd(file.as_raw_fd())?.available_bytes)
    }
    /// 完成前复核原目录挂载/身份，不把漂移误报为成功。
    pub fn verify_identity(&self) -> io::Result<()> {
        let current = probe::probe_path(&self.facts.resolved_path).map_err(err)?;
        if VaultSnapshot::of_facts(&current) != VaultSnapshot::of_facts(&self.facts) {
            return Err(err("restore directory identity changed"));
        }
        Ok(())
    }
}
