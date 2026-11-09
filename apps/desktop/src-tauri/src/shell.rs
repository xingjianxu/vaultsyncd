use crate::commands::*;
use crate::file_commands::*;
use crate::history_commands::*;
use crate::notifications::*;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

// Compiled only for the native acceptance binary; no test report command in production.
#[cfg(feature = "native-acceptance")]
#[tauri::command]
fn acceptance_report(app: tauri::AppHandle, report: serde_json::Value) -> Result<(), String> {
    let path = std::env::var_os("VAULTSYNCD_S080_REPORT").ok_or("missing test report path")?;
    std::fs::write(
        path,
        serde_json::to_vec(&report).map_err(|_| "invalid report")?,
    )
    .map_err(|_| "report write failed")?;
    app.exit(if report["ok"] == true { 0 } else { 1 });
    Ok(())
}

#[cfg(feature = "native-acceptance")]
#[tauri::command]
fn acceptance_file_control(action: String) -> Result<(), String> {
    use std::{
        fs::OpenOptions,
        os::windows::fs::OpenOptionsExt,
        sync::{Mutex, OnceLock},
    };
    static HELD: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
    let fixture = std::env::var_os("VAULTSYNCD_S082_FIXTURE").ok_or("missing fixture")?;
    let fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture).map_err(|_| "fixture read")?)
            .map_err(|_| "fixture decode")?;
    let path = std::path::PathBuf::from(
        fixture["localPath"]
            .as_str()
            .ok_or("missing test local path")?,
    );
    let mut held = HELD
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "test lock")?;
    match action.as_str() {
        "hold" => {
            *held = Some(
                OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(path.join("server-seed.txt"))
                    .map_err(|_| "test exclusive open")?,
            )
        }
        "unhold" => *held = None,
        "late-dirty" => {
            std::fs::write(
                path.join("S105-last-moment.txt"),
                b"S105 unique content written after detach confirmation opened",
            )
            .map_err(|_| "test last moment write")?;
        }
        "dirty" => {
            std::fs::write(path.join("local-only.txt"), b"S082-unconfirmed-local")
                .map_err(|_| "test dirty write")?;
            std::fs::write(path.join("pending-only.txt"), b"S082-pending-local")
                .map_err(|_| "test pending write")?;
        }
        _ => return Err("unknown test action".into()),
    }
    Ok(())
}

#[cfg(feature = "native-acceptance")]
#[tauri::command]
async fn acceptance_fixture_phase(action: String) -> Result<(), String> {
    if !matches!(
        action.as_str(),
        "restart" | "duplicate" | "resolve" | "recur" | "offline"
    ) {
        return Err("unknown fixture phase".into());
    }
    let fixture = std::env::var_os("VAULTSYNCD_S083_FIXTURE").ok_or("missing fixture")?;
    let fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture).map_err(|_| "fixture read")?)
            .map_err(|_| "fixture decode")?;
    let request = std::path::PathBuf::from(fixture["requestPath"].as_str().ok_or("request path")?);
    let ack = std::path::PathBuf::from(fixture["ackPath"].as_str().ok_or("ack path")?);
    tauri::async_runtime::spawn_blocking(move || {
        match std::fs::remove_file(&ack) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("ack cleanup".into()),
        }
        std::fs::write(request, &action).map_err(|_| "fixture request")?;
        let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < end {
            if std::fs::read_to_string(&ack).is_ok_and(|s| s == action) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err("offline fixture controller timeout".into())
    })
    .await
    .map_err(|_| "fixture controller thread")?
}
#[cfg(feature = "native-acceptance")]
#[tauri::command]
fn acceptance_toast_history(clear: bool) -> Result<Vec<String>, String> {
    use vaultsyncd_platform_windows::shell::toast;
    if clear {
        toast::clear_history().map_err(|e| e.to_string())?;
    }
    toast::history_tags().map_err(|e| e.to_string())
}

