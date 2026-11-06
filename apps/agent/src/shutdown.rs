//! Console lifecycle notification. The handler performs no network, database, or allocation.
#![allow(unsafe_code)]
use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
};
static STOP: AtomicBool = AtomicBool::new(false);
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}
unsafe extern "system" fn handler(event: u32) -> i32 {
    if matches!(event, 0 | 1 | 2 | 5 | 6) {
        STOP.store(true, Ordering::Release);
        1
    } else {
        0
    }
}
/// Installed console signal handler; keep alive until the application has drained.
pub struct Shutdown;
impl Shutdown {
    /// Install Ctrl-C/Break/Close/logoff/shutdown handling.
    pub fn install() -> io::Result<Self> {
        STOP.store(false, Ordering::Release);
        if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self)
    }
    /// Poll from the outer process loop; handlers never hold runtime locks.
    pub fn requested(&self) -> bool {
        STOP.load(Ordering::Acquire)
    }
}
impl Drop for Shutdown {
    fn drop(&mut self) {
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 0);
        }
    }
}
