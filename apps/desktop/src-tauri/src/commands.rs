use rand_core::{OsRng, RngCore};
use vaultsyncd_local_ipc::generated::{
    Command, CommandName as C, CommandParameters as P, GetStatusResult, ManagementResult, Request,
    ResponseKind,
};
use vaultsyncd_platform_windows::ipc::Endpoint;

#[derive(serde::Serialize)]
pub(crate) struct BridgeError {
    code: &'static str,
    message: String,
}
impl BridgeError {
    fn local(message: &str) -> Self {
        Self {
            code: "unavailable",
            message: message.into(),
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
        .map_err(|_| BridgeError::local("本机 agent 无响应；请检查后台进程后重新读取状态。"))?;
    if response.kind != ResponseKind::Ok {
        return Err(BridgeError {
            code: "agent_rejected",
            message: response
                .error
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
    crate::notifications::status(app).await
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConnectionInput {
    server_id: String,
    server_url: String,
    ca_file: Option<String>,
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
pub(crate) async fn agent_login(
    username: String,
    password: String,
) -> Result<ManagementResult, BridgeError> {
    manage(
        C::Login,
        Some(P {
            username: Some(username),
            password: Some(password),
            ..Default::default()
        }),
    )
    .await
}
#[tauri::command]
pub(crate) async fn agent_logout() -> Result<ManagementResult, BridgeError> {
    manage(C::Logout, None).await
}
#[tauri::command]
pub(crate) async fn agent_list_roots() -> Result<ManagementResult, BridgeError> {
    manage(C::ListRoots, None).await
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

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BindingInput {
    binding_id: String,
    root_id: String,
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
        binding_id: Some(input.binding_id),
        root_id: Some(input.root_id),
        local_path: Some(input.local_path),
        cache_path: Some(cache.to_string_lossy().into_owned()),
        mode: Some(input.mode),
        ..Default::default()
    };
    vaultsyncd_local_ipc::commands::validate(&Command {
        name: C::CreateBinding,
        parameters: Some(parameters.clone()),
    })
    .map_err(|_| BridgeError::local("本地目录须为现有绝对路径，Root 身份及模式须有效。"))?;
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
