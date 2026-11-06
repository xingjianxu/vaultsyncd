//! Every component is opened relative to an inspected directory HANDLE with
//! FILE_OPEN_REPARSE_POINT. An in-place junction change cannot redirect traversal.

use super::{CloudMetadata, EntryMetadata, FileIdentity, ScanEntry, ScanHealth, ScanReport};
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::{
    ffi::OsStrExt,
    fs::OpenOptionsExt,
    io::{AsRawHandle, FromRawHandle},
};
use std::path::{Component, Path, PathBuf, Prefix};
use vaultsyncd_fs_common::{
    name::{NameEquivalence, validate_name},
    path::RelativePath,
};

const FLAGS: u32 = 0x0200_0000 | 0x0020_0000 | 0x0010_0000;

#[repr(C)]
#[derive(Default)]
struct IdInfo {
    volume: u64,
    file: [u8; 16],
}
#[repr(C)]
#[derive(Default)]
struct AttributeInfo {
    attributes: u32,
    tag: u32,
}
#[repr(C)]
#[derive(Default)]
struct StandardInfo {
    allocation: i64,
    size: i64,
    links: u32,
    delete_pending: u8,
    directory: u8,
}
#[repr(C)]
#[derive(Default)]
struct BasicInfo {
    creation: i64,
    access: i64,
    write: i64,
    change: i64,
    attributes: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandleEx(
        handle: *mut c_void,
        class: i32,
        info: *mut c_void,
        size: u32,
    ) -> i32;
    fn CompareStringOrdinal(
        left: *const u16,
        left_len: i32,
        right: *const u16,
        right_len: i32,
        ignore_case: i32,
    ) -> i32;
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum: u16,
    buffer: *const u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: *mut c_void,
    name: *mut UnicodeString,
    attributes: u32,
    security: *const c_void,
    qos: *const c_void,
}
#[repr(C)]
#[derive(Default)]
struct IoStatus {
    status: usize,
    information: usize,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        handle: *mut *mut c_void,
        access: u32,
        attributes: *const ObjectAttributes,
        status: *mut IoStatus,
        allocation: *const i64,
        file_attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_length: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}
const _: () = {
    assert!(size_of::<IdInfo>() == 24);
    assert!(size_of::<AttributeInfo>() == 8);
    assert!(size_of::<StandardInfo>() == 24);
    assert!(size_of::<BasicInfo>() == 40);
    assert!(size_of::<UnicodeString>() == 16);
    assert!(size_of::<ObjectAttributes>() == 48);
    assert!(size_of::<IoStatus>() == 16);
};

fn open_relative(parent: &File, name: &str, directory: bool) -> io::Result<File> {
    open_relative_mode(parent, name, directory, false)
}

fn open_relative_mode(
    parent: &File,
    name: &str,
    directory: bool,
    asynchronous: bool,
) -> io::Result<File> {
    open_child(
        parent,
        name,
        directory,
        asynchronous,
        0x0010_0080 | u32::from(directory),
        false,
    )
}

pub(crate) fn open_child(
    parent: &File,
    name: &str,
    directory: bool,
    asynchronous: bool,
    access: u32,
    create: bool,
) -> io::Result<File> {
    open_child_shared(parent, name, directory, asynchronous, access, create, 3)
}

