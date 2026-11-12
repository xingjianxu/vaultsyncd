//! File-scoped Explorer windows. Only the original native selection is actionable.
use crate::{
    commands::{self, BridgeError},
    shell_request::{Action, ShellRequest},
};
use std::{
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use vaultsyncd_local_ipc::generated::{
    BindingInfo, CommandName as C, CommandParameters as P, FileStatus, GetStatusResult,
    ManagementResult,
};
use vaultsyncd_platform_windows::{
    files::{FileIdentity, VaultHandle},
    shell::context_menu,
};

#[derive(Clone, serde::Serialize)]
pub(crate) struct Target {
    action: Action,
    binding_id: String,
    entry_id: String,
    path: String,
}
#[derive(Clone)]
struct Bound {
    target: Target,
    binding: BindingInfo,
    file: FileStatus,
    physical: FileIdentity,
    root: FileIdentity,
}
pub(crate) struct ShellState {
    request: ShellRequest,
    original: Mutex<Option<Bound>>,
    operation: Mutex<Option<String>>,
    executing: AtomicBool,
}
impl ShellState {
    pub(crate) fn new(request: ShellRequest) -> Self {
        Self {
            request,
            original: Mutex::new(None),
            operation: Mutex::new(None),
            executing: AtomicBool::new(false),
        }
    }
}

async fn resolve(request: &ShellRequest) -> Result<Bound, String> {
    let status = commands::exchange(C::GetStatus, None)
        .await
        .map_err(detail)?;
    let bindings = status
        .management
        .and_then(|m| m.bindings)
        .ok_or("agent 未返回绑定清单。")?;
    let mut candidates = bindings
        .into_iter()
        .filter(|b| !b.detached)
        .filter_map(|b| {
            context_menu::relative_file(std::path::Path::new(&b.local_path), &request.path)
                .ok()
                .map(|path| (b, path))
        });
    let (binding, relative) = candidates
        .next()
        .ok_or("此文件不在活动的同步目录中，或路径不安全。")?;
    if candidates.next().is_some() {
        return Err("文件所属绑定不唯一，请先核对同步目录。".into());
    }
    // Keep root/ancestor handles through the IPC lookup, then inspect without hydration.
    let vault = VaultHandle::open(std::path::Path::new(&binding.local_path))
        .map_err(|e| format!("无法安全打开同步目录：{e}"))?;
    let physical = vault
        .inspect(&relative)
        .map_err(|e| format!("文件已移动、被替换或无法访问：{e}"))?;
    if physical.directory || !physical.supported() || physical.links != 1 {
        return Err("右键操作仅支持同步目录内的普通单链接文件。".into());
    }
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    let mut found: Option<FileStatus> = None;
    loop {
        let page = commands::manage(
            C::ListFiles,
            Some(P {
                binding_id: Some(binding.binding_id.clone()),
                cursor,
                ..Default::default()
            }),
        )
        .await
        .map_err(detail)?;
        for file in page.files.ok_or("agent 未返回文件清单。")? {
            if context_menu::same_file_path(
                &PathBuf::from(&binding.local_path).join(&file.path),
                &request.path,
            ) {
                if found.is_some() {
                    return Err("文件路径身份不唯一。".into());
                }
                found = Some(file);
            }
        }
        cursor = page.next_cursor;
        let Some(next) = cursor.as_ref() else {
            break;
        };
        if !seen.insert(next.clone()) {
            return Err("agent 文件分页游标重复。".into());
        }
    }
    let file = found.ok_or("文件尚未同步确认，或已被排除/移动；请先在客户端核对。")?;
    let entry_id = file
        .entry_id
        .clone()
        .ok_or("文件尚未确认身份，不能执行右键操作。")?;
    let after = vault
        .inspect(&relative)
        .map_err(|e| format!("文件状态发生变化：{e}"))?;
    if after.identity != physical.identity {
        return Err("文件已被替换，请重新打开右键菜单。".into());
    }
    if let Some(cloud) = &after.cloud {
        let identity =
            vaultsyncd_platform_windows::cfapi::catalog::Identity::decode(&cloud.identity)
                .map_err(|_| "占位文件身份无效。")?;
        if identity.entry.to_string() != entry_id
            || identity.vault.to_string() != binding.vault_id
            || identity.binding.to_string() != binding.binding_id
        {
            return Err("占位文件与 agent 身份不一致。".into());
        }
    }
    Ok(Bound {
        target: Target {
            action: request.action,
            binding_id: binding.binding_id.clone(),
            entry_id,
            path: file.path.clone(),
        },
        binding,
        file,
        physical: physical.identity,
        root: vault.identity(),
    })
}
fn detail(error: BridgeError) -> String {
    error.message
}

async fn checked(state: &ShellState) -> Result<Bound, String> {
    let original = state
        .original
        .lock()
        .map_err(|_| "文件窗口状态锁失败。")?
        .clone()
        .ok_or("请先读取文件目标。")?;
    let current = resolve(&state.request).await?;
    if current.target.binding_id != original.target.binding_id
        || current.target.entry_id != original.target.entry_id
        || current.physical != original.physical
        || current.root != original.root
        || current.target.path != original.target.path
    {
        return Err("文件或绑定身份已变化，请重新打开右键菜单。".into());
    }
    Ok(current)
}
#[tauri::command]
pub(crate) async fn get_shell_target(
    state: tauri::State<'_, ShellState>,
) -> Result<Target, String> {
    if let Some(original) = state
        .original
        .lock()
        .map_err(|_| "文件窗口状态锁失败。")?
        .as_ref()
    {
        return Ok(original.target.clone());
    }
    let bound = resolve(&state.request).await?;
    let mut original = state.original.lock().map_err(|_| "文件窗口状态锁失败。")?;
    Ok(original.get_or_insert(bound).target.clone())
}
#[derive(serde::Serialize)]
pub(crate) struct Status {
    file: FileStatus,
    binding: BindingInfo,
}
#[tauri::command]
pub(crate) async fn shell_file_status(
    state: tauri::State<'_, ShellState>,
) -> Result<Status, String> {
    let current = checked(&state).await?;
    Ok(Status {
        file: current.file,
        binding: current.binding,
    })
}
#[tauri::command]
pub(crate) async fn shell_file_action(
    state: tauri::State<'_, ShellState>,
    operation_id: String,
) -> Result<ManagementResult, String> {
    if operation_id.len() != 32 || !operation_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("操作身份无效。".into());
    }
    if state
        .executing
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err("文件操作正在执行。".into());
    }
    struct InFlight<'a>(&'a AtomicBool);
    impl Drop for InFlight<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    let _held = InFlight(&state.executing);
    let current = checked(&state).await?;
    // A lost reply must reconcile the admitted ID even if a completed action changed residency.
    let retry = {
        let operation = state.operation.lock().map_err(|_| "文件操作状态锁失败。")?;
        if operation
            .as_ref()
            .is_some_and(|previous| previous != &operation_id)
        {
            return Err("请使用此窗口原操作身份核对结果。".into());
        }
        operation.is_some()
    };
    let command = match state.request.action {
        Action::History => return Err("历史窗口只提供查看操作。".into()),
        Action::Release if !retry && !current.file.release_allowed => {
            return Err(current.file.release_reason);
        }
        Action::Release => C::ReleaseEntry,
        Action::Download if !retry && (current.binding.paused || current.file.dirty) => {
            return Err("绑定已暂停或文件有未确认修改，请先核对。".into());
        }
        Action::Download => C::DownloadEntry,
    };
    *state.operation.lock().map_err(|_| "文件操作状态锁失败。")? = Some(operation_id.clone());
    commands::manage(
        command,
        Some(P {
            binding_id: Some(current.target.binding_id),
            entry_id: Some(current.target.entry_id),
            operation_id: Some(operation_id),
            ..Default::default()
        }),
    )
    .await
    .map_err(detail)
}

