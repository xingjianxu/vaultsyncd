//! One GUI window per user session: a named mutex, never a process-name scan.
#![allow(unsafe_code)]
use std::{ffi::c_void, io};
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(attributes: *const c_void, owner: i32, name: *const u16) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetLastError() -> u32;
    fn GetCurrentProcess() -> *mut c_void;
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn GetTokenInformation(
        token: *mut c_void,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        needed: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *const c_void, string: *mut *mut u16) -> i32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}
struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn user_sid() -> io::Result<String> {
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

/// Held for the whole process lifetime; dropping it releases the name.
pub struct SingleInstance {
    _mutex: Handle,
}
/// Claim a per-user, per-session name. `Ok(None)` means another process already holds it.
/// The caller must keep the returned value alive; the mutex is not owned/abandoned-sensitive.
pub fn claim(scope: &str) -> io::Result<Option<SingleInstance>> {
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
    let raw = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let last = unsafe { GetLastError() };
    let mutex = Handle(raw);
    if last == 183 {
        return Ok(None);
    }
    Ok(Some(SingleInstance { _mutex: mutex }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_scope_is_exclusive_until_released_and_scopes_are_independent() {
        let first = claim("s109-desktop-test").unwrap().unwrap();
        assert!(claim("s109-desktop-test").unwrap().is_none());
        assert!(claim("s109-desktop-test-other").unwrap().is_some());
        assert!(claim("bad scope").is_err());
        drop(first);
        assert!(claim("s109-desktop-test").unwrap().is_some());
    }
}
