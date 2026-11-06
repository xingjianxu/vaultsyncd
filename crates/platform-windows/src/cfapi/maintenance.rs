//! Explicit, lossless provider removal. Incomplete placeholders are never passed to unregister.
use crate::files::{
    RootHandle,
    native::{metadata, open_child_shared},
};
use std::{
    fs::File,
    io::{self, Read, Seek},
    os::windows::io::AsRawHandle,
    path::Path,
};
use vaultsyncd_transfer::hash::{ContentHasher, to_hex};
#[link(name = "cldapi")]
unsafe extern "system" {
    fn CfRevertPlaceholder(
        file: *mut std::ffi::c_void,
        flags: u32,
        overlapped: *mut std::ffi::c_void,
    ) -> i32;
    fn CfUnregisterSyncRoot(path: *const u16) -> i32;
}
fn hash(file: &mut File) -> io::Result<String> {
    file.rewind()?;
    let mut h = ContentHasher::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(to_hex(&h.finish()))
}
/// Scan without recall. Unknown scans, reparses, partial or cloud-only files refuse removal.
pub fn check_materialized(path: &Path) -> io::Result<()> {
    let root = RootHandle::open(path)?;
    let report = root.scan();
    if !report.permits_missing_comparison() {
        return Err(io::Error::other("UNREGISTER_REFUSED: incomplete root scan"));
    }
    for item in report.entries {
        let m = item
            .metadata
            .ok_or_else(|| io::Error::other("UNREGISTER_REFUSED: unknown metadata"))?;
        if !m.supported()
            || m.links != 1
            || (!m.directory
                && m.placeholder()
                && m.cloud.as_ref().is_none_or(|c| c.on_disk < m.size))
        {
            return Err(io::Error::other(
                "UNREGISTER_REFUSED: download all cloud-only/partial content before provider removal",
            ));
        }
    }
    Ok(())
}
/// Revert only verified fully materialized objects, retaining hashes and every ordinary file.
/// Returns the preserved file hashes; errors retain data and prohibit subsequent unregister.
pub fn revert_materialized(path: &Path) -> io::Result<Vec<(String, String)>> {
    check_materialized(path)?;
    let root = RootHandle::open(path)?;
    let mut report = root.scan();
    if !report.permits_missing_comparison() {
        return Err(io::Error::other("UNREGISTER_REFUSED: rescan failed"));
    }
    report
        .entries
        .sort_by_key(|e| std::cmp::Reverse(e.path.matches('/').count()));
    let mut hashes = Vec::new();
    for item in report.entries {
        let observed = item
            .metadata
            .ok_or_else(|| io::Error::other("unknown entry"))?;
        root.with_parent(&item.path, |parent, leaf| {
            let mut file = open_child_shared(
                parent,
                leaf,
                observed.directory,
                false,
                0x0012_019f,
                false,
                0,
            )?;
            let current = metadata(&file)?;
            if current.identity != observed.identity
                || !current.supported()
                || (!current.directory
                    && current.placeholder()
                    && current
                        .cloud
                        .as_ref()
                        .is_none_or(|c| c.on_disk < current.size))
            {
                return Err(io::Error::other("LOCAL_CHANGED: do not unregister"));
            }
            let before = if !current.directory {
                Some(hash(&mut file)?)
            } else {
                None
            };
            if current.placeholder() {
                let hr =
                    unsafe { CfRevertPlaceholder(file.as_raw_handle(), 0, std::ptr::null_mut()) };
                if hr < 0 {
                    return Err(io::Error::other(format!("revert HRESULT {hr:08x}")));
                }
            }
            let after = metadata(&file)?;
            if after.placeholder() || after.identity != current.identity {
                return Err(io::Error::other(
                    "revert did not preserve ordinary identity",
                ));
            }
            if let Some(before) = before {
                if hash(&mut file)? != before {
                    return Err(io::Error::other("revert content hash changed"));
                }
                hashes.push((item.path.clone(), before));
            }
            Ok(())
        })?;
    }
    super::adapter::ensure_no_placeholders(path)?;
    Ok(hashes)
}
/// Remove only a root already verified free of placeholders. No content is deleted.
pub fn unregister_materialized(path: &Path) -> io::Result<()> {
    super::adapter::ensure_no_placeholders(path)?;
    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let hr = unsafe { CfUnregisterSyncRoot(wide.as_ptr()) };
    if hr < 0 && hr as u32 != 0x80070186 {
        return Err(io::Error::other(format!("unregister HRESULT {hr:08x}")));
    }
    Ok(())
}
