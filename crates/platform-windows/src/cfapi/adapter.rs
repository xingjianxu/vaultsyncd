//! Production CFAPI lifecycle and metadata adapter. Each connection owns its callback context.
//! Core callers persist intents and authorize generations before invoking namespace mutations.
use super::{
    catalog::{Catalog, Identity},
    prototype::wide,
    raw::*,
};
use crate::files::{
    EntryMetadata, RootHandle,
    native::{metadata, open_child, open_child_shared},
};
use std::{
    ffi::c_void,
    fs::File,
    io,
    os::windows::io::AsRawHandle,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

/// Copyable asynchronous reply address. Valid only while its owning connection is alive.
#[derive(Clone, Copy, Debug)]
pub struct Reply {
    /// Connection key from the actual callback.
    pub connection: i64,
    /// Transfer key from the actual callback.
    pub transfer: i64,
    /// Request key must be echoed unchanged.
    pub request: i64,
    /// Owned CORRELATION_VECTOR copy; no callback-stack pointer escapes.
    pub correlation: Option<[u8; 130]>,
}
impl Reply {
    /// Return a verified range or a negative NTSTATUS. Buffers are borrowed only during CfExecute.
    pub fn transfer(
        self,
        offset: i64,
        length: i64,
        bytes: Option<&[u8]>,
        status: i32,
    ) -> io::Result<()> {
        if length <= 0
            || offset < 0
            || (status >= 0 && bytes.is_none_or(|b| b.len() as i64 != length))
        {
            return Err(io::Error::other("invalid transfer reply"));
        }
        let info = CfOperationInfo {
            struct_size: size_of::<CfOperationInfo>() as u32,
            operation_type: CF_OPERATION_TYPE_TRANSFER_DATA,
            connection_key: CfConnectionKey {
                internal: self.connection,
            },
            transfer_key: self.transfer,
            correlation_vector: self
                .correlation
                .as_ref()
                .map_or(std::ptr::null(), |v| v.as_ptr().cast()),
            sync_status: std::ptr::null(),
            request_key: self.request,
        };
        let mut parameters = CfOperationParametersTransferData {
            param_size: CF_OPERATION_PARAMETERS_SIZE,
            padding: 0,
            flags: 0,
            completion_status: status,
            buffer: bytes.map_or(std::ptr::null(), |b| b.as_ptr().cast()),
            offset,
            length,
        };
        hr(unsafe { CfExecute(&info, std::ptr::from_mut(&mut parameters).cast()) })
    }
}
/// Validated file demand, independent of callback stack and path spelling.
#[derive(Clone, Copy, Debug)]
pub struct ReadDemand {
    /// Binding, root, entry and fixed version validated against the connection catalog.
    pub identity: Identity,
    /// Logical file length observed by CFAPI.
    pub file_size: u64,
    /// Required range start.
    pub offset: u64,
    /// Required range length.
    pub length: u64,
    /// Actual callback routing keys.
    pub reply: Reply,
}
/// Nonblocking callback port. Implementations enqueue bounded work and own asynchronous completion.
pub trait Callbacks: Send + Sync {
    /// Enqueue demand, or return an explicit negative NTSTATUS (offline, resources, unsupported).
    fn fetch(&self, demand: ReadDemand) -> Result<(), i32>;
    /// Cancel only the identified transfer. No network or database waits on the callback thread.
    fn cancel(&self, reply: Reply);
    /// Signal connection shutdown; implementations stop admission and cancel queued transfers.
    fn disconnect(&self) {}
}
/// Explicit unsupported implementation for metadata-only consumers.
pub struct MetadataOnly;
impl Callbacks for MetadataOnly {
    fn fetch(&self, _: ReadDemand) -> Result<(), i32> {
        Err(STATUS_NOT_SUPPORTED)
    }
    fn cancel(&self, _: Reply) {}
}
struct Context {
    catalog: Arc<RwLock<Catalog>>,
    callbacks: Arc<dyn Callbacks>,
}
fn hr(code: i32) -> io::Result<()> {
    if code < 0 {
        Err(io::Error::other(format!(
            "CFAPI HRESULT 0x{:08x}",
            code as u32
        )))
    } else {
        Ok(())
    }
}
unsafe fn route(
    info: *const CfCallbackInfo,
    params: *const CfCallbackParametersFetchData,
    cancel: bool,
) {
    if info.is_null() || params.is_null() {
        return;
    }
    let i = unsafe { &*info };
    let reply = Reply {
        connection: i.connection_key.internal,
        transfer: i.transfer_key,
        request: i.request_key,
        correlation: if i.correlation_vector.is_null() {
            None
        } else {
            Some(unsafe { std::ptr::read_unaligned(i.correlation_vector.cast::<[u8; 130]>()) })
        },
    };
    if i.callback_context.is_null() {
        return;
    }
    let context = unsafe { &*i.callback_context.cast::<Context>() };
    if cancel {
        context.callbacks.cancel(reply);
        return;
    }
    let p = unsafe { &*params };
    let run = || -> Result<(), i32> {
        if i.file_identity.is_null()
            || i.file_identity_length as usize != Identity::LEN
            || i.sync_root_identity.is_null()
            || i.sync_root_identity_length != 32
        {
            return Err(STATUS_CLOUD_FILE_INVALID_REQUEST);
        }
        let bytes = unsafe { std::slice::from_raw_parts(i.file_identity.cast(), Identity::LEN) };
        let root = unsafe { std::slice::from_raw_parts(i.sync_root_identity.cast::<u8>(), 32) };
        let cat = context
            .catalog
            .read()
            .map_err(|_| STATUS_CLOUD_FILE_INVALID_REQUEST)?;
        if root != cat.root_blob() {
            return Err(STATUS_CLOUD_FILE_INVALID_REQUEST);
        }
        let identity = cat
            .resolve(bytes)
            .map_err(|_| STATUS_CLOUD_FILE_INVALID_REQUEST)?;
        if i.file_size < 0
            || p.required_file_offset < 0
            || p.required_length <= 0
            || p.required_file_offset
                .checked_add(p.required_length)
                .is_none_or(|n| n > i.file_size)
        {
            return Err(STATUS_CLOUD_FILE_INVALID_REQUEST);
        }
        let demand = ReadDemand {
            identity,
            file_size: i.file_size as u64,
            offset: p.required_file_offset as u64,
            length: p.required_length as u64,
            reply,
        };
        drop(cat);
        context.callbacks.fetch(demand)
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
        .unwrap_or(Err(STATUS_CLOUD_FILE_INVALID_REQUEST));
    if let Err(status) = result {
        let _ = reply.transfer(
            p.required_file_offset,
            p.required_length,
            None,
            status.min(-1),
        );
    }
}
unsafe extern "system" fn fetch(i: *const CfCallbackInfo, p: *const CfCallbackParametersFetchData) {
    unsafe { route(i, p, false) }
}
unsafe extern "system" fn cancel(
    i: *const CfCallbackInfo,
    p: *const CfCallbackParametersFetchData,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        route(i, p, true)
    }));
}
static TABLE: [CfCallbackRegistration; 3] = [
    CfCallbackRegistration {
        callback_type: CF_CALLBACK_TYPE_FETCH_DATA,
        callback: Some(fetch),
    },
    CfCallbackRegistration {
        callback_type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
        callback: Some(cancel),
    },
    CfCallbackRegistration {
        callback_type: CF_CALLBACK_TYPE_NONE,
        callback: None,
    },
];

