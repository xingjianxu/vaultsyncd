//! Read the documented WebView2 runtime registration, never the Edge browser registration.
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
#[repr(C)]
struct OsVersion {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    service_pack: [u16; 128],
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(version: *mut OsVersion) -> i32;
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn positive_version(value: &str) -> bool {
    let parts: Vec<_> = value.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|part| part.parse::<u32>().is_ok())
        && parts
            .iter()
            .any(|part| part.parse::<u32>().is_ok_and(|n| n > 0))
}
pub(super) fn installed_program() -> io::Result<bool> {
    Ok(installed_directory()?.is_some())
}
pub(super) fn installed_directory() -> io::Result<Option<std::path::PathBuf>> {
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
pub(super) fn require() -> io::Result<()> {
    let mut version = OsVersion {
        size: std::mem::size_of::<OsVersion>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    if unsafe { RtlGetVersion(&mut version) } != 0 || version.major != 10 || version.build < 22000 {
        return Err(io::Error::other(
            "WINDOWS11_REQUIRED: this ZIP supports Windows 11 x64 only",
        ));
    }
    let subkey =
        wide(r"Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}");
    let name = wide("pv");
    // RRF_RT_REG_SZ plus the documented WOW64 view; predefined HKEY values sign-extend.
    for (key, view) in [
        (0x8000_0001u32, 0x0001_0000u32),
        (0x8000_0002u32, 0x0002_0000u32),
    ] {
        let mut data = [0u16; 256];
        let mut size = (data.len() * 2) as u32;
        let status = unsafe {
            RegGetValueW(
                key as i32 as isize as *mut c_void,
                subkey.as_ptr(),
                name.as_ptr(),
                2 | view,
                std::ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status == 0 && size >= 2 && size as usize <= data.len() * 2 {
            let value =
                String::from_utf16(&data[..size as usize / 2 - 1]).map_err(io::Error::other)?;
            if positive_version(&value) {
                return Ok(());
            }
        } else if !matches!(status, 0 | 2 | 3) {
            return Err(io::Error::other(format!(
                "WebView2 registry check failed ({status}); no GUI opened"
            )));
        }
    }
    Err(io::Error::other(
        "WEBVIEW2_REQUIRED: install Microsoft Edge WebView2 Evergreen Runtime from https://developer.microsoft.com/microsoft-edge/webview2/ then retry Start vaultsyncd.cmd. Nothing is downloaded automatically.",
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_requires_a_real_four_component_version() {
        for value in ["", "0.0.0.0", "browser", "1.0", "1.2.3.x"] {
            assert!(!positive_version(value));
        }
        assert!(positive_version("157.0.4341.42"));
    }
}
