//! Continuous CFAPI oplocks, fixed snapshots, and guarded local content publication.
use std::{
    ffi::c_void,
    fs::File,
    io,
    mem::ManuallyDrop,
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::Path,
};

#[link(name = "cldapi")]
unsafe extern "system" {
    fn CfOpenFileWithOplock(path: *const u16, flags: u32, handle: *mut *mut c_void) -> i32;
    fn CfReferenceProtectedHandle(handle: *mut c_void) -> u8;
    fn CfGetWin32HandleFromProtectedHandle(handle: *mut c_void) -> *mut c_void;
    fn CfReleaseProtectedHandle(handle: *mut c_void);
    fn CfCloseHandle(handle: *mut c_void);
}
/// CFAPI exclusive RH oplock. A completed break closes the admission gate.
pub struct ProtectedFile(usize);
impl ProtectedFile {
    /// Open through a pinned no-follow Vault, verifying the physical object before returning.
    pub fn open_under(
        vault: &crate::files::VaultHandle,
        path: &str,
        writable: bool,
    ) -> io::Result<(Self, crate::files::EntryMetadata)> {
        content::protect_under(vault, path, writable)
    }
    /// Own a referenced oplock across asynchronous application/database boundaries.
    /// Unlike a temporary lease, this keeps pending break acknowledgement deferred until Drop.
    pub fn into_owned_lease(self) -> io::Result<OwnedProtection> {
        let raw = self.0 as *mut c_void;
        if unsafe { CfReferenceProtectedHandle(raw) } == 0 {
            return Err(io::Error::other("LOCAL_IN_USE: oplock broken"));
        }
        let file = ManuallyDrop::new(unsafe {
            File::from_raw_handle(CfGetWin32HandleFromProtectedHandle(raw))
        });
        Ok(OwnedProtection { owner: self, file })
    }
    /// Open with read and optional write/delete access; failure never means idle.
    pub fn open(path: &Path, writable: bool) -> io::Result<Self> {
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut handle = std::ptr::null_mut();
        let status = unsafe {
            CfOpenFileWithOplock(
                path.as_ptr(),
                1 | if writable { 2 | 4 } else { 0 },
                &mut handle,
            )
        };
        if status < 0 {
            return Err(io::Error::other(format!(
                "LOCAL_IN_USE: CfOpenFileWithOplock {status:08x}"
            )));
        }
        Ok(Self(handle as usize))
    }
    /// Reference the protected handle until the bounded operation completes.
    pub fn lease(&self) -> io::Result<ProtectedLease<'_>> {
        let raw = self.0 as *mut c_void;
        if unsafe { CfReferenceProtectedHandle(raw) } == 0 {
            return Err(io::Error::other("LOCAL_IN_USE: oplock broken"));
        }
        let file = ManuallyDrop::new(unsafe {
            File::from_raw_handle(CfGetWin32HandleFromProtectedHandle(raw))
        });
        Ok(ProtectedLease { owner: self, file })
    }
}
/// Owned referenced Win32 handle; Send/Sync like the underlying File, with no self-reference.
pub struct OwnedProtection {
    owner: ProtectedFile,
    file: ManuallyDrop<File>,
}
impl OwnedProtection {
    /// Borrow without closing or duplicating this handle.
    pub fn file(&self) -> &File {
        &self.file
    }
}
impl Drop for OwnedProtection {
    fn drop(&mut self) {
        unsafe { CfReleaseProtectedHandle(self.owner.0 as *mut c_void) };
    }
}
impl Drop for ProtectedFile {
    fn drop(&mut self) {
        unsafe { CfCloseHandle(self.0 as *mut c_void) }
    }
}
/// Referenced Win32 handle. Dropping acknowledges a pending oplock break.
pub struct ProtectedLease<'a> {
    owner: &'a ProtectedFile,
    file: ManuallyDrop<File>,
}
impl ProtectedLease<'_> {
    /// Borrow for handle-based CFAPI and file operations; never close or duplicate it.
    pub fn file(&self) -> &File {
        &self.file
    }
}
impl Drop for ProtectedLease<'_> {
    fn drop(&mut self) {
        unsafe { CfReleaseProtectedHandle(self.owner.0 as *mut c_void) }
    }
}

pub(crate) fn rename(file: &File, parent: &File, name: &str, flags: u32) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default)]
    struct IoStatus {
        status: usize,
        information: usize,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationFile(
            h: *mut c_void,
            ios: *mut IoStatus,
            buffer: *const c_void,
            length: u32,
            class: i32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
    }
    let name: Vec<u16> = name.encode_utf16().collect();
    let length = 20 + name.len() * 2;
    let mut words = vec![0u64; length.div_ceil(8)];
    let bytes =
        unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), words.len() * 8) };
    bytes[..4].copy_from_slice(&flags.to_le_bytes());
    bytes[8..16].copy_from_slice(&(parent.as_raw_handle() as usize).to_le_bytes());
    bytes[16..20].copy_from_slice(&((name.len() * 2) as u32).to_le_bytes());
    for (out, ch) in bytes[20..length].chunks_exact_mut(2).zip(name) {
        out.copy_from_slice(&ch.to_le_bytes());
    }
    let mut completion = IoStatus {
        status: 0x103,
        information: 0,
    };
    let mut status = unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut completion,
            words.as_ptr().cast(),
            length as u32,
            10,
        )
    };
    if status == 0x103 {
        // Protected Win32 handles can be asynchronous. Keep both IOSB and the rename buffer
        // alive, and keep the caller's CfReference held, until the kernel actually completes.
        loop {
            let waited = unsafe { WaitForSingleObject(file.as_raw_handle(), u32::MAX) };
            if waited != 0 {
                // Returning would let the kernel write into dead stack/buffer memory. An invalid
                // kernel wait handle is an unrecoverable ABI/ownership violation, never success.
                std::process::abort();
            }
            status = unsafe { std::ptr::read_volatile(&completion.status) } as i32;
            if status != 0x103 {
                break;
            }
        }
    }
    if status < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    Ok(())
}

mod content;
mod read;
pub use content::{
    Evidence, FixedSnapshot, ObservedContent, Publication, RecoveryOutcome, WindowsFiles,
};
pub(crate) use read::read_at;

#[cfg(test)]
mod tests;
