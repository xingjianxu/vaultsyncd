//! Desktop-owned preferences; these never change the agent database or wire protocol.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CloseAction {
    #[default]
    Tray,
    Exit,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    pub close_action: CloseAction,
    pub stop_agent_on_exit: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            close_action: CloseAction::Tray,
            stop_agent_on_exit: true,
        }
    }
}
pub(crate) struct DesktopSettings {
    path: PathBuf,
    value: Mutex<Result<Settings, String>>,
}
impl DesktopSettings {
    pub(crate) fn load(path: PathBuf) -> Self {
        let value = read(&path);
        Self {
            path,
            value: Mutex::new(value),
        }
    }
    pub(crate) fn get(&self) -> Result<Settings, String> {
        self.value
            .lock()
            .map_err(|_| "程序配置状态不可用".to_owned())?
            .clone()
    }
    pub(crate) fn reload(&self) -> Result<Settings, String> {
        let mut current = self.value.lock().map_err(|_| "程序配置状态不可用")?;
        *current = read(&self.path);
        current.clone()
    }
    pub(crate) fn set(&self, settings: Settings) -> Result<Settings, String> {
        let mut current = self.value.lock().map_err(|_| "程序配置状态不可用")?;
        write(&self.path, settings).map_err(|e| format!("程序配置无法保存：{e}"))?;
        *current = Ok(settings);
        Ok(settings)
    }
}
fn read(path: &Path) -> Result<Settings, String> {
    if !path.is_absolute() {
        return Err("无法定位本用户程序配置；关闭窗口将保留在托盘。".into());
    }
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("程序配置损坏，请重新保存；关闭窗口将保留在托盘。详情：{e}")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(format!("程序配置无法读取；关闭窗口将保留在托盘。详情：{e}")),
    }
}
fn write(path: &Path, settings: Settings) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::other("配置路径不是本机绝对路径"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("配置目录不存在"))?;
    fs::create_dir_all(parent)?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temporary = parent.join(format!(
        "desktop-settings-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(&serde_json::to_vec_pretty(&settings)?)?;
        file.sync_all()?;
        drop(file);
        // Rust's Windows rename uses MoveFileExW(REPLACE_EXISTING); readers see a whole file.
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Profile(PathBuf);
    impl Profile {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "s200-settings-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("desktop-settings.json")
        }
        fn load(&self) -> DesktopSettings {
            DesktopSettings::load(self.path())
        }
    }
    impl Drop for Profile {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn absent_profile_defaults_to_tray_and_orderly_agent_stop_without_writing() {
        let profile = Profile::new();
        assert_eq!(profile.load().get().unwrap(), Settings::default());
        assert!(!profile.path().exists());
    }
    #[test]
    fn choices_survive_reload_and_atomic_replacement() {
        let profile = Profile::new();
        let state = profile.load();
        for close_action in [CloseAction::Tray, CloseAction::Exit] {
            for stop_agent_on_exit in [false, true] {
                let value = Settings {
                    close_action,
                    stop_agent_on_exit,
                };
                assert_eq!(state.set(value).unwrap(), value);
                assert_eq!(profile.load().get().unwrap(), value);
            }
        }
        assert_eq!(fs::read_dir(&profile.0).unwrap().count(), 1);
    }
    #[test]
    fn corrupt_and_unreadable_preferences_remain_visible_until_explicitly_saved() {
        let profile = Profile::new();
        fs::write(profile.path(), b"{broken").unwrap();
        let state = profile.load();
        assert!(state.get().is_err());
        assert_eq!(fs::read(profile.path()).unwrap(), b"{broken");
        state.set(Settings::default()).unwrap();
        assert_eq!(state.get().unwrap(), Settings::default());
        fs::remove_file(profile.path()).unwrap();
        fs::create_dir(profile.path()).unwrap();
        assert!(profile.load().get().is_err());
    }
    #[test]
    fn retry_reads_repaired_preferences_instead_of_repeating_the_cached_error() {
        let profile = Profile::new();
        fs::write(profile.path(), b"broken").unwrap();
        let state = profile.load();
        assert!(state.get().is_err());
        let repaired = Settings {
            close_action: CloseAction::Exit,
            stop_agent_on_exit: false,
        };
        fs::write(profile.path(), serde_json::to_vec(&repaired).unwrap()).unwrap();
        assert_eq!(state.reload().unwrap(), repaired);
        assert_eq!(state.get().unwrap(), repaired);
    }
    #[test]
    fn failed_save_preserves_active_choice_and_leaves_no_partial_file() {
        let profile = Profile::new();
        let state = profile.load();
        let value = Settings {
            close_action: CloseAction::Exit,
            stop_agent_on_exit: false,
        };
        state.set(value).unwrap();
        fs::remove_file(profile.path()).unwrap();
        fs::create_dir(profile.path()).unwrap();
        assert!(state.set(Settings::default()).is_err());
        assert_eq!(state.get().unwrap(), value);
        assert_eq!(fs::read_dir(&profile.0).unwrap().count(), 1);
    }
    #[test]
    fn invalid_actions_or_missing_fields_never_enable_exit() {
        for invalid in [
            r#"{}"#,
            r#"{"close_action":"quit","stop_agent_on_exit":true}"#,
            r#"{"close_action":"exit"}"#,
            r#"{"close_action":"exit","stop_agent_on_exit":true,"extra":1}"#,
        ] {
            assert!(serde_json::from_str::<Settings>(invalid).is_err());
        }
        assert!(
            DesktopSettings::load(PathBuf::from("relative.json"))
                .get()
                .is_err()
        );
    }
}
