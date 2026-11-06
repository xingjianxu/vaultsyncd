//! Owned overlapped request: drop cancels and joins I/O before freeing any buffers.

use super::{RescanReason, WatchBatch, decode_notifications};
use crate::files::RootHandle;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::{ffi::c_void, fs::File, io, ptr};

#[repr(C)]
#[derive(Default)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut c_void,
}

const _: () = assert!(size_of::<Overlapped>() == 32);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(
        attributes: *const c_void,
        manual: i32,
        initial: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn ResetEvent(event: *mut c_void) -> i32;
    fn ReadDirectoryChangesW(
        directory: *mut c_void,
        buffer: *mut c_void,
        size: u32,
        subtree: i32,
        filter: u32,
        returned: *mut u32,
        overlapped: *mut Overlapped,
        completion: *const c_void,
    ) -> i32;
    fn WaitForSingleObject(object: *mut c_void, timeout: u32) -> u32;
    fn GetOverlappedResult(
        file: *mut c_void,
        overlapped: *mut Overlapped,
        transferred: *mut u32,
        wait: i32,
    ) -> i32;
}

/// A recursive native watch with bounded polling and explicit hint-loss reporting.
/// The root borrow keeps all ancestor pins alive throughout the asynchronous request.
pub struct DirectoryWatcher<'root> {
    root: &'root RootHandle,
    directory: File,
    event: File,
    request: Box<Overlapped>,
    buffer: Box<[u32]>,
    pending: bool,
}

impl<'root> DirectoryWatcher<'root> {
    /// Start a native watch; buffer size is bounded to 16 B..64 KiB (tiny sizes test overflow).
    ///
    /// # Errors
    /// Returns ACL, unsupported volume or native registration failures.
    pub fn new(root: &'root RootHandle, buffer_bytes: usize) -> io::Result<Self> {
        root.verify()?;
        let directory = root.watch_file()?;
        // SAFETY: null security/name pointers are supported; ownership transfers only on success.
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let event = unsafe { File::from_raw_handle(event) };
        let request = Box::new(Overlapped {
            event: event.as_raw_handle(),
            ..Overlapped::default()
        });
        let mut watcher = Self {
            root,
            directory,
            event,
            request,
            buffer: vec![0; buffer_bytes.clamp(16, 65536).div_ceil(4)].into_boxed_slice(),
            pending: false,
        };
        watcher.arm()?;
        Ok(watcher)
    }

    fn arm(&mut self) -> io::Result<()> {
        if unsafe { ResetEvent(self.event.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: heap buffer and OVERLAPPED remain at fixed addresses until I/O completes.
        let result = unsafe {
            ReadDirectoryChangesW(
                self.directory.as_raw_handle(),
                self.buffer.as_mut_ptr().cast(),
                (self.buffer.len() * 4) as u32,
                1,
                0x15f,
                ptr::null_mut(),
                &mut *self.request,
                ptr::null(),
            )
        };
        if result == 0 && io::Error::last_os_error().raw_os_error() != Some(997) {
            return Err(io::Error::last_os_error());
        }
        self.pending = true;
        Ok(())
    }

    /// Wait at most 60 seconds. Completion failures are explicit rescan requirements.
    #[must_use]
    pub fn poll(&mut self, timeout_ms: u32) -> WatchBatch {
        if self.root.verify().is_err() {
            return WatchBatch::Rescan(RescanReason::WatchLost);
        }
        if !self.pending && self.arm().is_err() {
            return WatchBatch::Rescan(RescanReason::WatchLost);
        }
        match unsafe { WaitForSingleObject(self.event.as_raw_handle(), timeout_ms.min(60000)) } {
            258 => return WatchBatch::Idle,
            0 => {}
            _ => return WatchBatch::Rescan(RescanReason::WatchLost),
        }
        let mut bytes = 0;
        let result = unsafe {
            GetOverlappedResult(
                self.directory.as_raw_handle(),
                &mut *self.request,
                &mut bytes,
                0,
            )
        };
        self.pending = false;
        if result == 0 {
            let error = io::Error::last_os_error().raw_os_error();
            // An incomplete request still owns the buffers; never rearm over it.
            self.pending = error == Some(996);
            return WatchBatch::Rescan(if error == Some(1022) {
                RescanReason::Overflow
            } else {
                RescanReason::WatchLost
            });
        }
        if bytes as usize > self.buffer.len() * 4 {
            return WatchBatch::Rescan(RescanReason::InvalidNotification);
        }
        // SAFETY: bytes were initialized by the completed kernel request and bounded above.
        let batch = decode_notifications(unsafe {
            std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), bytes as usize)
        });
        // Rearm immediately; periodic scans also cover the unavoidable completion/rearm gap.
        if self.arm().is_err() {
            return WatchBatch::Rescan(RescanReason::WatchLost);
        }
        batch
    }
}

impl Drop for DirectoryWatcher<'_> {
    fn drop(&mut self) {
        if self.pending {
            unsafe {
                crate::cfapi::raw::CancelIoEx(
                    self.directory.as_raw_handle(),
                    (&*self.request as *const Overlapped).cast(),
                );
                let mut bytes = 0;
                GetOverlappedResult(
                    self.directory.as_raw_handle(),
                    &mut *self.request,
                    &mut bytes,
                    1,
                );
            }
        }
    }
}