/// Live connection. Drop disconnects; registration and files survive process exit.
pub struct Adapter {
    root: RootHandle,
    path: PathBuf,
    key: Option<CfConnectionKey>,
    context: Box<Context>,
}
impl Adapter {
    /// Register (or verify an existing registration), then connect after loading durable identities.
    pub fn connect(
        path: &Path,
        catalog: Catalog,
        callbacks: Arc<dyn Callbacks>,
    ) -> io::Result<Self> {
        let root = RootHandle::open(path)?;
        register(path, &catalog)?;
        let context = Box::new(Context {
            catalog: Arc::new(RwLock::new(catalog)),
            callbacks,
        });
        let mut key = CfConnectionKey::default();
        hr(unsafe {
            CfConnectSyncRoot(
                wide(path).as_ptr(),
                TABLE.as_ptr(),
                std::ptr::from_ref(&*context).cast_mut().cast(),
                CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
                &mut key,
            )
        })?;
        Ok(Self {
            root,
            path: path.to_path_buf(),
            key: Some(key),
            context,
        })
    }
    /// Catalog shared with the application; publish only after persistent/platform success.
    pub fn catalog(&self) -> Arc<RwLock<Catalog>> {
        self.context.catalog.clone()
    }
    /// Metadata-only scan; errors disable missing-entry inference (S065).
    pub fn scan(&self) -> crate::files::ScanReport {
        self.root.scan()
    }
    /// Safe metadata observation.
    pub fn inspect(&self, path: &str) -> io::Result<EntryMetadata> {
        self.root.inspect(path)
    }
    /// Create a new placeholder without replacing any local object. Intermediate failures stay visible.
    /// Uses a root-relative create then CFAPI conversion, so path races cannot escape the pinned parent.
    pub fn create(
        &self,
        path: &str,
        id: Identity,
        size: u64,
        directory: bool,
    ) -> io::Result<EntryMetadata> {
        self.check_identity(id)?;
        self.root.with_parent(path, |parent, name| {
            let file = open_child_shared(
                parent,
                name,
                directory,
                false,
                0x0011_0182 | u32::from(directory),
                true,
                0,
            )?;
            let bytes = id.encode();
            let mut usn = 0;
            hr(unsafe {
                CfConvertToPlaceholder(
                    file.as_raw_handle(),
                    bytes.as_ptr().cast(),
                    bytes.len() as u32,
                    1,
                    &mut usn,
                    std::ptr::null_mut(),
                )
            })?;
            update(&file, id, size, directory)?;
            metadata(&file)
        })
    }
    fn check_identity(&self, id: Identity) -> io::Result<()> {
        let cat = self
            .context
            .catalog
            .read()
            .map_err(|_| io::Error::other("catalog poisoned"))?;
        if id.binding != cat.binding || id.root != cat.root {
            return Err(io::Error::other("foreign identity"));
        }
        Ok(())
    }
    /// Convert verified complete regular contents without discarding a byte.
    /// The complete digest is checked under a share-none no-follow handle before marking in-sync.
    pub fn adopt_complete(
        &self,
        path: &str,
        expected: &EntryMetadata,
        id: Identity,
        digest: [u8; 32],
        size: u64,
    ) -> io::Result<EntryMetadata> {
        use std::os::windows::fs::FileExt;
        self.check_identity(id)?;
        self.root.with_parent(path, |parent, name| {
            let _ = (parent, name);
            let (protected, _) =
                crate::files::controlled::ProtectedFile::open_under(&self.root, path, true)?;
            let lease = protected.lease()?;
            let file = lease.file();
            let meta = metadata(file)?;
            check_expected(&meta, expected)?;
            if meta.size != size || meta.directory || meta.links != 1 {
                return Err(io::Error::other("LOCAL_CHANGED"));
            }
            if let Some(cloud) = &meta.cloud {
                let prior = Identity::decode(&cloud.identity)
                    .map_err(|e| io::Error::other(format!("{e:?}")))?;
                if prior.binding != id.binding
                    || prior.root != id.root
                    || prior.entry != id.entry
                    || cloud.on_disk < size
                {
                    return Err(io::Error::other(
                        "different or incomplete placeholder identity",
                    ));
                }
            }
            let mut h = vaultsyncd_transfer::hash::ContentHasher::new();
            let mut at = 0;
            while at < size {
                let mut b = vec![0; (size - at).min(1024 * 1024) as usize];
                let n = file.seek_read(&mut b, at)?;
                if n == 0 {
                    return Err(io::Error::other("short adoption read"));
                }
                h.update(&b[..n]);
                at += n as u64;
            }
            if h.finish() != digest {
                return Err(io::Error::other("LOCAL_CHANGED"));
            }
            let bytes = id.encode();
            let mut usn = 0;
            if meta.cloud.is_none() {
                hr(unsafe {
                    CfConvertToPlaceholder(
                        file.as_raw_handle(),
                        bytes.as_ptr().cast(),
                        bytes.len() as u32,
                        1,
                        &mut usn,
                        std::ptr::null_mut(),
                    )
                })?;
            } else {
                hr(unsafe {
                    CfUpdatePlaceholder(
                        file.as_raw_handle(),
                        std::ptr::null(),
                        bytes.as_ptr().cast(),
                        bytes.len() as u32,
                        std::ptr::null(),
                        0,
                        2,
                        &mut usn,
                        std::ptr::null_mut(),
                    )
                })?;
            }
            metadata(file)
        })
    }