pub(crate) fn open_child_shared(
    parent: &File,
    name: &str,
    directory: bool,
    asynchronous: bool,
    access: u32,
    create: bool,
    share: u32,
) -> io::Result<File> {
    validate_name(name).map_err(invalid)?;
    let wide: Vec<u16> = name.encode_utf16().collect();
    let mut text = UnicodeString {
        length: (wide.len() * 2) as u16,
        maximum: (wide.len() * 2) as u16,
        buffer: wide.as_ptr(),
    };
    let attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root: parent.as_raw_handle(),
        name: &mut text,
        attributes: 0x40,
        security: std::ptr::null(),
        qos: std::ptr::null(),
    };
    let mut status = IoStatus::default();
    let mut handle = std::ptr::null_mut();
    // RootDirectory prevents traversal through a changed textual ancestor. Name is one
    // validated component. OPEN_REPARSE_POINT opens the object itself, never its target.
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status,
            std::ptr::null(),
            0,
            share,
            if create { 2 } else { 1 },
            0x0020_0000
                | (if create && directory { 0 } else { 0x0040_0000 })
                | (if create {
                    if directory { 1 } else { 0x40 }
                } else {
                    0
                })
                | (if asynchronous { 0 } else { 0x20 }),
            std::ptr::null(),
            0,
        )
    };
    if result < 0 {
        let error = io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(result) } as i32);
        return Err(io::Error::new(
            error.kind(),
            format!("NtCreateFile component={name:?} status={result:08x}: {error}"),
        ));
    }
    let file = unsafe { File::from_raw_handle(handle) };
    if directory && !metadata(&file)?.directory {
        return Err(invalid("ancestor is not a directory"));
    }
    Ok(file)
}

// FILE_ID_EXTD_DIR_INFO has a fixed 88-byte header followed by UTF-16 FileName.
fn directory_names(file: &File) -> io::Result<Vec<String>> {
    let mut buffer = vec![0u64; 8192];
    let mut names = vec![];
    let mut restart = true;
    loop {
        buffer.fill(0);
        let result = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                if restart { 20 } else { 19 },
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
            )
        };
        restart = false;
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(18) {
                return Ok(names);
            }
            return Err(error);
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8) };
        let mut offset = 0usize;
        loop {
            let header = bytes
                .get(offset..offset + 88)
                .ok_or_else(|| invalid("directory record header truncated"))?;
            let next = u32::from_le_bytes(header[0..4].try_into().expect("four bytes")) as usize;
            let length =
                u32::from_le_bytes(header[60..64].try_into().expect("four bytes")) as usize;
            if length == 0 || length % 2 != 0 {
                return Err(invalid("invalid directory name length"));
            }
            let name = bytes
                .get(offset + 88..offset + 88 + length)
                .ok_or_else(|| invalid("directory name truncated"))?;
            let wide: Vec<_> = name
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            let name = String::from_utf16(&wide).map_err(invalid)?;
            if name != "." && name != ".." {
                names.push(name);
            }
            if next == 0 {
                break;
            }
            if next % 8 != 0 || next < 88 + length {
                return Err(invalid("invalid directory record offset"));
            }
            offset += next;
        }
    }
}

fn query<T: Default>(file: &File, class: i32) -> io::Result<T> {
    let mut info = T::default();
    // SAFETY: class and T pairs below match the SDK ABI; the buffer remains live.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            class,
            (&mut info as *mut T).cast(),
            size_of::<T>() as u32,
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("metadata class={class}: {error}"),
        ));
    }
    Ok(info)
}

fn cloud_metadata(file: &File) -> io::Result<Option<CloudMetadata>> {
    use crate::cfapi::raw::{
        CF_PLACEHOLDER_INFO_STANDARD, CfGetPlaceholderInfo, CfPlaceholderStandardInfo,
        HRESULT_FROM_WIN32_ERROR_NOT_A_CLOUD_FILE,
    };
    let mut buffer = vec![0u64; (64 + 4096) / 8];
    let mut returned = 0u32;
    let result = unsafe {
        CfGetPlaceholderInfo(
            file.as_raw_handle(),
            CF_PLACEHOLDER_INFO_STANDARD,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            &mut returned,
        )
    };
    if result == HRESULT_FROM_WIN32_ERROR_NOT_A_CLOUD_FILE {
        return Ok(None);
    }
    if result < 0 {
        return Err(io::Error::other(format!(
            "CfGetPlaceholderInfo failed: {result:08x}"
        )));
    }
    let info =
        unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast::<CfPlaceholderStandardInfo>()) };
    let length = info.file_identity_length as usize;
    if length > 4096 || returned as usize > buffer.len() * 8 || (returned as usize) < 60 + length {
        return Err(invalid("invalid CFAPI metadata length"));
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8) };
    Ok(Some(CloudMetadata {
        on_disk: info
            .on_disk_data_size
            .try_into()
            .map_err(io::Error::other)?,
        modified: info
            .modified_data_size
            .try_into()
            .map_err(io::Error::other)?,
        pin_state: info.pin_state,
        in_sync_state: info.in_sync_state,
        identity: bytes[60..60 + length].to_vec(),
    }))
}

