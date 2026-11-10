//! Legacy installed-client registration, used only to preserve installer maintenance behavior.
#![allow(unsafe_code)]
use std::{ffi::c_void, io};
#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        key: *mut c_void,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut c_void,
        size: *mut u32,
    ) -> i32;
}
pub(super) fn installed_directory() -> io::Result<Option<std::path::PathBuf>> {
    let wide = |value: &str| value.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let subkey = wide(r"Software\vaultsyncd");
    let name = wide("InstallDir");
    let mut data = [0u16; 32768];
    let mut size = (data.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            0x8000_0001u32 as i32 as isize as *mut c_void,
            subkey.as_ptr(),
            name.as_ptr(),
            2 | 0x0001_0000,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    match status {
        0 if size > 2 && size as usize <= data.len() * 2 => {
            let len = size as usize / 2 - 1;
            let text = String::from_utf16(&data[..len]).map_err(io::Error::other)?;
            Ok(Some(text.into()))
        }
        0 | 2 | 3 => Ok(None),
        _ => Err(io::Error::other(format!(
            "installed client registry check failed ({status})"
        ))),
    }
}
