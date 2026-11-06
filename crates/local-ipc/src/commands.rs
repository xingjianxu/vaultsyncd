//! Executor-side command shape validation, shared by every local frontend.
use crate::generated::{
    Command, CommandName as C, CommandParameters, ErrorBody, LocalIpcErrorCode,
};

/// Validate exact parameter sets, bounds and canonical identities before any side effect.
pub fn validate(command: &Command) -> Result<(), ErrorBody> {
    let empty = CommandParameters::default();
    let p = command.parameters.as_ref().unwrap_or(&empty);
    let fields = [
        ("binding_id", &p.binding_id),
        ("root_id", &p.root_id),
        ("entry_id", &p.entry_id),
        ("operation_id", &p.operation_id),
        ("version_id", &p.version_id),
        ("base_revision", &p.base_revision),
        ("local_path", &p.local_path),
        ("cache_path", &p.cache_path),
        ("mode", &p.mode),
        ("destination", &p.destination),
        ("server_id", &p.server_id),
        ("server_url", &p.server_url),
        ("ca_file", &p.ca_file),
        ("username", &p.username),
        ("password", &p.password),
        ("after_version", &p.after_version),
        ("notification_key", &p.notification_key),
        ("notification_generation", &p.notification_generation),
        ("cursor", &p.cursor),
        ("search", &p.search),
    ];
    let (required, optional): (&[&str], &[&str]) = match command.name {
        C::GetStatus | C::ListBindings | C::GetConnection | C::Logout | C::ListRoots => (&[], &[]),
        C::SubscribeStatus => (&[], &["after_version"]),
        C::ListFiles => (&["binding_id"], &["cursor"]),
        C::ClaimNotification => (&["notification_key", "notification_generation"], &[]),
        C::CreateBinding => (
            &["binding_id", "root_id", "local_path", "cache_path", "mode"],
            &[],
        ),
        C::PauseBinding | C::ResumeBinding | C::DetachBinding => (&["binding_id"], &[]),
        C::DownloadEntry | C::ReleaseEntry => (&["binding_id", "entry_id", "operation_id"], &[]),
        C::ListProblems => (&[], &["binding_id", "search"]),
        C::RetryProblem => (&["binding_id", "operation_id"], &[]),
        C::ExportRecovery => (&["binding_id", "destination"], &[]),
        C::SetConnection => (&["server_id", "server_url"], &["ca_file"]),
        C::Login => (&["username", "password"], &[]),
        C::SetMode => (&["binding_id", "mode"], &[]),
        C::ListHistory => (&["binding_id", "entry_id"], &["cursor"]),
        C::ListTrash => (&["binding_id"], &["cursor"]),
        C::RestoreEntry => (
            &["binding_id", "entry_id", "operation_id", "base_revision"],
            &["version_id"],
        ),
    };
    for (name, value) in fields {
        if required.contains(&name) && value.is_none() {
            return Err(invalid());
        }
        let Some(value) = value else { continue };
        if !required.contains(&name) && !optional.contains(&name) {
            return Err(invalid());
        }
        if value.is_empty() || value.len() > 4096 || value.contains('\0') {
            return Err(invalid());
        }
        if (name.ends_with("_id") || name == "cursor")
            && (value.len() != 32
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        {
            return Err(invalid());
        }
    }
    if p.mode
        .as_deref()
        .is_some_and(|m| !matches!(m, "full" | "on_demand"))
    {
        return Err(invalid());
    }
    if p.notification_generation.as_deref().is_some_and(|v| {
        v.parse::<i64>()
            .ok()
            .is_none_or(|n| n < 1 || n.to_string() != v)
    }) {
        return Err(invalid());
    }
    if p.base_revision.as_deref().is_some_and(|v| {
        v.parse::<i64>()
            .ok()
            .is_none_or(|n| n < 0 || n.to_string() != v)
    }) {
        return Err(invalid());
    }
    Ok(())
}
fn invalid() -> ErrorBody {
    ErrorBody {
        code: LocalIpcErrorCode::InvalidArgument,
        message: "命令参数缺失、越界、格式错误或不适用于此操作".into(),
        retryable: false,
    }
}
