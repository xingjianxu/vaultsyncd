//! Native desktop Toasts under one fixed AppUserModelID. No caller-selected executable or activation.
use std::{io, path::PathBuf};
use windows::{
    Data::Xml::Dom::XmlDocument,
    UI::Notifications::{NotificationSetting, ToastNotification, ToastNotificationManager},
    Win32::{
        Storage::EnhancedStorage::PKEY_AppUserModel_ID,
        System::{
            Com::StructuredStorage::{PROPVARIANT, PVCHF_DEFAULT, PropVariantChangeType},
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READWRITE},
            Variant::VT_LPWSTR,
            WinRT::{RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize},
        },
        UI::Shell::{
            IShellLinkW, PropertiesSystem::IPropertyStore, SetCurrentProcessExplicitAppUserModelID,
            ShellLink,
        },
    },
    core::{BSTR, HSTRING, Interface},
};
/// Identity shared by the GUI and its own Start Menu shortcut.
pub const APP_ID: &str = "org.vaultsyncd.desktop";
fn win(error: windows::core::Error) -> io::Error {
    io::Error::other(error.to_string())
}
fn apartment<T: Send + 'static>(
    action: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    std::thread::spawn(move || {
        unsafe {
            RoInitialize(RO_INIT_SINGLETHREADED).map_err(win)?;
        }
        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe {
                    RoUninitialize();
                }
            }
        }
        let _cleanup = Cleanup;
        action()
    })
    .join()
    .map_err(|_| io::Error::other("notification thread failed"))?
}
fn shortcut() -> io::Result<()> {
    let base =
        std::env::var_os("APPDATA").ok_or_else(|| io::Error::other("missing user app data"))?;
    let directory = PathBuf::from(base).join("Microsoft/Windows/Start Menu/Programs");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("vaultsyncd.lnk");
    let executable = std::env::current_exe()?;
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_ID)).map_err(win)?;
        let link: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(win)?;
        let persist: IPersistFile = link.cast().map_err(win)?;
        let properties: IPropertyStore = link.cast().map_err(win)?;
        if path.exists() {
            if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err(io::Error::other("shortcut is a reparse point"));
            }
            persist
                .Load(
                    &HSTRING::from(path.to_string_lossy().as_ref()),
                    STGM_READWRITE,
                )
                .map_err(win)?;
            let identity = properties.GetValue(&PKEY_AppUserModel_ID).map_err(win)?;
            if BSTR::try_from(&identity).map_err(win)? != APP_ID {
                return Err(io::Error::other("shortcut belongs to another application"));
            }
        }
        link.SetPath(&HSTRING::from(executable.to_string_lossy().as_ref()))
            .map_err(win)?;
        link.SetDescription(&HSTRING::from("vaultsyncd"))
            .map_err(win)?;
        let source = PROPVARIANT::from(APP_ID);
        let mut identity = PROPVARIANT::default();
        PropVariantChangeType(&mut identity, &source, PVCHF_DEFAULT, VT_LPWSTR).map_err(win)?;
        properties
            .SetValue(&PKEY_AppUserModel_ID, &identity)
            .map_err(win)?;
        properties.Commit().map_err(win)?;
        persist
            .Save(&HSTRING::from(path.to_string_lossy().as_ref()), true)
            .map_err(win)?;
    }
    Ok(())
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
/// Send a native notification; false means Windows policy disabled delivery, never successful display.
pub fn show(tag: &str, title: &str, body: &str) -> io::Result<bool> {
    let (tag, title, body) = (tag.to_owned(), title.to_owned(), body.to_owned());
    apartment(move || {
        shortcut().map_err(|e| io::Error::other(format!("shortcut registration: {e}")))?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))
            .map_err(|e| io::Error::other(format!("create notifier: {e}")))?;
        // An unpackaged desktop app may have no per-app settings row until its first Show.
        // Only this specific absent-setting HRESULT permits the first submission; other errors
        // remain explicit failures, and a known disabled setting never calls Show.
        match notifier.Setting() {
            Ok(setting) if setting != NotificationSetting::Enabled => return Ok(false),
            Ok(_) => {}
            Err(error) if error.code().0 == 0x80070490u32 as i32 => {}
            Err(error) => return Err(io::Error::other(format!("notification setting: {error}"))),
        }
        let document = XmlDocument::new().map_err(win)?;
        document.LoadXml(&HSTRING::from(format!("<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",escape(&title),escape(&body)))).map_err(win)?;
        let toast = ToastNotification::CreateToastNotification(&document).map_err(win)?;
        toast.SetTag(&HSTRING::from(tag)).map_err(win)?;
        toast.SetGroup(&HSTRING::from("vaultsyncd")).map_err(win)?;
        notifier
            .Show(&toast)
            .map_err(|e| io::Error::other(format!("show notification: {e}")))?;
        Ok(true)
    })
}
/// Read only this application's actual Windows notification-center tags for native diagnostics.
pub fn history_tags() -> io::Result<Vec<String>> {
    apartment(|| {
        let history = ToastNotificationManager::History()
            .map_err(win)?
            .GetHistoryWithId(&HSTRING::from(APP_ID))
            .map_err(win)?;
        let mut tags = Vec::new();
        for i in 0..history.Size().map_err(win)? {
            tags.push(
                history
                    .GetAt(i)
                    .map_err(win)?
                    .Tag()
                    .map_err(win)?
                    .to_string(),
            );
        }
        Ok(tags)
    })
}
/// Clear this app's Windows notification center only. It cannot acknowledge or delete an agent issue.
pub fn clear_history() -> io::Result<()> {
    apartment(|| {
        ToastNotificationManager::History()
            .map_err(win)?
            .ClearWithId(&HSTRING::from(APP_ID))
            .map_err(win)
    })
}
