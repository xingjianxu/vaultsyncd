//! Per-user classic Explorer cascade. The desktop resolves selected paths through agent IPC.
//!
//! Windows 11 exposes these static verbs under "Show more options". They do not install a
//! COM extension, require elevation, or execute the selected file. AppliesTo is a visibility
//! filter only; the desktop and agent must independently validate every invocation.
use std::{
    ffi::c_void,
    io,
    path::{Component, Path, PathBuf, Prefix},
    ptr,
};

const ROOT: &str = r"Software\Classes\*\shell\vaultsyncd";
// ExtendedSubCommandsKey is relative to HKCR; its target contains the shell subverbs.
const SUBCOMMANDS_CLASS: &str = r"*\shell\vaultsyncd";
const OBSOLETE_VALUES: &[&str] = &["SubCommands", ""];
const OWNER: &str = "org.vaultsyncd.desktop.explorer-menu";
const KEY_READ_WRITE: u32 = 0x0002_001f;
const REG_SZ: u32 = 1;
const FILE_NOT_FOUND: i32 = 2;
const MORE_DATA: i32 = 234;
type Handle = *mut c_void;
const CURRENT_USER: Handle = (-2_147_483_647_isize) as Handle;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(
        key: Handle,
        subkey: *const u16,
        options: u32,
        access: u32,
        result: *mut Handle,
    ) -> i32;
    fn RegCreateKeyExW(
        key: Handle,
        subkey: *const u16,
        reserved: u32,
        class: *mut u16,
        options: u32,
        access: u32,
        security: *const c_void,
        result: *mut Handle,
        disposition: *mut u32,
    ) -> i32;
    fn RegQueryValueExW(
        key: Handle,
        name: *const u16,
        reserved: *mut u32,
        kind: *mut u32,
        data: *mut u8,
        size: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        key: Handle,
        name: *const u16,
        reserved: u32,
        kind: u32,
        data: *const u8,
        size: u32,
    ) -> i32;
    fn RegDeleteValueW(key: Handle, name: *const u16) -> i32;
    fn RegDeleteTreeW(key: Handle, subkey: *const u16) -> i32;
    fn RegCloseKey(key: Handle) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CompareStringOrdinal(
        left: *const u16,
        left_length: i32,
        right: *const u16,
        right_length: i32,
        ignore_case: i32,
    ) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn SHChangeNotify(event: i32, flags: u32, item1: *const c_void, item2: *const c_void);
}

fn associations_changed() {
    // SHCNE_ASSOCCHANGED / SHCNF_IDLIST invalidates Explorer's verb/association caches.
    unsafe { SHChangeNotify(0x0800_0000, 0, ptr::null(), ptr::null()) };
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn status(code: i32) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}
struct Key(Handle);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}
impl Key {
    fn open(parent: Handle, path: &str) -> io::Result<Option<Self>> {
        let mut key = ptr::null_mut();
        let code =
            unsafe { RegOpenKeyExW(parent, wide(path).as_ptr(), 0, KEY_READ_WRITE, &mut key) };
        if code == FILE_NOT_FOUND {
            return Ok(None);
        }
        status(code)?;
        Ok(Some(Self(key)))
    }
    fn create(parent: Handle, path: &str) -> io::Result<Self> {
        let mut key = ptr::null_mut();
        status(unsafe {
            RegCreateKeyExW(
                parent,
                wide(path).as_ptr(),
                0,
                ptr::null_mut(),
                0,
                KEY_READ_WRITE,
                ptr::null(),
                &mut key,
                ptr::null_mut(),
            )
        })?;
        Ok(Self(key))
    }
    fn get(&self, name: &str) -> io::Result<Option<String>> {
        let name = wide(name);
        let mut kind = 0;
        let mut bytes = 0;
        let code = unsafe {
            RegQueryValueExW(
                self.0,
                name.as_ptr(),
                ptr::null_mut(),
                &mut kind,
                ptr::null_mut(),
                &mut bytes,
            )
        };
        if code == FILE_NOT_FOUND {
            return Ok(None);
        }
        status(code)?;
        if kind != REG_SZ || bytes % 2 != 0 || bytes > 256 * 1024 {
            return Err(io::Error::other("invalid Explorer menu registry value"));
        }
        let mut data = vec![0_u16; bytes as usize / 2];
        let code = unsafe {
            RegQueryValueExW(
                self.0,
                name.as_ptr(),
                ptr::null_mut(),
                &mut kind,
                data.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if code == MORE_DATA {
            return Err(io::Error::other(
                "Explorer menu registry changed while reading",
            ));
        }
        status(code)?;
        if kind != REG_SZ || bytes < 2 || bytes % 2 != 0 {
            return Err(io::Error::other("invalid Explorer menu registry string"));
        }
        data.truncate(bytes as usize / 2);
        if data.pop() != Some(0) || data.contains(&0) {
            return Err(io::Error::other("invalid Explorer menu registry string"));
        }
        String::from_utf16(&data)
            .map(Some)
            .map_err(io::Error::other)
    }
    fn set(&self, name: &str, value: &str) -> io::Result<()> {
        let value = wide(value);
        let bytes = u32::try_from(value.len() * 2)
            .map_err(|_| io::Error::other("Explorer menu registry value too large"))?;
        status(unsafe {
            RegSetValueExW(
                self.0,
                wide(name).as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                bytes,
            )
        })
    }
    fn remove_value(&self, name: &str) -> io::Result<()> {
        let code = unsafe { RegDeleteValueW(self.0, wide(name).as_ptr()) };
        if code == FILE_NOT_FOUND {
            Ok(())
        } else {
            status(code)
        }
    }
    fn owned(&self) -> io::Result<()> {
        if self.get("Owner")?.as_deref() != Some(OWNER) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Explorer menu registry key belongs to another application",
            ));
        }
        Ok(())
    }
}

