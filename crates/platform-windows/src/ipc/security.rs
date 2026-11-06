//! Token-based identity and user-only pipe security. No environment identity is trusted.
use std::{ffi::c_void, io, os::windows::io::AsRawHandle};
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer};

type Raw = *mut c_void;
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> Raw;
    fn CloseHandle(handle: Raw) -> i32;
    fn LocalFree(memory: Raw) -> Raw;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Raw;
    fn GetNamedPipeServerProcessId(pipe: Raw, pid: *mut u32) -> i32;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Raw, access: u32, token: *mut Raw) -> i32;
    fn OpenThreadToken(thread: Raw, access: u32, open_as_self: i32, token: *mut Raw) -> i32;
    fn GetTokenInformation(token: Raw, class: u32, buffer: Raw, len: u32, needed: *mut u32) -> i32;
    fn ConvertSidToStringSidW(sid: *const c_void, string: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut Raw,
        size: *mut u32,
    ) -> i32;
    fn ImpersonateNamedPipeClient(pipe: Raw) -> i32;
    fn RevertToSelf() -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThread() -> Raw;
}
struct Handle(Raw);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Local(Raw);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn checked(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn token_user(token: Raw) -> io::Result<String> {
    let mut needed = 0;
    unsafe {
        GetTokenInformation(token, 1, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 || needed > 65536 {
        return Err(io::Error::other("invalid token size"));
    }
    let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
    checked(unsafe {
        GetTokenInformation(token, 1, buffer.as_mut_ptr().cast(), needed, &mut needed)
    })?;
    let mut text = std::ptr::null_mut();
    checked(unsafe { ConvertSidToStringSidW(buffer[0] as Raw, &mut text) })?;
    let _local = Local(text.cast());
    let mut len = 0;
    while len < 1024 && unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    if len == 1024 {
        return Err(io::Error::other("invalid SID"));
    }
    String::from_utf16(unsafe { std::slice::from_raw_parts(text, len) }).map_err(io::Error::other)
}
fn process_token(process: Raw) -> io::Result<Handle> {
    let mut token = std::ptr::null_mut();
    checked(unsafe { OpenProcessToken(process, 8, &mut token) })?;
    Ok(Handle(token))
}
/// Current process user SID and session ID from its token.
pub(super) fn identity() -> io::Result<(String, u32)> {
    let token = process_token(unsafe { GetCurrentProcess() })?;
    let mut session = 0u32;
    let mut needed = 0;
    checked(unsafe {
        GetTokenInformation(
            token.0,
            12,
            (&mut session as *mut u32).cast(),
            4,
            &mut needed,
        )
    })?;
    Ok((token_user(token.0)?, session))
}
#[repr(C)]
pub(super) struct Attributes {
    length: u32,
    descriptor: Raw,
    inherit: i32,
}
/// Descriptor retained until CreateNamedPipeW returns; handles are not inherited.
pub(super) struct Security {
    descriptor: Local,
}
impl Security {
    pub(super) fn new(sid: &str) -> io::Result<Self> {
        let text: Vec<u16> = format!("D:P(A;;GA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = std::ptr::null_mut();
        checked(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        })?;
        Ok(Self {
            descriptor: Local(descriptor),
        })
    }
    pub(super) fn attributes(&self) -> Attributes {
        Attributes {
            length: std::mem::size_of::<Attributes>() as u32,
            descriptor: self.descriptor.0,
            inherit: 0,
        }
    }
}
struct Revert;
impl Drop for Revert {
    fn drop(&mut self) {
        // Never return a worker thread to Tokio under a client's identity.
        if unsafe { RevertToSelf() } == 0 {
            std::process::abort();
        }
    }
}
pub(super) fn check_client(pipe: &NamedPipeServer, sid: &str) -> io::Result<()> {
    checked(unsafe { ImpersonateNamedPipeClient(pipe.as_raw_handle()) })?;
    let _revert = Revert;
    let mut token = std::ptr::null_mut();
    checked(unsafe { OpenThreadToken(GetCurrentThread(), 8, 1, &mut token) })?;
    let token = Handle(token);
    same_user(&token_user(token.0)?, sid)
}
fn same_user(actual: &str, expected: &str) -> io::Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC peer user mismatch",
        ))
    }
}
pub(super) fn check_server(pipe: &NamedPipeClient, sid: &str) -> io::Result<()> {
    let mut pid = 0;
    checked(unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) })?;
    let process = Handle(unsafe { OpenProcess(0x1000, 0, pid) });
    if process.0.is_null() {
        return Err(io::Error::last_os_error());
    }
    let token = process_token(process.0)?;
    same_user(&token_user(token.0)?, sid)
}
