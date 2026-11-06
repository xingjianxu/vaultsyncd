//! Process ownership before opening the client database.
#![allow(unsafe_code)]
use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io,
    os::windows::fs::OpenOptionsExt,
    path::Path,
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(attributes: *const c_void, owner: i32, name: *const u16) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn GetLastError() -> u32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn GetTokenInformation(
        token: *mut c_void,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        needed: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *const c_void, string: *mut *mut u16) -> i32;
}
struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Current user SID, obtained from the process token rather than environment variables.
pub fn user_sid() -> io::Result<String> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    let mut needed = 0;
    unsafe {
        GetTokenInformation(token.0, 1, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 || needed > 65536 {
        return Err(io::Error::other("invalid token user size"));
    }
    let mut words = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe { GetTokenInformation(token.0, 1, words.as_mut_ptr().cast(), needed, &mut needed) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    let sid = words[0] as *const c_void;
    let mut string = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut string) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0;
    while len < 1024 && unsafe { *string.add(len) } != 0 {
        len += 1;
    }
    let result = String::from_utf16(unsafe { std::slice::from_raw_parts(string, len) })
        .map_err(io::Error::other);
    unsafe {
        LocalFree(string.cast());
    }
    result
}

/// Retain this guard until all runtimes and database connections have shut down.
/// The named mutex enforces one agent per user session; the share-none file also
/// prevents two sessions of the same user from writing the same state directory.
pub struct InstanceGuard {
    _mutex: Handle,
    _state: File,
}
impl InstanceGuard {
    /// Acquire process ownership. The existing state directory must already be validated.
    /// `scope` is the application identity; production always uses `vaultsyncd-agent`.
    pub fn acquire(state: &Path, scope: &str) -> io::Result<Self> {
        if scope.is_empty()
            || scope.len() > 100
            || !scope
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err(io::Error::other("invalid instance scope"));
        }
        let name: Vec<u16> = format!("Local\\{scope}-{}", user_sid()?)
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let raw = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let last = unsafe { GetLastError() };
        let mutex = Handle(raw);
        if last == 183 {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "agent already running in this user session",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .custom_flags(0x00200000)
            .open(state.join("agent.writer.lock"))?;
        if file.metadata()?.file_type().is_symlink() {
            return Err(io::Error::other("state lock is reparse point"));
        }
        Ok(Self {
            _mutex: mutex,
            _state: file,
        })
    }
}
