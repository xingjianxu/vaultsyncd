//! CfGetWin32HandleFromProtectedHandle may return an asynchronous handle. Never hand it to
//! std::FileExt's synchronous wrapper: ERROR_IO_PENDING must retain the buffer and OVERLAPPED.
use std::{ffi::c_void, fs::File, io, os::windows::io::AsRawHandle};
#[repr(C)]
#[derive(Default)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut c_void,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(
        attributes: *const c_void,
        manual: i32,
        initial: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn ReadFile(
        handle: *mut c_void,
        buffer: *mut u8,
        length: u32,
        read: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
    fn GetOverlappedResult(
        handle: *mut c_void,
        overlapped: *mut Overlapped,
        read: *mut u32,
        wait: i32,
    ) -> i32;
}
struct Event(*mut c_void);
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
pub(crate) fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
    let length = u32::try_from(bytes.len()).map_err(io::Error::other)?;
    if length == 0 {
        return Ok(0);
    }
    let event = Event(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) });
    if event.0.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut overlapped = Overlapped {
        offset: offset as u32,
        offset_high: (offset >> 32) as u32,
        event: event.0,
        ..Default::default()
    };
    let mut read = 0;
    if unsafe {
        ReadFile(
            file.as_raw_handle(),
            bytes.as_mut_ptr(),
            length,
            &mut read,
            &mut overlapped,
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(997) {
            return if error.raw_os_error() == Some(38) {
                Ok(0)
            } else {
                Err(error)
            };
        }
        #[cfg(test)]
        PENDING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The referenced protected handle, event, OVERLAPPED and caller's byte buffer all stay
        // live until completion. A completed I/O error is returned without fabricating bytes.
        if unsafe { GetOverlappedResult(file.as_raw_handle(), &mut overlapped, &mut read, 1) } == 0
        {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(38) {
                Ok(0)
            } else {
                Err(error)
            };
        }
    }
    Ok(read as usize)
}

#[cfg(test)]
static PENDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[test]
fn native_overlapped_pending_read_keeps_buffer_event_and_request_alive() {
    use std::{
        io::Write,
        os::windows::{ffi::OsStrExt, io::FromRawHandle},
    };
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open: u32,
            mode: u32,
            instances: u32,
            out_size: u32,
            in_size: u32,
            timeout: u32,
            security: *const c_void,
        ) -> *mut c_void;
    }
    let name = format!(
        "\\\\.\\pipe\\vaultsyncd-s070-pending-{}",
        std::process::id()
    );
    let wide: Vec<u16> = std::ffi::OsStr::new(&name)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let raw = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            0x40000001,
            0,
            1,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    assert_ne!(raw, -1isize as *mut c_void);
    let reader = unsafe { File::from_raw_handle(raw) };
    let mut writer = std::fs::OpenOptions::new().write(true).open(&name).unwrap();
    let before = PENDING.load(std::sync::atomic::Ordering::SeqCst);
    let thread = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        writer.write_all(b"actual pending IO").unwrap();
    });
    let mut bytes = [0; 17];
    assert_eq!(read_at(&reader, &mut bytes, 0).unwrap(), 17);
    thread.join().unwrap();
    assert_eq!(&bytes, b"actual pending IO");
    assert!(PENDING.load(std::sync::atomic::Ordering::SeqCst) > before);
}