/// Keep menu failures visible without discarding a successfully authenticated agent status.
pub(crate) fn report_menu_error(error: Option<BridgeError>) {
    static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
    let message = error.map(detail);
    let mut previous = LAST_ERROR
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if *previous == message {
        return;
    }
    *previous = message.clone();
    if let Some(message) = message {
        tauri::async_runtime::spawn_blocking(move || {
            vaultsyncd_platform_windows::shell::dialog::desktop_error(&message);
        });
    }
}

/// Called by the ordinary main window's authenticated status poll.
pub(crate) async fn sync_menu(status: &GetStatusResult) -> Result<(), BridgeError> {
    let roots: Vec<PathBuf> = status
        .management
        .as_ref()
        .and_then(|m| m.bindings.as_ref())
        .ok_or_else(|| BridgeError::local("agent 未返回右键菜单需要的绑定清单。"))?
        .iter()
        .filter(|b| !b.detached)
        .map(|b| PathBuf::from(&b.local_path))
        .collect();
    let executable = std::env::current_exe()
        .map_err(|e| BridgeError::local(&format!("无法定位右键菜单程序：{e}")))?;
    tauri::async_runtime::spawn_blocking(move || context_menu::sync(&executable, &roots))
        .await
        .map_err(|_| BridgeError::local("右键菜单注册线程失败。"))?
        .map_err(|e| BridgeError::local(&format!("右键菜单注册失败：{e}")))
}