pub(super) fn run() {
    let builder = tauri::Builder::default();
    #[cfg(not(feature = "native-acceptance"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        agent_list_files,
        agent_download_entry,
        agent_release_entry,
        agent_list_problems,
        agent_retry_problem,
        agent_set_mode,
        agent_get_rules,
        agent_set_rules,
        agent_detach_binding,
        agent_export_recovery,
        agent_list_history,
        agent_list_trash,
        agent_restore_entry,
        agent_notification_settings,
        agent_set_notifications,
        agent_get_status,
        agent_set_connection,
        agent_login,
        agent_logout,
        agent_list_vaults,
        select_local_directory,
        agent_create_binding,
        agent_pause_binding,
        agent_resume_binding
    ]);
    #[cfg(feature = "native-acceptance")]
    let builder = builder.invoke_handler(tauri::generate_handler![
        agent_list_files,
        agent_download_entry,
        agent_release_entry,
        agent_list_problems,
        agent_retry_problem,
        agent_set_mode,
        agent_get_rules,
        agent_set_rules,
        agent_detach_binding,
        agent_export_recovery,
        agent_list_history,
        agent_list_trash,
        agent_restore_entry,
        agent_notification_settings,
        agent_set_notifications,
        agent_get_status,
        agent_set_connection,
        agent_login,
        agent_logout,
        agent_list_vaults,
        select_local_directory,
        agent_create_binding,
        agent_pause_binding,
        agent_resume_binding,
        acceptance_report,
        acceptance_file_control,
        acceptance_fixture_phase,
        acceptance_toast_history
    ]);
    builder
        .setup(|app| {
            app.manage(crate::notifications::Notifications::load());
            #[cfg(feature = "native-acceptance")]
            app.add_capability(r#"{"identifier":"native-acceptance","windows":["main"],"permissions":["allow-acceptance-report","allow-acceptance-file-control","allow-acceptance-fixture-phase","allow-acceptance-toast-history"]}"#)?;
            let window =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    .title("vaultsyncd")
                    .inner_size(1000.0, 720.0)
                    .on_web_resource_request(|_request, response| {
                        // This hook only runs for Tauri's bundled asset protocol.
                        // Mint a fresh nonce for each HTML response, including reloads.
                        if !response.headers().get("content-type").is_some_and(|value| {
                            value.to_str().is_ok_and(|value| value.starts_with("text/html"))
                        }) {
                            return;
                        }
                        use rand_core::RngCore;
                        let mut bytes = [0_u8; 16];
                        let updated = rand_core::OsRng.try_fill_bytes(&mut bytes).ok().and_then(|()| {
                            let nonce: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
                            let html = std::str::from_utf8(response.body()).ok()?;
                            let policy = response.headers().get("content-security-policy")?.to_str().ok()?;
                            let (html, policy) = crate::style_csp::with_style_nonce(html, policy, &nonce)?;
                            let policy = policy.parse::<tauri::http::HeaderValue>().ok()?;
                            Some((html, policy))
                        });
                        if let Some((html, policy)) = updated {
                            *response.body_mut() = html.into_bytes().into();
                            response.headers_mut().insert("content-security-policy", policy);
                            response.headers_mut().remove("content-length");
                        } else {
                            *response.status_mut() = tauri::http::StatusCode::INTERNAL_SERVER_ERROR;
                            *response.body_mut() = b"Desktop style policy unavailable".as_slice().into();
                            response.headers_mut().remove("content-length");
                        }
                    })
                    .on_navigation(|url| {
                        (url.scheme() == "http" && url.host_str() == Some("tauri.localhost"))
                            || (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
                    });
            #[cfg(feature = "native-acceptance")]
            let window = if let Some(fixture)=std::env::var_os("VAULTSYNCD_S083_FIXTURE"){
                let fixture:serde_json::Value=serde_json::from_slice(&std::fs::read(fixture)?)?;
                window.initialization_script(format!("window.__S083_FIXTURE__ = {fixture};"))
                    .initialization_script(include_str!("../../../../tests/platform/windows/s083-webview.js"))
            } else if let Some(fixture) = std::env::var_os("VAULTSYNCD_S082_FIXTURE") {
                let fixture: serde_json::Value = serde_json::from_slice(&std::fs::read(fixture)?)?;
                window.initialization_script(format!("window.__S082_FIXTURE__ = {fixture};"))
                    .initialization_script(include_str!("../../../../tests/platform/windows/s082-webview.js"))
            } else if let Some(fixture) = std::env::var_os("VAULTSYNCD_S081_FIXTURE") {
                let fixture: serde_json::Value = serde_json::from_slice(&std::fs::read(fixture)?)?;
                window.initialization_script(format!("window.__S081_FIXTURE__ = {fixture};"))
                    .initialization_script(include_str!("../../../../tests/platform/windows/s081-webview.js"))
            } else {
                window.initialization_script(include_str!("../../../../tests/platform/windows/s080-webview.js"))
            };
            window.build()?;
            let open =
                tauri::menu::MenuItem::with_id(app, "open", "打开 vaultsyncd", true, None::<&str>)?;
            let quit = tauri::menu::MenuItem::with_id(app, "quit", "退出界面", true, None::<&str>)?;
            let menu = tauri::menu::Menu::with_items(app, &[&open, &quit])?;
            tauri::tray::TrayIconBuilder::new()
                .icon(tauri::image::Image::new_owned(
                    [28, 120, 98, 255].repeat(16 * 16),
                    16,
                    16,
                ))
                .tooltip("vaultsyncd")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("desktop runtime failed");
}
