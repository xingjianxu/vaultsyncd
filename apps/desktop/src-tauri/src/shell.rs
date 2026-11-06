use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use vaultsyncd_local_ipc::generated::{
    Command, CommandName, GetStatusResult, Request, ResponseKind,
};

#[derive(serde::Serialize)]
struct BridgeError {
    code: &'static str,
    message: &'static str,
}

#[tauri::command]
async fn agent_get_status() -> Result<GetStatusResult, BridgeError> {
    use rand_core::{OsRng, RngCore};
    use vaultsyncd_platform_windows::ipc::Endpoint;
    let unavailable = || BridgeError {
        code: "unavailable",
        message: "无法连接同一用户会话的本机 agent。",
    };
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let request = Request {
        request_id: bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        ipc_version: vaultsyncd_local_ipc::version::supported(),
        command: Command {
            name: CommandName::GetStatus,
            parameters: None,
        },
    };
    let endpoint = Endpoint::current("vaultsyncd-agent").map_err(|_| unavailable())?;
    let response = endpoint
        .request(&request)
        .await
        .map_err(|_| unavailable())?;
    if response.kind != ResponseKind::Ok {
        return Err(BridgeError {
            code: "agent_rejected",
            message: "agent 拒绝状态请求，请通过 CLI 查看原因。",
        });
    }
    response.result.ok_or(BridgeError {
        code: "invalid_response",
        message: "agent 状态应答无效。",
    })
}

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
    let builder = builder.invoke_handler(tauri::generate_handler![agent_get_status]);
    #[cfg(feature = "native-acceptance")]
    let builder = builder.invoke_handler(tauri::generate_handler![
        agent_get_status,
        acceptance_report
    ]);
    builder
        .setup(|app| {
            let window =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    .title("vaultsyncd")
                    .inner_size(1000.0, 720.0)
            .on_navigation(|url| {
                (url.scheme() == "http" && url.host_str() == Some("tauri.localhost"))
                    || (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
            });
            #[cfg(feature = "native-acceptance")]
            let window = window.initialization_script(include_str!(
                "../../../../tests/platform/windows/s080-webview.js"
            ));
            window.build()?;
            let open =
                tauri::menu::MenuItem::with_id(app, "open", "打开 vaultsyncd", true, None::<&str>)?;
            let quit = tauri::menu::MenuItem::with_id(app, "quit", "退出界面", true, None::<&str>)?;
            let menu = tauri::menu::Menu::with_items(app, &[&open, &quit])?;
            tauri::tray::TrayIconBuilder::new()
                .icon(tauri::image::Image::new_owned(
                    vec![28, 120, 98, 255].repeat(16 * 16),
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
