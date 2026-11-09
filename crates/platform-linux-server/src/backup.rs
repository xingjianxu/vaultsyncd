//! 备份专用btrfs准备区；固定子卷名字，不提供任意挂载、格式化或路径删除命令。
use crate::{
    files::{ResolveMode, RestrictedDir},
    mounts::probe::{self, ScanCompleteness, ScanPolicy},
};
use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};
fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
fn btrfs(args: &[&str], paths: &[&Path]) -> io::Result<()> {
    let result = Command::new("btrfs")
        .env("LC_ALL", "C")
        .args(args)
        .arg("--")
        .args(paths)
        .output()?;
    if !result.status.success() {
        return Err(err(format!(
            "btrfs {:?}: {}",
            args,
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    Ok(())
}
/// 服务用户拥有的私有备份基目录。其挂载必须明确允许拥有者删除子卷。
pub struct Arena {
    base: PathBuf,
}
impl Arena {
    /// 不授予任何权限；缺少后端或user_subvol_rm_allowed时直接拒绝，不留下半份快照。
    pub fn open(base: &Path) -> io::Result<Self> {
        if !base.exists() {
            fs::DirBuilder::new().mode(0o700).create(base)?;
        }
        let metadata = fs::symlink_metadata(base)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(err("backup arena must be private directory mode 0700"));
        }
        let facts = probe::probe_path(base).map_err(err)?;
        if facts.btrfs.is_none()
            || facts.mount.has_option("nodatacow")
            || facts.mount.has_option("nodatasum")
            || facts.inode_flags.is_none_or(|f| f & 0x0080_0000 != 0)
        {
            return Err(err("backup arena requires CoW btrfs"));
        }
        if !facts.mount.has_option("user_subvol_rm_allowed") {
            return Err(err(
                "backup arena mount needs user_subvol_rm_allowed; service gets no mount privilege",
            ));
        }
        RestrictedDir::open(base, ResolveMode::Auto).map_err(err)?;
        Ok(Self {
            base: fs::canonicalize(base)?,
        })
    }
    fn directory(&self, id: &str) -> io::Result<PathBuf> {
        if id.len() != 32
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(err("invalid backup identity"));
        }
        Ok(self.base.join(id))
    }
    /// 创建一个独占、没有嵌套子卷的准备子卷。
    pub fn create(&self, id: &str) -> io::Result<PathBuf> {
        let dir = self.directory(id)?;
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let prepared = dir.join("prepared");
        btrfs(&["subvolume", "create"], &[&prepared])?;
        fs::set_permissions(&prepared, fs::Permissions::from_mode(0o700))?;
        fs::File::open(&dir)?.sync_all()?;
        Ok(prepared)
    }
    /// 仅返回本备份的固定只读快照路径。
    pub fn snapshot_path(&self, id: &str) -> io::Result<PathBuf> {
        Ok(self.directory(id)?.join("snapshot"))
    }
    /// 复核没有嵌套子卷/CoW禁用/扫描缺口，创建只读快照并核对ro属性。
    pub fn freeze(&self, id: &str) -> io::Result<PathBuf> {
        let dir = self.directory(id)?;
        let prepared = dir.join("prepared");
        self.check_flat(&prepared)?;
        let snapshot = dir.join("snapshot");
        btrfs(&["subvolume", "snapshot", "-r"], &[&prepared, &snapshot])?;
        if !self.is_read_only(id)? {
            return Err(err("snapshot was not read-only"));
        }
        fs::File::open(&dir)?.sync_all()?;
        btrfs(&["subvolume", "delete"], &[&prepared])?;
        fs::File::open(&dir)?.sync_all()?;
        Ok(snapshot)
    }
    /// 固定路径只读属性验证，不依赖命令退出成功来推断。
    pub fn is_read_only(&self, id: &str) -> io::Result<bool> {
        let path = self.snapshot_path(id)?;
        self.check_flat(&path)?;
        let out = Command::new("btrfs")
            .env("LC_ALL", "C")
            .args(["property", "get", "-ts"])
            .arg(path)
            .arg("ro")
            .output()?;
        if !out.status.success() {
            return Err(err(String::from_utf8_lossy(&out.stderr)));
        }
        Ok(out.stdout == b"ro=true\n")
    }
    fn check_flat(&self, path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() {
            return Err(err("backup subvolume path is not a directory"));
        }
        let facts = probe::probe_path(path).map_err(err)?;
        if facts.inode != 256 {
            return Err(err("backup path is not a subvolume vault"));
        }
        let id = facts
            .btrfs
            .as_ref()
            .and_then(|f| f.subvolume_id)
            .ok_or_else(|| err("backup subvolume identity unavailable"))?;
        let scan = probe::scan_vault(
            path,
            id,
            &ScanPolicy {
                max_entries: 1_000_000,
                max_depth: 8,
            },
        )
        .map_err(err)?;
        if scan.completeness != ScanCompleteness::Complete
            || !scan.subvolumes.is_empty()
            || !scan.nocow_directories.is_empty()
        {
            return Err(err(
                "backup preparation has nested subvolumes/NoCoW or incomplete scan",
            ));
        }
        Ok(())
    }
    /// 只清这个持久ID的prepared/snapshot；未知条目、符号链接、非子卷和嵌套子卷均保留报错。
    pub fn remove(&self, id: &str) -> io::Result<()> {
        let dir = self.directory(id)?;
        if !dir.try_exists()? {
            return Ok(());
        }
        let handle = RestrictedDir::open(&dir, ResolveMode::Auto).map_err(err)?;
        for entry in handle.list_entries().map_err(err)? {
            if entry.name != "prepared" && entry.name != "snapshot" {
                return Err(err("unknown backup artifact; preserved"));
            }
            let path = dir.join(&entry.name);
            self.check_flat(&path)?;
            // 调用方已持久进入failed/releasing后才可清只读标志；普通用户删除ro子卷会得到EROFS。
            if entry.name == "snapshot" {
                let out = Command::new("btrfs")
                    .env("LC_ALL", "C")
                    .args(["property", "set", "-ts"])
                    .arg(&path)
                    .args(["ro", "false"])
                    .output()?;
                if !out.status.success() {
                    return Err(err(String::from_utf8_lossy(&out.stderr)));
                }
            }
            btrfs(&["subvolume", "delete"], &[&path])?;
        }
        fs::File::open(&dir)?.sync_all()?;
        fs::remove_dir(&dir)?;
        fs::File::open(&self.base)?.sync_all()
    }
}