fn local_path(path: &Path) -> io::Result<&str> {
    let text = path
        .to_str()
        .ok_or_else(|| io::Error::other("Explorer menu path must be Unicode"))?;
    let mut parts = path.components();
    if !matches!(parts.next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_)))
        || parts.next() != Some(Component::RootDir)
        || !parts.all(|part| matches!(part, Component::Normal(_)))
        || text
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '<' | '>' | '|' | '*' | '?'))
        || text[2..].contains(':')
        || text[2..]
            .split(['\\', '/'])
            .any(|part| matches!(part, "." | ".."))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Explorer menu requires an absolute ordinary local drive path",
        ));
    }
    Ok(text)
}

fn same_component(left: &str, right: &str) -> bool {
    let left: Vec<_> = left.encode_utf16().collect();
    let right: Vec<_> = right.encode_utf16().collect();
    let (Ok(left_length), Ok(right_length)) =
        (i32::try_from(left.len()), i32::try_from(right.len()))
    else {
        return false;
    };
    // CompareStringOrdinal returns CSTR_EQUAL (2), or zero on failure. Fail closed.
    unsafe {
        CompareStringOrdinal(left.as_ptr(), left_length, right.as_ptr(), right_length, 1) == 2
    }
}

/// Compare validated ordinary local paths using Windows ordinal case-insensitive components.
/// Invalid paths and comparison failures are unequal. This is lexical comparison only;
/// callers must inspect the physical identity before performing a file action.
pub fn same_file_path(left: &Path, right: &Path) -> bool {
    if local_path(left).is_err() || local_path(right).is_err() {
        return false;
    }
    let left: Vec<_> = left.components().collect();
    let right: Vec<_> = right.components().collect();
    left.len() == right.len()
        && left.iter().zip(&right).all(|(left, right)| {
            same_component(
                left.as_os_str().to_str().unwrap(),
                right.as_os_str().to_str().unwrap(),
            )
        })
}

/// Resolve a strict descendant lexically using Windows ordinal case-insensitive components.
/// Returns a forward-slash relative path, rejecting roots, siblings, ADS, UNC/device paths,
/// and explicit dot components. This performs no I/O or authorization; the caller must pin
/// and inspect the binding and selected file before acting on its identity.
pub fn relative_file(root: &Path, file: &Path) -> io::Result<String> {
    local_path(root)?;
    local_path(file)?;
    let root: Vec<_> = root.components().collect();
    let file: Vec<_> = file.components().collect();
    if file.len() <= root.len()
        || !root.iter().zip(&file).all(|(root, file)| {
            // local_path already requires Unicode for the whole path.
            same_component(
                root.as_os_str().to_str().unwrap(),
                file.as_os_str().to_str().unwrap(),
            )
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "selected file is outside the binding",
        ));
    }
    Ok(file[root.len()..]
        .iter()
        .map(|part| part.as_os_str().to_str().unwrap())
        .collect::<Vec<_>>()
        .join("/"))
}

