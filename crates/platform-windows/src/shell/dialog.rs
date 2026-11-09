//! Native status dialog usable while the WebView is closed during maintenance.
#![allow(unsafe_code)]
#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(
        window: *mut std::ffi::c_void,
        text: *const u16,
        title: *const u16,
        flags: u32,
    ) -> i32;
}
/// Show a user-visible update result without executing any content or shell command.
pub fn update_result(text: &str) {
    let title: Vec<u16> = "vaultsyncd 自动更新"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0x40);
    }
}
