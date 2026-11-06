use crate::commands::*;
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

pub(super) fn run() {
    let builder = tauri::Builder::default();
    #[cfg(not(feature = "native-acceptance"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        agent_get_status,
        agent_set_connection,
        agent_login,
        agent_logout,
        agent_list_roots,
        agent_create_binding,
        agent_pause_binding,
        agent_resume_binding
    ]);
    #[cfg(feature = "native-acceptance")]
    let builder = builder.invoke_handler(tauri::generate_handler![
        agent_get_status,
        agent_set_connection,
        agent_login,
        agent_logout,
        agent_list_roots,
        agent_create_binding,
        agent_pause_binding,
        agent_resume_binding,
        acceptance_report
    ]);
    builder
        .setup(|app| {
            #[cfg(feature = "native-acceptance")]
            app.add_capability(r#"{"identifier":"native-acceptance","windows":["main"],"permissions":["allow-acceptance-report"]}"#)?;
            let window =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    .title("vaultsyncd")
                    .inner_size(1000.0, 720.0)
                    .on_navigation(|url| {
                        (url.scheme() == "http" && url.host_str() == Some("tauri.localhost"))
                            || (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
                    });
            #[cfg(feature = "native-acceptance")]
            let window = if let Some(fixture) = std::env::var_os("VAULTSYNCD_S081_FIXTURE") {
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
