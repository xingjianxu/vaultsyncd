//! Explorer integration via Windows Runtime, separate from the CFAPI connection lifecycle.
use std::{io, path::Path};
use vaultsyncd_domain::ids::BindingId;
use windows::{
    Storage::{
        Provider::{
            StorageProviderHydrationPolicy, StorageProviderPopulationPolicy,
            StorageProviderSyncRootInfo, StorageProviderSyncRootManager,
        },
        StorageFolder,
    },
    core::HSTRING,
};
fn map(e: windows::core::Error) -> io::Error {
    io::Error::other(e.to_string())
}
/// Stable Explorer registration ID for a local binding. Does not encode server addresses or credentials.
pub fn registration_id(binding: BindingId) -> io::Result<String> {
    use std::ffi::c_void;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
        fn GetTokenInformation(
            token: *mut c_void,
            class: u32,
            buffer: *mut c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *const c_void, text: *mut *mut u16) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(-1isize as *mut c_void, 8, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = [0u64; 128];
    let mut returned = 0;
    let result = unsafe {
        GetTokenInformation(
            token,
            1,
            buffer.as_mut_ptr().cast(),
            size_of_val(&buffer) as u32,
            &mut returned,
        )
    };
    unsafe {
        CloseHandle(token);
    }
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    let sid = unsafe { std::ptr::read(buffer.as_ptr().cast::<*const c_void>()) };
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let sid = unsafe { crate::cfapi::prototype::read_wide(text) };
    unsafe {
        LocalFree(text.cast());
    }
    Ok(format!("vaultsyncd!{sid}!{binding}"))
}
/// Register an existing local NTFS root with Explorer. Call from a worker, never a UI callback.
/// The calling thread must initialize Windows Runtime (MTA or STA) first.
pub fn register(path: &Path, catalog: &crate::cfapi::catalog::Catalog) -> io::Result<()> {
    let _pin = crate::files::RootHandle::open(path)?;
    crate::cfapi::adapter::register(path, catalog)?;
    let binding = catalog.binding;
    let path = path
        .to_str()
        .ok_or_else(|| io::Error::other("non-Unicode root"))?;
    let folder = StorageFolder::GetFolderFromPathAsync(&HSTRING::from(path))
        .map_err(map)?
        .get()
        .map_err(map)?;
    let info = StorageProviderSyncRootInfo::new().map_err(map)?;
    info.SetVersion(&HSTRING::from("0.1.0")).map_err(map)?;
    info.SetId(&HSTRING::from(registration_id(binding)?))
        .map_err(map)?;
    info.SetPath(&folder).map_err(map)?;
    info.SetDisplayNameResource(&HSTRING::from("vaultsyncd"))
        .map_err(map)?;
    info.SetIconResource(&HSTRING::from("%SystemRoot%\\system32\\imageres.dll,-1040"))
        .map_err(map)?;
    info.SetHydrationPolicy(StorageProviderHydrationPolicy::Partial)
        .map_err(map)?;
    info.SetPopulationPolicy(StorageProviderPopulationPolicy::Full)
        .map_err(map)?;
    let writer = windows::Storage::Streams::DataWriter::new().map_err(map)?;
    writer.WriteBytes(&catalog.root_blob()).map_err(map)?;
    info.SetContext(&writer.DetachBuffer().map_err(map)?)
        .map_err(map)?;
    info.SetProviderId(windows::core::GUID::from_u128(
        0x7a5c1f01_5eed_4a31_9b2e_001122334455,
    ))
    .map_err(map)?;
    StorageProviderSyncRootManager::Register(&info).map_err(map)
}
/// Remove Explorer registration only. Existing files are never deleted by this function.
pub fn unregister(binding: BindingId) -> io::Result<()> {
    for root in StorageProviderSyncRootManager::GetCurrentSyncRoots().map_err(map)? {
        if root.Id().map_err(map)? == registration_id(binding)? {
            let path = root.Path().map_err(map)?.Path().map_err(map)?.to_string();
            crate::cfapi::adapter::ensure_no_placeholders(Path::new(&path))?;
        }
    }

    StorageProviderSyncRootManager::Unregister(&HSTRING::from(registration_id(binding)?))
        .map_err(map)
}
/// Query Explorer's persisted registration for restart verification.
pub fn registered(binding: BindingId) -> io::Result<bool> {
    let roots = StorageProviderSyncRootManager::GetCurrentSyncRoots().map_err(map)?;
    for root in roots {
        if root.Id().map_err(map)? == registration_id(binding)? {
            return Ok(true);
        }
    }
    Ok(false)
}