#[derive(Debug)]
struct Value {
    key: String,
    name: &'static str,
    value: String,
}
fn values(executable: &Path, bindings: &[PathBuf]) -> io::Result<Vec<Value>> {
    let executable = local_path(executable)?;
    if !executable.to_ascii_lowercase().ends_with(".exe") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Explorer menu target must be an EXE",
        ));
    }
    let mut roots = bindings
        .iter()
        .map(|path| {
            let root = local_path(path)?.replace('/', "\\");
            // The separator is part of the prefix: C:\\Sync must not match C:\\SyncOther.
            Ok(format!(
                "System.ItemPathDisplay:~<\"{}\\\"",
                root.trim_end_matches('\\')
            ))
        })
        .collect::<io::Result<Vec<_>>>()?;
    roots.sort();
    roots.dedup();
    let mut result = Vec::new();
    let mut add = |key: &str, name, value: String| {
        result.push(Value {
            key: key.into(),
            name,
            value,
        });
    };
    for (name, value) in [
        ("Owner", OWNER.into()),
        ("MUIVerb", "vaultsyncd".into()),
        ("Icon", format!("\"{executable}\",0")),
        ("ExtendedSubCommandsKey", SUBCOMMANDS_CLASS.into()),
        ("MultiSelectModel", "Single".into()),
        ("NeverDefault", String::new()),
        (
            "AppliesTo",
            roots
                .into_iter()
                .map(|root| format!("({root})"))
                .collect::<Vec<_>>()
                .join(" OR "),
        ),
    ] {
        add("", name, value);
    }
    for (key, action, title) in [
        ("01history", "history", "查看文件历史"),
        ("02download", "download", "下载文件"),
        ("03release", "release", "释放空间"),
    ] {
        let key = format!("shell\\{key}");
        add(&key, "MUIVerb", title.into());
        add(&key, "MultiSelectModel", "Single".into());
        add(&key, "NeverDefault", String::new());
        add(&key, "NoWorkingDirectory", String::new());
        add(
            &format!("{key}\\command"),
            "",
            format!("\"{executable}\" --shell-action {action} -- \"%1\""),
        );
    }
    Ok(result)
}

/// Reconcile the current user's classic file menu against active binding directories.
/// An unchanged registration is read without writes; no bindings removes only our own key.
/// Call from a worker thread. The selected file is passed as a literal EXE argument.
pub fn sync(executable: &Path, bindings: &[PathBuf]) -> io::Result<()> {
    if bindings.is_empty() {
        return unregister();
    }
    let expected = values(executable, bindings)?;
    let root = Key::open(CURRENT_USER, ROOT)?;
    if let Some(root) = &root {
        root.owned()?;
        let mut same = root.get("LegacyDisable")?.is_none();
        for name in OBSOLETE_VALUES {
            same &= root.get(name)?.is_none();
        }
        for value in &expected {
            let key = Key::open(root.0, &value.key)?;
            same &= match key {
                Some(key) => key.get(value.name)?.as_deref() == Some(&value.value),
                None => false,
            };
        }
        if same {
            return Ok(());
        }
    }
    let root = match root {
        Some(root) => root,
        None => Key::create(CURRENT_USER, ROOT)?,
    };
    root.set("Owner", OWNER)?;
    // Do not expose partially updated verbs. A failed update remains hidden for retry.
    root.set("LegacyDisable", "")?;
    // Normalize the previous cascade selector to the explicit ExtendedSubCommandsKey
    // layout; never retain competing selectors. Native probes confirmed that empty
    // SubCommands with a local shell subtree is also supported by Explorer.
    // The parent also has no default verb, as required for a static cascade.
    for name in OBSOLETE_VALUES {
        root.remove_value(name)?;
    }
    for value in expected {
        Key::create(root.0, &value.key)?.set(value.name, &value.value)?;
    }
    root.remove_value("LegacyDisable")?;
    associations_changed();
    Ok(())
}

