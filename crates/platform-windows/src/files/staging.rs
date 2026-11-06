//! Download staging in an internal pinned directory, never in a synchronization root.
use super::{
    RootHandle,
    native::{metadata, open_child_shared},
};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::windows::ffi::OsStrExt,
    path::Path,
    sync::{Arc, Mutex},
};
use vaultsyncd_transfer::ports::{LocalErrorKind, PortError, PortFuture, TempContent};

/// A pinned, private regular staging file. All opens are relative/no-follow and the held
/// file refuses other readers/writers, preventing replacement between validation and CFAPI.
pub struct StagingFile {
    root: Arc<RootHandle>,
    file: Mutex<File>,
    path: String,
    directory: Vec<u16>,
}
impl StagingFile {
    /// Open or create a single-name internal file. A missing file is created without overwrite;
    /// an existing symlink, cloud placeholder, hardlink, directory or volume escape is rejected.
    pub fn open(directory: &Path, name: &str) -> io::Result<Self> {
        if name.contains(['/', '\\']) {
            return Err(io::Error::other("staging name must be flat"));
        }
        let root = Arc::new(RootHandle::open(directory)?);
        let file = root.with_parent(name, |parent, name| {
            let file = match open_child_shared(parent, name, false, false, 0x0010_0183, false, 0) {
                Ok(f) => f,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    open_child_shared(parent, name, false, false, 0x0010_0183, true, 0)?
                }
                Err(error) => return Err(error),
            };
            let meta = metadata(&file)?;
            if meta.directory
                || meta.reparse_tag != 0
                || meta.cloud.is_some()
                || meta.links != 1
                || meta.identity.volume != root.identity().volume
            {
                return Err(io::Error::other("unsafe staging object"));
            }
            Ok(file)
        })?;
        Ok(Self {
            root,
            file: Mutex::new(file),
            path: directory
                .join(name)
                .to_str()
                .ok_or_else(|| io::Error::other("non UTF8 staging path"))?
                .to_owned(),
            directory: directory.as_os_str().encode_wide().chain(Some(0)).collect(),
        })
    }
    fn run<T>(&self, f: impl FnOnce(&mut File) -> io::Result<T>) -> Result<T, PortError> {
        self.root.verify().map_err(local)?;
        f(&mut *self
            .file
            .lock()
            .map_err(|_| PortError::protocol("staging lock poisoned"))?)
        .map_err(local)
    }
}
fn local(e: io::Error) -> PortError {
    PortError::local(
        if e.kind() == io::ErrorKind::StorageFull {
            LocalErrorKind::NoSpace
        } else {
            LocalErrorKind::Write
        },
        e.to_string(),
    )
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetDiskFreeSpaceExW(
        path: *const u16,
        available: *mut u64,
        total: *mut u64,
        free: *mut u64,
    ) -> i32;
}
impl TempContent for StagingFile {
    fn path(&self) -> &str {
        &self.path
    }
    fn ensure_capacity(&self, needed: u64) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let existing = self.run(|f| Ok(f.metadata()?.len()))?;
            let mut available = 0;
            if unsafe {
                GetDiskFreeSpaceExW(
                    self.directory.as_ptr(),
                    &mut available,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(local(io::Error::last_os_error()));
            }
            if available < needed.saturating_sub(existing) {
                return Err(PortError::local(
                    LocalErrorKind::NoSpace,
                    "insufficient staging capacity",
                ));
            }
            Ok(())
        })
    }
    fn write_at<'a>(
        &'a self,
        offset: u64,
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            self.run(|f| {
                f.seek(SeekFrom::Start(offset))?;
                f.write_all(bytes)?;
                f.sync_data()
            })
        })
    }
    fn read_at(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            self.run(|f| {
                let mut b = vec![0; usize::try_from(length).map_err(io::Error::other)?];
                f.seek(SeekFrom::Start(offset))?;
                f.read_exact(&mut b)?;
                Ok(b)
            })
        })
    }
    fn sync(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.run(|f| f.sync_all()) })
    }
    fn discard(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            self.run(|f| {
                f.set_len(0)?;
                f.sync_all()
            })
        })
    }
}