    /// Update only an empty placeholder under the expected physical and logical identity.
    /// Hydrated/modified content is preserved for controlled application in S068.
    pub fn update(
        &self,
        path: &str,
        expected: &EntryMetadata,
        id: Identity,
        size: u64,
    ) -> io::Result<EntryMetadata> {
        self.check_identity(id)?;
        self.root.with_parent(path, |parent, name| {
            let file = open_child_shared(
                parent,
                name,
                expected.directory,
                false,
                0x0010_0182 | u32::from(expected.directory),
                false,
                0,
            )?;
            let current = metadata(&file)?;
            check_expected(&current, expected)?;
            let cloud = current
                .cloud
                .as_ref()
                .ok_or_else(|| io::Error::other("not placeholder"))?;
            let prior = Identity::decode(&cloud.identity)
                .map_err(|e| io::Error::other(format!("identity {e:?}")))?;
            if prior.binding != id.binding
                || prior.root != id.root
                || prior.entry != id.entry
                || cloud.on_disk != 0
                || cloud.modified != 0
                || cloud.pin_state == 1
            {
                return Err(io::Error::other("LOCAL_CHANGED or retained content"));
            }
            self.context
                .catalog
                .read()
                .map_err(|_| io::Error::other("catalog poisoned"))?
                .resolve(&cloud.identity)
                .map_err(|e| io::Error::other(format!("identity {e:?}")))?;
            if !current.directory {
                hr(unsafe { CfSetInSyncState(file.as_raw_handle(), 1, 0, std::ptr::null_mut()) })?;
            }
            update(&file, id, size, current.directory)?;
            metadata(&file)
        })
    }
    /// Move a placeholder by handle, refusing overwrite. No content stream is opened or transferred.
    pub fn move_entry(
        &self,
        from: &str,
        to: &str,
        expected: &EntryMetadata,
    ) -> io::Result<EntryMetadata> {
        self.root.with_parent(from, |parent, name| {
            let file = open_child(parent, name, expected.directory, false, 0x0011_0080, false)?;
            let current = metadata(&file)?;
            check_expected(&current, expected)?;
            let blob = &current
                .cloud
                .as_ref()
                .ok_or_else(|| io::Error::other("not placeholder"))?
                .identity;
            self.context
                .catalog
                .read()
                .map_err(|_| io::Error::other("catalog poisoned"))?
                .resolve(blob)
                .map_err(|e| io::Error::other(format!("identity {e:?}")))?;
            self.root
                .with_parent(to, |target, name| rename(&file, target, name))?;
            metadata(&file)
        })
    }
    /// Disconnect explicitly so errors can be reported before application shutdown.
    pub fn disconnect(&mut self) -> io::Result<()> {
        if let Some(key) = self.key {
            self.context.callbacks.disconnect();
            hr(unsafe { CfDisconnectSyncRoot(key) })?;
            self.key = None;
        }
        Ok(())
    }
    /// Refuse while placeholders or incomplete scan evidence remain; Windows would delete partial files.
    /// After explicit materialization/conversion, disconnect and unregister without deleting ordinary files.
    pub fn unregister(&mut self) -> io::Result<()> {
        ensure_no_placeholders(&self.path)?;
        self.disconnect()?;
        self.root.verify()?;
        hr(unsafe { CfUnregisterSyncRoot(wide(&self.path).as_ptr()) })
    }
}
impl Drop for Adapter {
    fn drop(&mut self) {
        if self.disconnect().is_err() {
            // Keep callback storage alive if Windows still owns the connection.
            let catalog = self.context.catalog.clone();
            let context = std::mem::replace(
                &mut self.context,
                Box::new(Context {
                    catalog,
                    callbacks: Arc::new(MetadataOnly),
                }),
            );
            let _ = Box::leak(context);
        }
    }
}
fn check_expected(current: &EntryMetadata, expected: &EntryMetadata) -> io::Result<()> {
    if current != expected {
        Err(io::Error::other("LOCAL_CHANGED: metadata/identity differs"))
    } else {
        Ok(())
    }
}
fn update(file: &File, id: Identity, size: u64, directory: bool) -> io::Result<()> {
    let bytes = id.encode();
    let mut usn = 0;
    let meta = CfFsMetadata {
        basic_info: FileBasicInfo {
            creation_time: 0,
            last_access_time: 0,
            last_write_time: 0,
            change_time: 0,
            file_attributes: if directory {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_ARCHIVE
            },
        },
        file_size: i64::try_from(size).map_err(|_| io::Error::other("file too large"))?,
    };
    hr(unsafe {
        CfUpdatePlaceholder(
            file.as_raw_handle(),
            &meta,
            bytes.as_ptr().cast(),
            bytes.len() as u32,
            std::ptr::null(),
            0,
            if directory { 0x10 | 2 } else { 4 | 2 },
            &mut usn,
            std::ptr::null_mut(),
        )
    })
}
#[link(name = "cldapi")]
unsafe extern "system" {
    fn CfConvertToPlaceholder(
        file: Handle,
        identity: *const c_void,
        length: u32,
        flags: u32,
        usn: *mut i64,
        overlapped: *mut c_void,
    ) -> i32;
    fn CfUpdatePlaceholder(
        file: Handle,
        metadata: *const CfFsMetadata,
        identity: *const c_void,
        length: u32,
        ranges: *const c_void,
        count: u32,
        flags: u32,
        usn: *mut i64,
        overlapped: *mut c_void,
    ) -> i32;
}
#[repr(C)]
#[derive(Default)]
struct IoStatus {
    status: usize,
    information: usize,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtSetInformationFile(
        handle: Handle,
        status: *mut IoStatus,
        info: *const c_void,
        size: u32,
        class: i32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}
fn rename(file: &File, parent: &File, name: &str) -> io::Result<()> {
    let name: Vec<u16> = name.encode_utf16().collect();
    // FILE_RENAME_INFO: DWORD flags, HANDLE RootDirectory at 8, DWORD FileNameLength at 16, name at 20.
    let len = 20 + name.len() * 2;
    let mut buffer = vec![0u64; len.div_ceil(8)];
    let p = buffer.as_mut_ptr().cast::<u8>();
    unsafe {
        std::ptr::write_unaligned(p.add(8).cast::<Handle>(), parent.as_raw_handle());
        std::ptr::write_unaligned(p.add(16).cast::<u32>(), (name.len() * 2) as u32);
        std::ptr::copy_nonoverlapping(name.as_ptr().cast::<u8>(), p.add(20), name.len() * 2);
        let status = NtSetInformationFile(
            file.as_raw_handle(),
            &mut IoStatus::default(),
            p.cast(),
            len as u32,
            10,
        );
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                RtlNtStatusToDosError(status) as i32
            ));
        }
    }
    Ok(())
}
pub(crate) fn register(path: &Path, catalog: &Catalog) -> io::Result<()> {
    let name: Vec<u16> = "vaultsyncd\0".encode_utf16().collect();
    let version: Vec<u16> = "0.1.0\0".encode_utf16().collect();
    let identity = catalog.root_blob();
    let registration = CfSyncRegistration {
        struct_size: size_of::<CfSyncRegistration>() as u32,
        provider_name: name.as_ptr(),
        provider_version: version.as_ptr(),
        sync_root_identity: identity.as_ptr().cast(),
        sync_root_identity_length: 32,
        file_identity: std::ptr::null(),
        file_identity_length: 0,
        provider_id: Guid {
            data1: 0x7a5c1f01,
            data2: 0x5eed,
            data3: 0x4a31,
            data4: [0x9b, 0x2e, 0, 0x11, 0x22, 0x33, 0x44, 0x55],
        },
    };
    let policies = CfSyncPolicies {
        struct_size: size_of::<CfSyncPolicies>() as u32,
        hydration: CfHydrationPolicy {
            primary: CF_HYDRATION_POLICY_PARTIAL,
            modifier: 0,
        },
        population: CfPopulationPolicy {
            primary: CF_POPULATION_POLICY_PARTIAL,
            modifier: 0,
        },
        in_sync: 0,
        hard_link: 0,
        placeholder_management: 0,
    };
    // Verify existing registration identity before accepting ERROR_ALREADY_EXISTS; never steal a root.
    let mut buf = vec![0u64; 1024];
    let mut returned = 0;
    let existing = unsafe {
        CfGetSyncRootInfoByPath(
            wide(path).as_ptr(),
            1,
            buf.as_mut_ptr().cast(),
            (buf.len() * 8) as u32,
            &mut returned,
        )
    };
    if existing >= 0 {
        let b = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len() * 8) };
        // CF_SYNC_ROOT_STANDARD_INFO: 28-byte prefix + 2 WCHAR[256], length at 1052, blob at 1056.
        if returned < 1088 || b[1052..1056] != 32u32.to_le_bytes() || b[1056..1088] != identity {
            return Err(io::Error::other("foreign root registration"));
        }
        return Ok(());
    }
    if ![
        0x80070186u32 as i32,
        0x80070178u32 as i32,
        0x80070002u32 as i32,
    ]
    .contains(&existing)
    {
        return hr(existing);
    }
    hr(unsafe { CfRegisterSyncRoot(wide(path).as_ptr(), &registration, &policies, 0) })
}

#[cfg(test)]
mod native_tests;

/// CfUnregisterSyncRoot deletes partial placeholders. Require explicit prior materialization/conversion.
pub(crate) fn ensure_no_placeholders(path: &Path) -> io::Result<()> {
    let root = RootHandle::open(path)?;
    let report = root.scan();
    if !report.permits_missing_comparison()
        || report
            .entries
            .iter()
            .any(|e| e.metadata.as_ref().is_none_or(|m| m.placeholder()))
    {
        return Err(io::Error::other(
            "UNREGISTER_REFUSED: materialize and explicitly revert placeholders first; Windows unregister would delete partial content",
        ));
    }
    Ok(())
}