pub(crate) fn metadata(file: &File) -> io::Result<EntryMetadata> {
    let id: IdInfo = query(file, 18)?;
    let attr: AttributeInfo = query(file, 9)?;
    let standard: StandardInfo = query(file, 1)?;
    let basic: BasicInfo = query(file, 0)?;
    Ok(EntryMetadata {
        identity: FileIdentity {
            volume: id.volume,
            file: id.file,
        },
        size: standard.size.try_into().map_err(io::Error::other)?,
        allocated: standard.allocation.try_into().map_err(io::Error::other)?,
        directory: standard.directory != 0,
        attributes: attr.attributes,
        reparse_tag: attr.tag,
        links: standard.links,
        created: basic.creation,
        modified: basic.write,
        cloud: cloud_metadata(file)?,
    })
}

fn open(path: &Path) -> io::Result<File> {
    // FILE_READ_ATTRIBUTES only: no GENERIC_READ and no content recall.
    OpenOptions::new()
        .access_mode(0x81)
        .share_mode(3)
        .custom_flags(FLAGS)
        .open(path)
}

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_string())
}

/// Native ordinal case-insensitive comparison, conservative also in case-sensitive directories.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeNames;
impl NameEquivalence for NativeNames {
    fn equivalent(&self, left: &str, right: &str) -> bool {
        let left: Vec<u16> = left.encode_utf16().collect();
        let right: Vec<u16> = right.encode_utf16().collect();
        let (Ok(ll), Ok(rl)) = (left.len().try_into(), right.len().try_into()) else {
            return true;
        };
        // Fail closed on API error (0): report a collision rather than permit overwrite.
        matches!(
            unsafe { CompareStringOrdinal(left.as_ptr(), ll, right.as_ptr(), rl, 1) },
            0 | 2
        )
    }
}

/// Pinned local root and all of its ancestors. Only local drive paths are supported.
pub struct RootHandle {
    path: PathBuf,
    pins: Vec<File>,
    ancestors: Vec<FileIdentity>,
    identity: FileIdentity,
}

