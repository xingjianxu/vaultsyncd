use rand_core::{OsRng, RngCore};
use vaultsyncd_local_ipc::generated::{
    Command, CommandName as C, CommandParameters as P, GetStatusResult, ManagementResult, Request,
    ResponseKind,
};
use vaultsyncd_platform_windows::ipc::Endpoint;

#[derive(serde::Serialize)]
pub(crate) struct BridgeError {
    code: &'static str,
    pub(crate) message: String,
    retryable: bool,
}
impl BridgeError {
    pub(crate) fn local(message: &str) -> Self {
        Self {
            code: "unavailable",
            message: message.into(),
            retryable: false,
        }
    }
}
pub(crate) async fn exchange(
    name: C,
    parameters: Option<P>,
) -> Result<GetStatusResult, BridgeError> {
    let command = Command { name, parameters };
    vaultsyncd_local_ipc::commands::validate(&command)
        .map_err(|_| BridgeError::local("输入无效；请核对服务器身份、目录绝对路径及模式。"))?;
    let mut bytes = [0; 16];
    OsRng.fill_bytes(&mut bytes);
    let request = Request {
        request_id: bytes.iter().map(|b| format!("{b:02x}")).collect(),
        ipc_version: vaultsyncd_local_ipc::version::supported(),
        command,
    };
    let response = Endpoint::current("vaultsyncd-agent")
        .map_err(|_| BridgeError::local("无法连接同一用户会话的本机 agent。"))?
        .request(&request)
        .await
        .map_err(|error| BridgeError {
            code: "unavailable",
            message: format!("本机 agent 请求未完成：{error}"),
            retryable: matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::WouldBlock
            ),
        })?;
    if response.kind != ResponseKind::Ok {
        let error = response.error;
        return Err(BridgeError {
            code: "agent_rejected",
            retryable: error.as_ref().is_some_and(|e| e.retryable),
            message: error
                .map(|e| e.message)
                .unwrap_or_else(|| "agent 返回无效错误应答。".into()),
        });
    }
    response
        .result
        .ok_or_else(|| BridgeError::local("agent 状态应答无效。"))
}
pub(crate) async fn manage(
    name: C,
    parameters: Option<P>,
) -> Result<ManagementResult, BridgeError> {
    exchange(name, parameters)
        .await?
        .management
        .ok_or_else(|| BridgeError::local("agent 未返回管理命令结果。"))
}
#[tauri::command]
pub(crate) async fn agent_get_status(
    app: tauri::AppHandle,
) -> Result<GetStatusResult, BridgeError> {
    let status = crate::notifications::status(app).await?;
    let menu_error = crate::shell_actions::sync_menu(&status).await.err();
    crate::shell_actions::report_menu_error(menu_error);
    Ok(status)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConnectionInput {
    server_id: String,
    server_url: String,
    ca_file: Option<String>,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConnectionAddress {
    server_url: String,
    ca_file: Option<String>,
}
#[tauri::command]
pub(crate) async fn agent_discover_connection(
    input: ConnectionAddress,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::DiscoverConnection,
        Some(P {
            server_url: Some(input.server_url),
            ca_file: input.ca_file,
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_set_connection(
    input: ConnectionInput,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::SetConnection,
        Some(P {
            server_id: Some(input.server_id),
            server_url: Some(input.server_url),
            ca_file: input.ca_file,
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_remove_connection(
    server_id: String,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::RemoveConnection,
        Some(P {
            server_id: Some(server_id),
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_login(
    server_id: String,
    username: String,
    password: String,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::Login,
        Some(P {
            server_id: Some(server_id),
            username: Some(username),
            password: Some(password),
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_logout(server_id: String) -> Result<ManagementResult, BridgeError> {
    manage(
        C::Logout,
        Some(P {
            server_id: Some(server_id),
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_list_vaults(server_id: String) -> Result<ManagementResult, BridgeError> {
    manage(
        C::ListVaults,
        Some(P {
            server_id: Some(server_id),
            ..Default::default()
        }),
    )
    .await
}

// Window handle comes from Tauri, never from caller parameters. The picker performs no sync I/O.
#[tauri::command]
pub(crate) async fn select_local_directory(
    window: tauri::WebviewWindow,
) -> Result<Option<String>, BridgeError> {
    let owner = window
        .hwnd()
        .map_err(|_| BridgeError::local("无法定位目录选择窗口。"))?
        .0 as isize;
    tauri::async_runtime::spawn_blocking(move || {
        vaultsyncd_platform_windows::shell::select_directory(owner)
            .map_err(|_| BridgeError::local("无法打开或读取目录选择器，请重新选择。"))
    })
    .await
    .map_err(|_| BridgeError::local("目录选择器运行失败，请重试。"))?
}

// The webview supplies only a binding identity, never an arbitrary shell target.
#[tauri::command]
pub(crate) async fn open_binding_directory(binding_id: String) -> Result<(), BridgeError> {
    let status = exchange(C::GetStatus, None).await?;
    let binding = status
        .management
        .and_then(|management| management.bindings)
        .and_then(|bindings| {
            bindings
                .into_iter()
                .find(|binding| binding.binding_id == binding_id && !binding.detached)
        })
        .ok_or_else(|| BridgeError::local("同步目录已不存在或已解除绑定，请重新读取状态。"))?;
    tauri::async_runtime::spawn_blocking(move || {
        vaultsyncd_platform_windows::shell::open_directory(std::path::Path::new(
            &binding.local_path,
        ))
        .map_err(|_| {
            BridgeError::local("无法打开本地目录，请检查目录是否存在、访问权限及默认文件管理器。")
        })
    })
    .await
    .map_err(|_| BridgeError::local("文件管理器启动失败，请重试。"))?
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BindingInput {
    server_id: String,
    binding_id: String,
    vault_id: String,
    local_path: String,
    mode: String,
}
#[tauri::command]
pub(crate) async fn agent_create_binding(
    input: BindingInput,
) -> Result<ManagementResult, BridgeError> {
    // Cache placement is fixed inside the app's user data, never a caller-selected internal path.
    if input.binding_id.len() != 32
        || !input
            .binding_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(BridgeError::local("绑定身份无效，请重新创建。"));
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| BridgeError::local("无法定位本用户应用数据目录。"))?;
    let cache = std::path::PathBuf::from(base)
        .join("vaultsyncd")
        .join("binding-cache")
        .join(&input.binding_id);
    let parameters = P {
        server_id: Some(input.server_id),
        binding_id: Some(input.binding_id),
        vault_id: Some(input.vault_id),
        local_path: Some(input.local_path),
        cache_path: Some(cache.to_string_lossy().into_owned()),
        mode: Some(input.mode),
        ..Default::default()
    };
    vaultsyncd_local_ipc::commands::validate(&Command {
        name: C::CreateBinding,
        parameters: Some(parameters.clone()),
    })
    .map_err(|_| {
        BridgeError::local("本地目录须为现有绝对路径，服务器、Vault 身份及模式须有效。")
    })?;
    std::fs::create_dir_all(&cache)
        .map_err(|_| BridgeError::local("无法创建应用缓存目录，请检查本机权限与空间。"))?;
    manage(C::CreateBinding, Some(parameters)).await
}
#[tauri::command]
pub(crate) async fn agent_pause_binding(
    binding_id: String,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::PauseBinding,
        Some(P {
            binding_id: Some(binding_id),
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_resume_binding(
    binding_id: String,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::ResumeBinding,
        Some(P {
            binding_id: Some(binding_id),
            ..Default::default()
        }),
    )
    .await
}
