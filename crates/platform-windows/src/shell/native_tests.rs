use super::*;
use crate::{
    cfapi::{
        adapter::{Adapter, MetadataOnly},
        catalog::{Catalog, Identity},
    },
    files::VaultHandle,
};
use std::{path::PathBuf, sync::Arc};
use vaultsyncd_domain::ids::{EntryId, VaultId, VersionId};

pub(super) fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "vaultsyncd-S222-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

// Exercise the actual Shell parser and Windows ICO decoder, not just ICO magic bytes.
pub(super) fn decode_icon(resource: &str) -> PathBuf {
    #[link(name = "shlwapi")]
    unsafe extern "system" {
        fn PathParseIconLocationW(path: *mut u16) -> i32;
    }
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ExtractIconExW(
            file: *const u16,
            index: i32,
            large: *mut *mut std::ffi::c_void,
            small: *mut *mut std::ffi::c_void,
            count: u32,
        ) -> u32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn DestroyIcon(icon: *mut std::ffi::c_void) -> i32;
    }
    let mut text: Vec<u16> = resource.encode_utf16().chain(Some(0)).collect();
    assert_eq!(unsafe { PathParseIconLocationW(text.as_mut_ptr()) }, 0);
    let path = PathBuf::from(unsafe { crate::cfapi::prototype::read_wide(text.as_ptr()) });
    assert!(path.is_absolute(), "{resource}");
    let mut icon = std::ptr::null_mut();
    assert_eq!(
        unsafe { ExtractIconExW(text.as_ptr(), 0, &mut icon, std::ptr::null_mut(), 1) },
        1,
        "failed to decode {resource}"
    );
    assert!(!icon.is_null());
    assert_ne!(unsafe { DestroyIcon(icon) }, 0);
    path
}

#[derive(Debug, PartialEq, Eq)]
struct Registration {
    id: String,
    path: String,
    name: String,
    icon: String,
    context: Vec<u8>,
}
fn registration(binding: BindingId) -> Registration {
    with_runtime(move || {
        let root = StorageProviderSyncRootManager::GetSyncRootInformationForId(&HSTRING::from(
            registration_id(binding)?,
        ))
        .map_err(map)?;
        let reader =
            windows::Storage::Streams::DataReader::FromBuffer(&root.Context().map_err(map)?)
                .map_err(map)?;
        let mut context = vec![0; reader.UnconsumedBufferLength().map_err(map)? as usize];
        reader.ReadBytes(&mut context).map_err(map)?;
        assert_eq!(
            root.PopulationPolicy().map_err(map)?,
            StorageProviderPopulationPolicy::AlwaysFull
        );
        assert_eq!(
            root.HydrationPolicy().map_err(map)?,
            StorageProviderHydrationPolicy::Partial
        );
        Ok(Registration {
            id: root.Id().map_err(map)?.to_string(),
            path: root.Path().map_err(map)?.Path().map_err(map)?.to_string(),
            name: root.DisplayNameResource().map_err(map)?.to_string(),
            icon: root.IconResource().map_err(map)?.to_string(),
            context,
        })
    })
    .unwrap()
}