impl RootHandle {
    /// Open an existing directory without following any reparse in its ancestry.
    ///
    /// # Errors
    /// Rejects relative/UNC/device paths, reparses, unavailable volumes and access failures.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut components = path.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(invalid("absolute local drive path required"));
        };
        let Prefix::Disk(drive) = prefix.kind() else {
            return Err(invalid("UNC/device/extended input paths are unsupported"));
        };
        if components.next() != Some(Component::RootDir) {
            return Err(invalid("drive-relative path is unsafe"));
        }
        let mut current = PathBuf::from(format!("\\\\?\\{}:\\", drive as char));
        let drive_pin = open(&current)?;
        let drive_meta = metadata(&drive_pin)?;
        if !drive_meta.directory || drive_meta.reparse_tag != 0 {
            return Err(invalid("drive root is a reparse or non-directory"));
        }
        let mut pins = vec![drive_pin];
        let mut ancestors = vec![drive_meta.identity];
        let components: Vec<_> = components.collect();
        let count = components.len();
        for (index, component) in components.into_iter().enumerate() {
            let Component::Normal(name) = component else {
                return Err(invalid("unsafe root component"));
            };
            let text = name
                .to_str()
                .ok_or_else(|| invalid("root name is not UTF-8"))?;
            let file = open_relative(pins.last().expect("drive pin"), text, true)?;
            let meta = metadata(&file)?;
            if !meta.directory
                || (meta.reparse_tag != 0 && (index + 1 != count || !meta.supported()))
            {
                return Err(invalid(
                    "root ancestry contains an unsupported reparse or non-directory",
                ));
            }
            current.push(name);
            ancestors.push(meta.identity);
            pins.push(file);
        }
        let identity = *ancestors
            .last()
            .ok_or_else(|| invalid("root identity absent"))?;
        Ok(Self {
            path: current,
            pins,
            ancestors,
            identity,
        })
    }

    /// Root's physical identity.
    #[must_use]
    pub fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Detect equality or ancestry by directory identity, including drive-letter aliases.
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        self.ancestors.contains(&other.identity) || other.ancestors.contains(&self.identity)
    }

    /// Recheck the pinned root's physical identity.
    ///
    /// # Errors
    /// An inaccessible or changed root never becomes an empty successful scan.
    pub fn verify(&self) -> io::Result<()> {
        let file = self
            .pins
            .last()
            .ok_or_else(|| invalid("missing root pin"))?;
        let current = metadata(file)?;
        if current.identity != self.identity || !current.supported() {
            return Err(io::Error::other("root identity changed"));
        }
        // Recheck current ACLs through a fresh rooted open, without backup intent.
        // Existing handles keep granted access even after a deny ACL is installed.
        if self.pins.len() > 1 {
            let name = self
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid("root has no name"))?;
            let checked = open_relative(&self.pins[self.pins.len() - 2], name, true)?;
            if metadata(&checked)?.identity != self.identity {
                return Err(io::Error::other("root identity changed"));
            }
        }
        let reopened = open(&self.path)?;
        if metadata(&reopened)?.identity != self.identity {
            return Err(io::Error::other("root path or volume changed"));
        }
        Ok(())
    }

    fn walk(&self, relative: &str, directory_end: bool) -> io::Result<(Vec<File>, EntryMetadata)> {
        let relative = RelativePath::parse(relative).map_err(invalid)?;
        self.verify()?;
        let mut current = self.path.clone();
        let mut pins = vec![];
        let parts = relative.components();
        let mut result = None;
        for (index, name) in parts.iter().enumerate() {
            current.push(name);
            if current.as_os_str().encode_wide().count() >= 32760 {
                return Err(invalid("Windows extended path too long"));
            }
            let parent = pins
                .last()
                .unwrap_or_else(|| self.pins.last().expect("root pin"));
            let file = open_relative(parent, name, index + 1 < parts.len() || directory_end)?;
            let meta = metadata(&file)?;
            if meta.identity.volume != self.identity.volume {
                return Err(invalid("volume boundary changed"));
            }
            if !meta.supported() {
                return Err(invalid(format!(
                    "unsupported reparse tag {:08x}",
                    meta.reparse_tag
                )));
            }
            if index + 1 < parts.len() && !meta.directory {
                return Err(invalid("ancestor is not a directory"));
            }
            result = Some(meta);
            pins.push(file);
        }
        Ok((pins, result.ok_or_else(|| invalid("empty relative path"))?))
    }

    pub(crate) fn with_parent<T>(
        &self,
        relative: &str,
        action: impl FnOnce(&File, &str) -> io::Result<T>,
    ) -> io::Result<T> {
        let parsed = RelativePath::parse(relative).map_err(invalid)?;
        self.verify()?;
        let parts = parsed.components();
        let leaf = parts.last().ok_or_else(|| invalid("empty path"))?;
        let held;
        let parent = if parts.len() == 1 {
            self.pins.last().expect("root pin")
        } else {
            held = self.walk(&parts[..parts.len() - 1].join("/"), true)?.0;
            held.last().expect("parent pin")
        };
        action(parent, leaf)
    }

    /// Safely query a relative entry without content I/O. Unknown reparses are refused.
    ///
    /// # Errors
    /// Invalid names, traversal, unexpected volumes, reparses and I/O failures are explicit.
    pub fn inspect(&self, relative: &str) -> io::Result<EntryMetadata> {
        self.walk(relative, false).map(|(_, meta)| meta)
    }

    /// Create a normal directory relative to pinned no-follow parents.
    /// An existing directory is accepted only after the same safety validation; files are never replaced.
    pub fn ensure_directory(&self, relative: &str) -> io::Result<EntryMetadata> {
        self.with_parent(relative, |parent, name| {
            match open_child(parent, name, true, false, 0x0010_0081, true) {
                Ok(file) => metadata(&file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let existing = self.inspect(relative)?;
                    if !existing.directory {
                        return Err(invalid("directory target is a file"));
                    }
                    Ok(existing)
                }
                Err(error) => Err(error),
            }
        })
    }

    /// Enumerate metadata only; unsupported objects and errors remain visible and disable deletion.
    #[must_use]
    pub fn scan(&self) -> ScanReport {
        let mut report = ScanReport {
            root: self.identity,
            health: ScanHealth::Complete,
            entries: vec![],
        };
        if self.verify().is_err() {
            report.health = ScanHealth::RootUnavailable;
            return report;
        }
        let mut pending = vec![String::new()];
        while let Some(relative) = pending.pop() {
            pending.extend(self.scan_directory(&relative, &mut report));
        }
        if self.verify().is_err() {
            report.health = ScanHealth::RootUnavailable;
        }
        report
    }

    fn scan_directory(&self, relative: &str, report: &mut ScanReport) -> Vec<String> {
        let mut subdirectories = vec![];
        let held = if relative.is_empty() {
            None
        } else {
            match self.walk(relative, true) {
                Ok(value) => Some(value),
                Err(error) => {
                    record_error(report, relative, error);
                    return subdirectories;
                }
            }
        };
        let directory = held
            .as_ref()
            .and_then(|(pins, _)| pins.last())
            .unwrap_or_else(|| self.pins.last().expect("root pin"));
        let listing = match directory_names(directory) {
            Ok(value) => value,
            Err(error) => {
                record_error(report, relative, error);
                return subdirectories;
            }
        };
        let mut names = Vec::new();
        for name in listing {
            let child = if relative.is_empty() {
                name.clone()
            } else {
                format!("{relative}/{name}")
            };
            if names
                .iter()
                .any(|previous: &String| NativeNames.equivalent(previous, &name))
            {
                record_error(report, &child, invalid("native name collision"));
            }
            names.push(name);
            match self.walk(&child, false) {
                Ok((_child_pins, meta)) => {
                    let issue = if !meta.directory && meta.links > 1 {
                        Some("multiple hard links: content replacement is unsupported".into())
                    } else {
                        None
                    };
                    if issue.is_some() && report.health == ScanHealth::Complete {
                        report.health = ScanHealth::Incomplete;
                    }
                    let directory = meta.directory;
                    report.entries.push(ScanEntry {
                        path: child.clone(),
                        metadata: Some(meta),
                        issue,
                    });
                    if directory {
                        subdirectories.push(child);
                    }
                }
                Err(error) => record_error(report, &child, error),
            }
        }
        drop(held);
        subdirectories
    }

    pub(crate) fn watch_file(&self) -> io::Result<File> {
        self.verify()?;
        let file = if self.pins.len() > 1 {
            let name = self
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid("root has no name"))?;
            open_relative_mode(&self.pins[self.pins.len() - 2], name, true, true)?
        } else {
            OpenOptions::new()
                .access_mode(0x81)
                .share_mode(3)
                .custom_flags(FLAGS | 0x4000_0000)
                .open(&self.path)?
        };
        let observed = metadata(&file)?;
        if observed.identity != self.identity || observed.reparse_tag != 0 {
            return Err(io::Error::other("watch root identity changed"));
        }
        Ok(file)
    }

    pub(crate) fn root_path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn root_file(&self) -> &File {
        self.pins.last().expect("root pin")
    }
}

fn record_error(report: &mut ScanReport, path: &str, error: io::Error) {
    if report.health != ScanHealth::PermissionDenied {
        report.health = if error.kind() == io::ErrorKind::PermissionDenied {
            ScanHealth::PermissionDenied
        } else {
            ScanHealth::Incomplete
        };
    }
    report.entries.push(ScanEntry {
        path: path.into(),
        metadata: None,
        issue: Some(error.to_string()),
    });
}

#[cfg(test)]
#[path = "native_tests.rs"]
mod tests;
