//! Explicit, lossless provider removal. Incomplete placeholders are never passed to unregister.
use crate::files::{
    VaultHandle,
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
/// Read an existing provider identity without registering or modifying the directory.
/// Unknown/foreign layouts and API failures are never treated as an absent registration.
pub fn registered_identity(path: &Path) -> io::Result<Option<[u8; 32]>> {
    let mut buffer = vec![0u64; 1024];
    let mut returned = 0;
    let result = unsafe {
        super::raw::CfGetSyncRootInfoByPath(
            super::prototype::wide(path).as_ptr(),
            1,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            &mut returned,
        )
    };
    if result < 0 {
        if [
            0x80070186u32 as i32,
            0x80070178u32 as i32,
            0x80070002u32 as i32,
        ]
        .contains(&result)
        {
            return Ok(None);
        }
        return Err(io::Error::other(format!(
            "registration HRESULT {result:08x}"
        )));
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8) };
    if returned < 1088 || bytes[1052..1056] != 32u32.to_le_bytes() {
        return Err(io::Error::other("foreign vault registration"));
    }
    Ok(Some(
        bytes[1056..1088].try_into().map_err(io::Error::other)?,
    ))
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
    let vault = VaultHandle::open(path)?;
    let report = vault.scan();
    if !report.permits_missing_comparison() {
        return Err(io::Error::other(
            "UNREGISTER_REFUSED: incomplete vault scan",
        ));
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
    let vault = VaultHandle::open(path)?;
    let mut report = vault.scan();
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
        vault.with_parent(&item.path, |parent, leaf| {
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
/// Remove only a vault already verified free of placeholders. No content is deleted.
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
