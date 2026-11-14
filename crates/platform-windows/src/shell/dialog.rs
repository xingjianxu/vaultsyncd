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
    show("vaultsyncd 自动更新", text, 0x40);
}
/// Show a startup failure when the WebView cannot be opened safely.
pub fn startup_error(text: &str) {
    show("vaultsyncd 启动失败", text, 0x10);
}
/// Show desktop settings and orderly-exit failures without depending on the WebView.
pub fn desktop_error(text: &str) {
    show("vaultsyncd 程序配置", text, 0x10);
}
fn show(title: &str, text: &str, flags: u32) {
    let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), flags);
    }
}