#[test]
fn native_shell_existing_registration_refresh_and_restart() {
    let paths = [scratch("家庭 资料, Ω"), scratch("@work & files (2026)")];
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let catalogs = [
        Catalog::new(
            BindingId::from_bytes(seed.to_le_bytes()),
            VaultId::from_bytes([222; 16]),
        ),
        Catalog::new(
            BindingId::from_bytes((seed + 1).to_le_bytes()),
            VaultId::from_bytes([223; 16]),
        ),
    ];
    let mut adapters = Vec::new();
    let mut expected = Vec::new();
    for (path, catalog) in paths.iter().zip(&catalogs) {
        std::fs::create_dir(path).unwrap();
        std::fs::write(path.join("pending.txt"), b"unconfirmed local content").unwrap();
        let keep = VaultHandle::open(path)
            .unwrap()
            .inspect("pending.txt")
            .unwrap();
        register(path, catalog).unwrap();
        let mut catalog = catalog.clone();
        let identity = Identity {
            binding: catalog.binding,
            vault: catalog.vault,
            entry: EntryId::from_bytes([224; 16]),
            version: VersionId::from_bytes([225; 16]),
        };
        catalog.insert(identity).unwrap();
        let mut adapter = Adapter::connect(path, catalog.clone(), Arc::new(MetadataOnly)).unwrap();
        let remote = adapter
            .create("remote.bin", identity, 12345, false)
            .unwrap();
        // Emulate a persisted pre-S222 Explorer registration without unregistering CFAPI.
        let binding = catalog.binding;
        with_runtime(move || {
            let root = StorageProviderSyncRootManager::GetSyncRootInformationForId(&HSTRING::from(
                registration_id(binding)?,
            ))
            .map_err(map)?;
            root.SetDisplayNameResource(&HSTRING::from("vaultsyncd"))
                .map_err(map)?;
            root.SetIconResource(&HSTRING::from("%SystemRoot%\\system32\\imageres.dll,-1040"))
                .map_err(map)?;
            StorageProviderSyncRootManager::Register(&root).map_err(map)
        })
        .unwrap();
        assert_eq!(registration(binding).name, "vaultsyncd");
        for _ in 0..2 {
            register(path, &catalog).unwrap();
            assert_eq!(adapter.inspect("pending.txt").unwrap(), keep);
            assert_eq!(adapter.inspect("remote.bin").unwrap(), remote);
        }
        let current = registration(binding);
        assert_eq!(current.id, registration_id(binding).unwrap());
        assert_eq!(current.path, path.to_str().unwrap());
        assert_eq!(current.name, path.file_name().unwrap().to_str().unwrap());
        assert_eq!(current.context, catalog.vault_blob());
        let icon = decode_icon(&current.icon);
        assert!(!icon.starts_with(path));
        assert!(icon.parent().unwrap().ends_with("vaultsyncd\\shell-icons"));
        assert_eq!(
            std::fs::read(icon).unwrap(),
            include_bytes!("../../../../assets/branding/icon.ico")
        );
        expected.push((keep, remote, current));
        adapter.disconnect().unwrap();
        adapters.push(adapter);
    }
    assert_ne!(expected[0].2.id, expected[1].2.id);
    assert_ne!(expected[0].2.name, expected[1].2.name);
    assert_eq!(expected[0].2.icon, expected[1].2.icon);
    drop(adapters);
    // A new process reinitializes the WinRT worker and reloads the persisted icon;
    // no executable-neighbour resources or process-local registration data are used.
    for ((path, catalog), (keep, remote, before)) in paths.iter().zip(&catalogs).zip(&expected) {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shell::native_tests::native_shell_restart_child",
                "--nocapture",
            ])
            .env("VAULTSYNCD_S222_PATH", path)
            .env("VAULTSYNCD_S222_BINDING", catalog.binding.to_string())
            .env("VAULTSYNCD_S222_VAULT", catalog.vault.to_string())
            .env("VAULTSYNCD_S222_KEEP", format!("{keep:?}"))
            .env("VAULTSYNCD_S222_REMOTE", format!("{remote:?}"))
            .env("VAULTSYNCD_S222_ICON", &before.icon)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{} {}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert_eq!(registration(catalog.binding), *before);
        std::fs::remove_file(path.join("remote.bin")).unwrap();
        unregister(catalog.binding).unwrap();
        assert_eq!(
            std::fs::read(path.join("pending.txt")).unwrap(),
            b"unconfirmed local content"
        );
        assert!(!registered(catalog.binding).unwrap());
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[test]
fn native_shell_restart_child() {
    let Some(path) = std::env::var_os("VAULTSYNCD_S222_PATH") else {
        return;
    };
    let path = PathBuf::from(path);
    let binding = BindingId::from_hex(&std::env::var("VAULTSYNCD_S222_BINDING").unwrap()).unwrap();
    let vault = VaultId::from_hex(&std::env::var("VAULTSYNCD_S222_VAULT").unwrap()).unwrap();
    let catalog = Catalog::new(binding, vault);
    let mut adapter = Adapter::connect(&path, catalog.clone(), Arc::new(MetadataOnly)).unwrap();
    for _ in 0..2 {
        register(&path, &catalog).unwrap();
        assert_eq!(
            format!("{:?}", adapter.inspect("pending.txt").unwrap()),
            std::env::var("VAULTSYNCD_S222_KEEP").unwrap()
        );
        assert_eq!(
            format!("{:?}", adapter.inspect("remote.bin").unwrap()),
            std::env::var("VAULTSYNCD_S222_REMOTE").unwrap()
        );
        assert_eq!(
            std::fs::read(path.join("pending.txt")).unwrap(),
            b"unconfirmed local content"
        );
        let current = registration(binding);
        assert_eq!(current.name, path.file_name().unwrap().to_str().unwrap());
        assert_eq!(current.context, catalog.vault_blob());
        assert_eq!(current.icon, std::env::var("VAULTSYNCD_S222_ICON").unwrap());
        decode_icon(&current.icon);
    }
    adapter.disconnect().unwrap();
}
