//! Explorer integration via Windows Runtime, separate from the CFAPI connection lifecycle.
pub mod toast;
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
type ShellWork = Box<dyn FnOnce() + Send>;
// Keep the apartment alive for the process lifetime, like windows-rs' activation
// factory cache. Tearing down the last MTA between calls invalidates cached factories.
// No WinRT object or apartment guard crosses an async caller's await.
fn with_runtime<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    use std::sync::{OnceLock, mpsc};
    static WORKER: OnceLock<io::Result<mpsc::Sender<ShellWork>>> = OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<ShellWork>();
        let (ready, started) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("vaultsyncd-explorer".into())
            .spawn(move || {
                use windows::Win32::System::WinRT::{
                    RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
                };
                let initialized = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(map);
                if let Err(error) = initialized {
                    let _ = ready.send(Err(error));
                    return;
                }
                struct Apartment;
                impl Drop for Apartment {
                    fn drop(&mut self) {
                        unsafe { RoUninitialize() };
                    }
                }
                let _apartment = Apartment;
                if ready.send(Ok(())).is_err() {
                    return;
                }
                while let Ok(action) = receiver.recv() {
                    action();
                }
            })?;
        started
            .recv()
            .map_err(|_| io::Error::other("Explorer worker initialization failed"))??;
        Ok(sender)
    });
    let worker = worker
        .as_ref()
        .map_err(|e| io::Error::new(e.kind(), e.to_string()))?;
    let (result, completed) = mpsc::sync_channel(1);
    worker
        .send(Box::new(move || {
            let _ = result.send(work());
        }))
        .map_err(|_| io::Error::other("Explorer registration worker stopped"))?;
    completed
        .recv()
        .map_err(|_| io::Error::other("Explorer registration worker failed"))?
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
/// Owns its Windows Runtime thread; callers do not need to initialize COM/WinRT.
pub fn register(path: &Path, catalog: &crate::cfapi::catalog::Catalog) -> io::Result<()> {
    let path = path.to_owned();
    // Shell registration needs only the durable root identity, not the file catalog.
    let catalog = crate::cfapi::catalog::Catalog::new(catalog.binding, catalog.root);
    with_runtime(move || register_in_runtime(&path, &catalog))
}
fn register_in_runtime(path: &Path, catalog: &crate::cfapi::catalog::Catalog) -> io::Result<()> {
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
    // Names are maintained by snapshots/events, with no FETCH_PLACEHOLDERS callback.
    // WinRT registration must preserve the CFAPI adapter's ALWAYS_FULL policy.
    info.SetPopulationPolicy(StorageProviderPopulationPolicy::AlwaysFull)
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
    with_runtime(move || unregister_in_runtime(binding))
}
fn unregister_in_runtime(binding: BindingId) -> io::Result<()> {
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
    with_runtime(move || registered_in_runtime(binding))
}
fn registered_in_runtime(binding: BindingId) -> io::Result<bool> {
    let roots = StorageProviderSyncRootManager::GetCurrentSyncRoots().map_err(map)?;
    for root in roots {
        if root.Id().map_err(map)? == registration_id(binding)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Open the Windows single-folder picker owned by the desktop window.
/// Must run on a blocking worker; cancellation returns `None` and never changes files.
pub fn select_directory(owner: isize) -> io::Result<Option<String>> {
    use windows::Win32::{
        Foundation::{ERROR_CANCELLED, HWND},
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize,
        },
        UI::Shell::{
            FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS,
            FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
        },
    };
    // A dedicated STA avoids inheriting COM state from a shared runtime worker.
    std::thread::spawn(move || {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                .ok()
                .map_err(map)?;
        }
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                unsafe {
                    CoUninitialize();
                }
            }
        }
        let _apartment = Apartment;
        unsafe {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(map)?;
            dialog
                .SetOptions(
                    dialog.GetOptions().map_err(map)?
                        | FOS_PICKFOLDERS
                        | FOS_FORCEFILESYSTEM
                        | FOS_PATHMUSTEXIST
                        | FOS_NOCHANGEDIR,
                )
                .map_err(map)?;
            dialog
                .SetTitle(&HSTRING::from("选择本地同步目录"))
                .map_err(map)?;
            dialog
                .SetOkButtonLabel(&HSTRING::from("选择目录"))
                .map_err(map)?;
            if let Err(error) = dialog.Show(Some(HWND(owner as *mut std::ffi::c_void))) {
                if error.code() == windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) {
                    return Ok(None);
                }
                return Err(map(error));
            }
            let path = dialog
                .GetResult()
                .map_err(map)?
                .GetDisplayName(SIGDN_FILESYSPATH)
                .map_err(map)?;
            let selected = path.to_string().map_err(io::Error::other);
            CoTaskMemFree(Some(path.0.cast()));
            selected.map(Some)
        }
    })
    .join()
    .map_err(|_| io::Error::other("directory picker thread failed"))?
}