/// Remove only vaultsyncd's own per-user menu, preserving bindings, files, and other verbs.
pub fn unregister() -> io::Result<()> {
    let Some(root) = Key::open(CURRENT_USER, ROOT)? else {
        return Ok(());
    };
    root.owned()?;
    drop(root);
    let code = unsafe { RegDeleteTreeW(CURRENT_USER, wide(ROOT).as_ptr()) };
    if code == FILE_NOT_FOUND {
        Ok(())
    } else {
        status(code)?;
        associations_changed();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_are_literal_prefixes_and_commands_never_use_a_shell() {
        let values = values(
            Path::new(r"C:\client & 100%\vaultsyncd-desktop.exe"),
            &[
                PathBuf::from(r"D:\绑定 (一) & 100%"),
                PathBuf::from(r"C:\Sync"),
            ],
        )
        .unwrap();
        let applies = &values.iter().find(|v| v.name == "AppliesTo").unwrap().value;
        assert_eq!(
            applies,
            r#"(System.ItemPathDisplay:~<"C:\Sync\") OR (System.ItemPathDisplay:~<"D:\绑定 (一) & 100%\")"#
        );
        let command = &values
            .iter()
            .find(|v| v.key == r"shell\03release\command")
            .unwrap()
            .value;
        assert_eq!(
            command,
            r#""C:\client & 100%\vaultsyncd-desktop.exe" --shell-action release -- "%1""#
        );
        assert_eq!(
            values
                .iter()
                .filter(|v| v.name == "MultiSelectModel")
                .count(),
            4
        );
        assert_eq!(
            values
                .iter()
                .find(|v| v.key.is_empty() && v.name == "ExtendedSubCommandsKey")
                .unwrap()
                .value,
            r"*\shell\vaultsyncd"
        );
        assert!(values.iter().all(|v| v.name != "SubCommands"));
    }

    #[test]
    fn rejects_nonlocal_relative_and_unquotable_inputs() {
        let executable = Path::new(r"C:\client\vaultsyncd-desktop.exe");
        for path in [
            r"C:relative",
            r"\\server\share",
            r"\\?\C:\Sync",
            r"C:\Sync\..\Other",
            "C:\\bad\"quote",
            r"C:\Sync:stream",
        ] {
            assert!(
                values(executable, &[PathBuf::from(path)]).is_err(),
                "{path}"
            );
        }
        assert!(
            values(
                Path::new(r"C:\client\script.cmd"),
                &[PathBuf::from(r"C:\Sync")]
            )
            .is_err()
        );
    }

    #[test]
    fn descendants_use_ordinal_components_without_traversal_or_prefix_confusion() {
        let root = Path::new(r"C:\同步 Ä\Root");
        assert_eq!(
            relative_file(root, Path::new(r"c:\同步 ä\ROOT\子目录\file & 100%.txt")).unwrap(),
            "子目录/file & 100%.txt"
        );
        for file in [
            r"C:\同步 Ä\RootOther\file.txt",
            r"C:\同步 Ä\Root",
            r"C:\同步 Ä\Root\..\file.txt",
            r"C:\同步 Ä\Root\.\file.txt",
            r"C:\同步 Ä\Root\file.txt:stream",
            r"D:\同步 Ä\Root\file.txt",
        ] {
            assert!(relative_file(root, Path::new(file)).is_err(), "{file}");
        }
    }

    #[test]
    fn file_paths_compare_every_component_with_windows_ordinal_rules() {
        let selected = Path::new(r"c:\同步 ä\ROOT\子目录\FiLe & 100%.TXT");
        assert!(same_file_path(
            selected,
            Path::new(r"C:\同步 Ä\root\子目录\file & 100%.txt")
        ));
        assert!(same_file_path(
            selected,
            Path::new("C:/同步 Ä/root/子目录/file & 100%.txt")
        ));
        for different in [
            r"C:\同步 Ä\root\子目录\other.txt",
            r"C:\同步 Ä\root\子目录Other\file & 100%.txt",
            r"C:\同步 Ä\root\子目录\file & 100%.txt:stream",
            r"C:\同步 Ä\root\子目录\.\file & 100%.txt",
            r"D:\同步 Ä\root\子目录\file & 100%.txt",
        ] {
            assert!(
                !same_file_path(selected, Path::new(different)),
                "{different}"
            );
        }
    }
}
