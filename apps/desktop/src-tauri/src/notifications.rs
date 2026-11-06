use crate::commands::{BridgeError, exchange};
use std::sync::Mutex;
use tauri::Manager;
use vaultsyncd_local_ipc::generated::{CommandName as C, CommandParameters as P, GetStatusResult};
use vaultsyncd_platform_windows::shell::toast;

#[derive(Clone, serde::Serialize)]
pub(crate) struct Settings {
    pub enabled: bool,
    pub delivered: u64,
    pub blocked: u64,
    pub last_error: Option<String>,
}
pub(crate) struct Notifications {
    value: Mutex<Settings>,
    path: std::path::PathBuf,
}
impl Notifications {
    pub(crate) fn load() -> Self {
        let path = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default())
            .join("vaultsyncd/desktop-notifications.json");
        let mut settings = Settings {
            enabled: true,
            delivered: 0,
            blocked: 0,
            last_error: None,
        };
        if !path.is_absolute() {
            settings.enabled = false;
            settings.last_error = Some("无法定位用户通知设置，问题仍完整保留。".into());
            return Self {
                value: Mutex::new(settings),
                path,
            };
        }
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<bool>(&bytes) {
                Ok(enabled) => settings.enabled = enabled,
                Err(_) => {
                    settings.enabled = false;
                    settings.last_error =
                        Some("通知设置无法读取，请重新选择开关；问题记录保留。".into());
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                settings.enabled = false;
                settings.last_error = Some("无法读取通知设置，问题仍在页面。".into());
            }
        }
        Self {
            value: Mutex::new(settings),
            path,
        }
    }
}
#[tauri::command]
pub(crate) fn agent_notification_settings(
    state: tauri::State<'_, Notifications>,
) -> Result<Settings, String> {
    Ok(state
        .value
        .lock()
        .map_err(|_| "notification state lock")?
        .clone())
}
#[tauri::command]
pub(crate) fn agent_set_notifications(
    state: tauri::State<'_, Notifications>,
    enabled: bool,
) -> Result<Settings, String> {
    if !state.path.is_absolute() {
        return Err("无法定位本机通知设置".into());
    }
    let mut value = state.value.lock().map_err(|_| "notification state lock")?;
    std::fs::create_dir_all(state.path.parent().ok_or("notification settings path")?)
        .map_err(|_| "通知设置无法保存")?;
    std::fs::write(
        &state.path,
        if enabled { &b"true"[..] } else { &b"false"[..] },
    )
    .map_err(|_| "通知设置无法保存")?;
    value.enabled = enabled;
    value.last_error = None;
    Ok(value.clone())
}
pub(crate) async fn observe(app: &tauri::AppHandle, status: &GetStatusResult) {
    let state = app.state::<Notifications>();
    if !state.value.lock().is_ok_and(|s| s.enabled) {
        return;
    }
    let Some(notices) = status.management.as_ref().and_then(|m| m.notices.as_ref()) else {
        return;
    };
    for notice in notices
        .iter()
        .filter(|n| n.critical && !n.claimed && n.state == "active")
    {
        if !state.value.lock().is_ok_and(|s| s.enabled) {
            break;
        }
        let claimed = exchange(
            C::ClaimNotification,
            Some(P {
                notification_key: Some(notice.key.clone()),
                notification_generation: Some(notice.generation.clone()),
                ..Default::default()
            }),
        )
        .await;
        match claimed {
            Ok(value)
                if value
                    .management
                    .as_ref()
                    .and_then(|m| m.notification_claimed)
                    == Some(true) => {}
            Ok(_) => continue,
            Err(_) => {
                if let Ok(mut s) = state.value.lock() {
                    s.last_error = Some("无法领取系统通知，问题仍在页面。".into());
                }
                continue;
            }
        }
        if !state.value.lock().is_ok_and(|s| s.enabled) {
            break;
        }
        let tag = notice.key.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        let tag = format!("{tag:016x}");
        let body = format!(
            "Root {}：{}。未确认内容保留，请打开问题与恢复页面。",
            notice.root_id.as_deref().unwrap_or("全局"),
            notice.cause
        );
        let delivered = tauri::async_runtime::spawn_blocking(move || {
            toast::show(&tag, "vaultsyncd 需要处理", &body)
        })
        .await;
        #[cfg(feature = "native-acceptance")]
        if let Ok(Err(error)) = &delivered {
            eprintln!("native Toast delivery failed: {error:?}");
        }
        if let Ok(mut s) = state.value.lock() {
            match delivered {
                Ok(Ok(true)) => s.delivered += 1,
                Ok(Ok(false)) => {
                    s.blocked += 1;
                    s.last_error = Some("Windows已禁用系统通知，问题仍完整保留在页面。".into());
                }
                _ => {
                    s.blocked += 1;
                    s.last_error = Some("系统通知不可用，问题仍完整保留在页面。".into());
                }
            }
        }
    }
}
pub(crate) async fn status(app: tauri::AppHandle) -> Result<GetStatusResult, BridgeError> {
    let value = exchange(C::GetStatus, None).await?;
    observe(&app, &value).await;
    Ok(value)
}
