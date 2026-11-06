//! Windows Credential Manager persistence; credential bytes never enter configuration or logs.
#![allow(unsafe_code)]
use std::{ffi::c_void, io};
#[repr(C)]
struct Credential {
    flags: u32,
    kind: u32,
    target: *mut u16,
    comment: *mut u16,
    written: [u32; 2],
    size: u32,
    blob: *mut u8,
    persist: u32,
    attributes: u32,
    attribute: *mut c_void,
    alias: *mut u16,
    user: *mut u16,
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn CredWriteW(credential: *const Credential, flags: u32) -> i32;
    fn CredReadW(
        target: *const u16,
        kind: u32,
        flags: u32,
        credential: *mut *mut Credential,
    ) -> i32;
    fn CredDeleteW(target: *const u16, kind: u32, flags: u32) -> i32;
    fn CredFree(buffer: *mut c_void);
}
/// A current-user generic credential scoped to a stable server identity.
pub struct CredentialVault {
    target: Vec<u16>,
}
impl CredentialVault {
    /// Stable ID must be hexadecimal and is never a token or password.
    pub fn new(server: &str) -> io::Result<Self> {
        if server.len() != 32 || !server.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(io::Error::other("invalid credential server identity"));
        }
        Ok(Self {
            target: format!("vaultsyncd/session/{server}")
                .encode_utf16()
                .chain(Some(0))
                .collect(),
        })
    }
    /// Persist atomically in Credential Manager. Any error must block use of newly issued credentials.
    pub fn store(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() || bytes.len() > 5120 {
            return Err(io::Error::other("credential payload exceeds Windows bound"));
        }
        let value = Credential {
            flags: 0,
            kind: 1,
            target: self.target.as_ptr().cast_mut(),
            comment: std::ptr::null_mut(),
            written: [0; 2],
            size: bytes.len() as u32,
            blob: bytes.as_ptr().cast_mut(),
            persist: 2,
            attributes: 0,
            attribute: std::ptr::null_mut(),
            alias: std::ptr::null_mut(),
            user: std::ptr::null_mut(),
        };
        if unsafe { CredWriteW(&value, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    /// Missing credentials require login; no fallback to plaintext files.
    pub fn load(&self) -> io::Result<Option<Vec<u8>>> {
        let mut value = std::ptr::null_mut();
        if unsafe { CredReadW(self.target.as_ptr(), 1, 0, &mut value) } == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(1168) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        let result = unsafe {
            let c = &*value;
            if c.size == 0 || c.size > 5120 || c.blob.is_null() {
                Err(io::Error::other("malformed Windows credential"))
            } else {
                Ok(Some(
                    std::slice::from_raw_parts(c.blob, c.size as usize).to_vec(),
                ))
            }
        };
        unsafe {
            CredFree(value.cast());
        }
        result
    }
    /// Remove only this server's credential; absence is idempotent.
    pub fn erase(&self) -> io::Result<()> {
        if unsafe { CredDeleteW(self.target.as_ptr(), 1, 0) } != 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(1168) {
            Ok(())
        } else {
            Err(e)
        }
    }
}
