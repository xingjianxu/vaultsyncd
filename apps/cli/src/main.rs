//! `vaultsyncd-cli`：Windows 用户会话里的最小管理入口（`docs/design.md` §5.1、§18.1）。
//!
//! 边界：
//!
//! - 只通过 `vaultsyncd_platform_windows::ipc::Endpoint` 调用运行中的 agent。CLI 不打开
//!   `client.sqlite`、不依赖 agent 核心或同步算法，也不自己判断暂停/释放是否安全；执行器是唯一
//!   的校验与副作用入口。
//! - 密码只从终端安全输入或标准输入（受控句柄）读取，从不接受 `--password`，不写命令行、shell
//!   历史、输出或日志。
//! - 成功与失败都有稳定退出码；`--json` 始终输出单行 JSON，错误也用同一种信封，便于脚本消费。
//! - 应答形状与命令不匹配时显式失败，不把缺失结果当成空列表或成功。

use std::{collections::HashMap, process::ExitCode};

#[cfg(windows)]
use vaultsyncd_local_ipc::generated::{Command, Request, ResponseKind};
use vaultsyncd_local_ipc::generated::{CommandName, CommandParameters, GetStatusResult};
#[cfg(any(windows, test))]
use vaultsyncd_local_ipc::generated::{ErrorBody, LocalIpcErrorCode};

/// 成功。
const EXIT_OK: u8 = 0;
/// 用法或本地输入错误（参数、密码输入等）。
const EXIT_USAGE: u8 = 2;
/// 无法连接运行中的 agent，或传输在完成前断开/超时。
const EXIT_UNAVAILABLE: u8 = 3;
/// agent 要求登录或重新认证。
#[cfg(any(windows, test))]
const EXIT_UNAUTHENTICATED: u8 = 4;
/// agent 拒绝命令，或命令执行失败。
const EXIT_REJECTED: u8 = 5;
/// 协议版本不兼容或应答形状无效。
const EXIT_PROTOCOL: u8 = 6;

const USAGE: &str = "\
用法：vaultsyncd-cli [--json] <命令> [选项]

命令：
  status                         查询 agent 版本、连接与全部 Binding 快照
  subscribe-status [--after-version <版本>]
                                 长轮询一次状态；游标过期时返回全量
  connection                     查看连接与证书配置（不返回凭据）
  set-connection --server-id <ID> --server-url <URL> [--ca-file <文件>]
  login --username <账号>         从标准输入读取密码后登录
  logout
  roots                          列出已授权 Root
  bindings                       列出持久 Binding
  files --binding-id <ID> [--cursor <ID>]  读取文件状态当前页
  create-binding --binding-id <ID> --root-id <ID> --local-path <目录>
                 --cache-path <目录> --mode <full|on_demand>
  pause --binding-id <ID>
  resume --binding-id <ID>
  mode --binding-id <ID> --mode <full|on_demand>
  detach --binding-id <ID>
  download --binding-id <ID> --entry-id <ID> [--operation-id <ID>]
  release --binding-id <ID> --entry-id <ID> [--operation-id <ID>]
  problems [--binding-id <ID>] [--search <文本>]
  retry --binding-id <ID> --operation-id <ID>
  history --binding-id <ID> --entry-id <ID> [--cursor <游标>]
  trash --binding-id <ID> [--cursor <游标>]
  restore --binding-id <ID> --entry-id <ID> --operation-id <ID>
          --base-revision <修订> [--version-id <ID>]
  export-recovery --binding-id <ID> --destination <目录>

全局选项：
  --json, -j                     输出单行机器可读 JSON（成功和失败都可用）
  -h, --help                     打印本说明

退出码：
  0  成功
  2  用法或本地输入错误
  3  无法连接运行中的 agent（命名管道不可用、断连或超时）
  4  未登录或需要重新认证
  5  agent 拒绝命令或执行失败
  6  IPC 版本不兼容或应答无效

密码只从标准输入读取：终端下不回显；管道只在首行读取。命令行参数、shell 历史与
JSON/日志输出都不包含密码。ID 为 32 位小写十六进制，由 agent 复核。";

/// CLI 本地失败：稳定退出码、机器可读 code 与可显示说明。
struct Failure {
    code: &'static str,
    message: String,
    exit: u8,
}

impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: "usage",
            message: message.into(),
            exit: EXIT_USAGE,
        }
    }

    #[cfg(windows)]
    fn unavailable() -> Self {
        Self {
            code: "agent_unavailable",
            message: "无法连接运行中的 vaultsyncd-agent；请确认同一 Windows 会话中 agent 正在运行"
                .into(),
            exit: EXIT_UNAVAILABLE,
        }
    }

    fn protocol(message: impl Into<String>) -> Self {
        Self {
            code: "protocol_error",
            message: message.into(),
            exit: EXIT_PROTOCOL,
        }
    }
}

/// 解析后的调用。登录密码在真正执行前才从 stdin 读取。
enum Action {
    Help,
    Ipc {
        name: CommandName,
        parameters: Box<Option<CommandParameters>>,
        login_username: Option<String>,
    },
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|arg| arg == "--json" || arg == "-j");
    let plain: Vec<String> = args
        .into_iter()
        .filter(|arg| arg != "--json" && arg != "-j")
        .collect();
    let action = match parse(&plain) {
        Ok(action) => action,
        Err(failure) => {
            render_failure(json, None, &failure);
            return ExitCode::from(failure.exit);
        }
    };
    let Action::Ipc {
        name,
        parameters,
        login_username,
    } = action
    else {
        println!("{USAGE}");
        return ExitCode::from(EXIT_OK);
    };
    let label = command_label(name);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            let failure = Failure {
                code: "internal",
                message: "无法创建 CLI 运行时".into(),
                exit: EXIT_REJECTED,
            };
            render_failure(json, Some(label), &failure);
            return ExitCode::from(failure.exit);
        }
    };
    match runtime.block_on(run(name, *parameters, login_username)) {
        Ok(result) => {
            if let Err(failure) = validate_result(name, &result) {
                render_failure(json, Some(label), &failure);
                return ExitCode::from(failure.exit);
            }
            render_success(json, name, &result);
            ExitCode::from(EXIT_OK)
        }
        Err(failure) => {
            render_failure(json, Some(label), &failure);
            ExitCode::from(failure.exit)
        }
    }
}

#[cfg(windows)]
async fn run(
    name: CommandName,
    parameters: Option<CommandParameters>,
    login_username: Option<String>,
) -> Result<GetStatusResult, Failure> {
    use vaultsyncd_platform_windows::ipc::Endpoint;

    let parameters = if name == CommandName::Login {
        let username = login_username.ok_or_else(|| Failure::usage("login 缺少 --username"))?;
        let password = read_password()?;
        Some(CommandParameters {
            username: Some(username),
            password: Some(password),
            ..Default::default()
        })
    } else {
        parameters
    };
    let request = Request {
        request_id: random_hex(),
        ipc_version: vaultsyncd_local_ipc::version::supported(),
        command: Command { name, parameters },
    };
    let endpoint = Endpoint::current("vaultsyncd-agent").map_err(|_| Failure::unavailable())?;
    let response = endpoint
        .request(&request)
        .await
        .map_err(|error| transport_failure(&error))?;
    match response.kind {
        ResponseKind::Ok => response
            .result
            .ok_or_else(|| Failure::protocol("agent 成功应答缺少 result")),
        ResponseKind::Error => {
            let error = response
                .error
                .ok_or_else(|| Failure::protocol("agent 错误应答缺少 error"))?;
            Err(agent_failure(error))
        }
    }
}

#[cfg(not(windows))]
async fn run(
    _name: CommandName,
    _parameters: Option<CommandParameters>,
    _login_username: Option<String>,
) -> Result<GetStatusResult, Failure> {
    Err(Failure {
        code: "unsupported_platform",
        message: "vaultsyncd-cli 只在 Windows 用户会话运行".into(),
        exit: EXIT_UNAVAILABLE,
    })
}

#[cfg(windows)]
fn transport_failure(error: &std::io::Error) -> Failure {
    // 不回显 io::Error 文本：其中可能带命名管道名（含 SID）或内部路径。
    match error.raw_os_error() {
        Some(2 | 3 | 5 | 109 | 231 | 233) => Failure::unavailable(),
        _ if error.kind() == std::io::ErrorKind::TimedOut => Failure::unavailable(),
        _ if error.kind() == std::io::ErrorKind::UnexpectedEof => Failure::unavailable(),
        _ => Failure::protocol("agent 应答无效或与请求不匹配"),
    }
}

#[cfg(any(windows, test))]
fn agent_failure(error: ErrorBody) -> Failure {
    let (exit, code) = match error.code {
        LocalIpcErrorCode::UnsupportedVersion => (EXIT_PROTOCOL, "unsupported_version"),
        LocalIpcErrorCode::Unauthenticated => (EXIT_UNAUTHENTICATED, "unauthenticated"),
        LocalIpcErrorCode::InvalidArgument => (EXIT_REJECTED, "invalid_argument"),
        LocalIpcErrorCode::NotFound => (EXIT_REJECTED, "not_found"),
        LocalIpcErrorCode::Blocked => (EXIT_REJECTED, "blocked"),
        LocalIpcErrorCode::Unavailable => (EXIT_REJECTED, "unavailable"),
        LocalIpcErrorCode::Conflict => (EXIT_REJECTED, "conflict"),
        LocalIpcErrorCode::Internal => (EXIT_REJECTED, "internal"),
    };
    Failure {
        code,
        message: error.message,
        exit,
    }
}

#[cfg(windows)]
fn read_password() -> Result<String, Failure> {
    use std::io::{BufRead, IsTerminal};
    let stdin = std::io::stdin();
    let password = if stdin.is_terminal() {
        rpassword::prompt_password("密码：")
            .map_err(|_| Failure::usage("无法从终端安全读取密码"))?
    } else {
        let mut line = String::new();
        stdin
            .lock()
            .read_line(&mut line)
            .map_err(|_| Failure::usage("无法从标准输入读取密码"))?;
        let end = line.trim_end_matches(['\r', '\n']).len();
        line.truncate(end);
        line
    };
    if password.is_empty() {
        return Err(Failure::usage("密码不能为空"));
    }
    // 密码只用于本次请求；不打印、不写日志、不进入 argv。
    Ok(password)
}

fn random_hex() -> String {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse(args: &[String]) -> Result<Action, Failure> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(Failure::usage("缺少命令；使用 --help 查看用法"));
    };
    if matches!(command, "help" | "-h" | "--help") {
        if args.len() != 1 {
            return Err(Failure::usage("help 不接受额外参数"));
        }
        return Ok(Action::Help);
    }
    let rest = &args[1..];
    let (name, parameters, login_username) = match command {
        "status" => no_options(rest, CommandName::GetStatus)?,
        "subscribe-status" => {
            let opts = parse_options(rest, &["--after-version"])?;
            (
                CommandName::SubscribeStatus,
                Some(CommandParameters {
                    after_version: optional(&opts, "--after-version"),
                    ..Default::default()
                }),
                None,
            )
        }
        "connection" => no_options(rest, CommandName::GetConnection)?,
        "set-connection" => {
            let opts = parse_options(rest, &["--server-id", "--server-url", "--ca-file"])?;
            (
                CommandName::SetConnection,
                Some(CommandParameters {
                    server_id: Some(required(&opts, "--server-id")?),
                    server_url: Some(required(&opts, "--server-url")?),
                    ca_file: optional(&opts, "--ca-file"),
                    ..Default::default()
                }),
                None,
            )
        }
        "login" => {
            let opts = parse_options(rest, &["--username"])?;
            let username = required(&opts, "--username")?;
            (CommandName::Login, None, Some(username))
        }
        "logout" => no_options(rest, CommandName::Logout)?,
        "roots" => no_options(rest, CommandName::ListRoots)?,
        "bindings" => no_options(rest, CommandName::ListBindings)?,
        "files" => {
            let opts = parse_options(rest, &["--binding-id", "--cursor"])?;
            (
                CommandName::ListFiles,
                Some(CommandParameters {
                    binding_id: Some(required(&opts, "--binding-id")?),
                    cursor: optional(&opts, "--cursor"),
                    ..Default::default()
                }),
                None,
            )
        }
        "create-binding" => {
            let opts = parse_options(
                rest,
                &[
                    "--binding-id",
                    "--root-id",
                    "--local-path",
                    "--cache-path",
                    "--mode",
                ],
            )?;
            (
                CommandName::CreateBinding,
                Some(CommandParameters {
                    binding_id: Some(required(&opts, "--binding-id")?),
                    root_id: Some(required(&opts, "--root-id")?),
                    local_path: Some(required(&opts, "--local-path")?),
                    cache_path: Some(required(&opts, "--cache-path")?),
                    mode: Some(mode(&opts)?),
                    ..Default::default()
                }),
                None,
            )
        }
        "pause" => binding_only(rest, CommandName::PauseBinding)?,
        "resume" => binding_only(rest, CommandName::ResumeBinding)?,
        "detach" => binding_only(rest, CommandName::DetachBinding)?,
        "mode" => {
            let opts = parse_options(rest, &["--binding-id", "--mode"])?;
            (
                CommandName::SetMode,
                Some(CommandParameters {
                    binding_id: Some(required(&opts, "--binding-id")?),
                    mode: Some(mode(&opts)?),
                    ..Default::default()
                }),
                None,
            )
        }
        "download" => entry_operation(rest, CommandName::DownloadEntry)?,
        "release" => entry_operation(rest, CommandName::ReleaseEntry)?,
        "problems" => {
            let opts = parse_options(rest, &["--binding-id", "--search"])?;
            (
                CommandName::ListProblems,
                Some(CommandParameters {
                    binding_id: optional(&opts, "--binding-id"),
                    search: optional(&opts, "--search"),
                    ..Default::default()
                }),
                None,
            )
        }
        "retry" => {
            let opts = parse_options(rest, &["--binding-id", "--operation-id"])?;
            (
                CommandName::RetryProblem,
                Some(CommandParameters {
                    binding_id: Some(required(&opts, "--binding-id")?),
                    operation_id: Some(required(&opts, "--operation-id")?),
                    ..Default::default()
                }),
                None,
            )
        }
        "history" => {
            let opts = parse_options(rest, &["--binding-id", "--entry-id", "--cursor"])?;
            (
                CommandName::ListHistory,
                Some(CommandParameters {
                    binding_id: Some(required(&opts, "--binding-id")?),
                    entry_id: Some(required(&opts, "--entry-id")?),
                    cursor: optional(&opts, "--cursor"),
                    ..Default::default()
                }),
                None,
            )
        }
        "trash" => {
            let opts = parse_options(rest, &["--binding-id", "--cursor"])?;
            (
                CommandName::ListTrash,
                Some(CommandParameters {
                    binding_id: Some(required(&opts, "--binding-id")?),
                    cursor: optional(&opts, "--cursor"),
                    ..Default::default()
                }),
                None,
            )
        }
        "restore" => {
            let opts = parse_options(
                rest,
                &[
                    "--binding-id",
                    "--entry-id",
                    "--operation-id",
                    "--base-revision",
                    "--version-id",
                ],
            )?;
            (
                CommandName::RestoreEntry,
                Some(CommandParameters {
                    binding_id: Some(required(&opts, "--binding-id")?),
                    entry_id: Some(required(&opts, "--entry-id")?),
                    operation_id: Some(required(&opts, "--operation-id")?),
                    base_revision: Some(required(&opts, "--base-revision")?),
                    version_id: optional(&opts, "--version-id"),
                    ..Default::default()
                }),
                None,
            )
        }
        "export-recovery" => {
            let opts = parse_options(rest, &["--binding-id", "--destination"])?;
            (
                CommandName::ExportRecovery,
                Some(CommandParameters {
                    binding_id: Some(required(&opts, "--binding-id")?),
                    destination: Some(required(&opts, "--destination")?),
                    ..Default::default()
                }),
                None,
            )
        }
        _ => return Err(Failure::usage(format!("未知命令：{command}"))),
    };
    Ok(Action::Ipc {
        name,
        parameters: Box::new(parameters),
        login_username,
    })
}

fn no_options(
    rest: &[String],
    name: CommandName,
) -> Result<(CommandName, Option<CommandParameters>, Option<String>), Failure> {
    if !rest.is_empty() {
        return Err(Failure::usage(format!(
            "{} 不接受参数：{}",
            command_label(name),
            rest.join(" ")
        )));
    }
    Ok((name, None, None))
}

fn binding_only(
    rest: &[String],
    name: CommandName,
) -> Result<(CommandName, Option<CommandParameters>, Option<String>), Failure> {
    let opts = parse_options(rest, &["--binding-id"])?;
    Ok((
        name,
        Some(CommandParameters {
            binding_id: Some(required(&opts, "--binding-id")?),
            ..Default::default()
        }),
        None,
    ))
}

fn entry_operation(
    rest: &[String],
    name: CommandName,
) -> Result<(CommandName, Option<CommandParameters>, Option<String>), Failure> {
    let opts = parse_options(rest, &["--binding-id", "--entry-id", "--operation-id"])?;
    Ok((
        name,
        Some(CommandParameters {
            binding_id: Some(required(&opts, "--binding-id")?),
            entry_id: Some(required(&opts, "--entry-id")?),
            operation_id: Some(optional(&opts, "--operation-id").unwrap_or_else(random_hex)),
            ..Default::default()
        }),
        None,
    ))
}

fn mode(opts: &HashMap<&'static str, String>) -> Result<String, Failure> {
    let value = required(opts, "--mode")?;
    if value != "full" && value != "on_demand" {
        return Err(Failure::usage("--mode 只能是 full 或 on_demand"));
    }
    Ok(value)
}

fn parse_options(
    args: &[String],
    allowed: &[&'static str],
) -> Result<HashMap<&'static str, String>, Failure> {
    let mut values = HashMap::new();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if matches!(flag, "-h" | "--help") {
            return Err(Failure::usage("命令帮助请使用 vaultsyncd-cli --help"));
        }
        let Some(key) = allowed.iter().copied().find(|key| *key == flag) else {
            return Err(Failure::usage(format!("未知选项：{flag}")));
        };
        let Some(value) = args.get(index + 1) else {
            return Err(Failure::usage(format!("{key} 缺少取值")));
        };
        if value.is_empty() || value.starts_with("--") {
            return Err(Failure::usage(format!("{key} 的取值无效")));
        }
        if values.insert(key, value.clone()).is_some() {
            return Err(Failure::usage(format!("{key} 重复出现")));
        }
        index += 2;
    }
    Ok(values)
}

fn required(values: &HashMap<&'static str, String>, key: &'static str) -> Result<String, Failure> {
    values
        .get(key)
        .cloned()
        .ok_or_else(|| Failure::usage(format!("缺少 {key}")))
}

fn optional(values: &HashMap<&'static str, String>, key: &'static str) -> Option<String> {
    values.get(key).cloned()
}

fn command_label(name: CommandName) -> &'static str {
    match name {
        CommandName::GetStatus => "status",
        CommandName::SubscribeStatus => "subscribe-status",
        CommandName::ListBindings => "bindings",
        CommandName::CreateBinding => "create-binding",
        CommandName::PauseBinding => "pause",
        CommandName::ResumeBinding => "resume",
        CommandName::DownloadEntry => "download",
        CommandName::ReleaseEntry => "release",
        CommandName::ListProblems => "problems",
        CommandName::RetryProblem => "retry",
        CommandName::DetachBinding => "detach",
        CommandName::ExportRecovery => "export-recovery",
        CommandName::GetConnection => "connection",
        CommandName::SetConnection => "set-connection",
        CommandName::Login => "login",
        CommandName::Logout => "logout",
        CommandName::ListRoots => "roots",
        CommandName::ListFiles => "files",
        CommandName::ClaimNotification => "claim-notification",
        CommandName::SetMode => "mode",
        CommandName::ListHistory => "history",
        CommandName::ListTrash => "trash",
        CommandName::RestoreEntry => "restore",
    }
}

fn validate_result(name: CommandName, result: &GetStatusResult) -> Result<(), Failure> {
    let management = result.management.as_ref();
    let missing = match name {
        CommandName::GetStatus | CommandName::SubscribeStatus => {
            result.bindings.is_none().then_some("Binding 状态")
        }
        CommandName::ListBindings => management
            .and_then(|m| m.bindings.as_ref())
            .is_none()
            .then_some("Binding 列表"),
        CommandName::ListFiles => management
            .and_then(|m| m.files.as_ref())
            .is_none()
            .then_some("文件列表"),
        CommandName::ListProblems => management
            .and_then(|m| m.problems.as_ref())
            .is_none()
            .then_some("问题列表"),
        CommandName::ListRoots | CommandName::ListHistory | CommandName::ListTrash => management
            .and_then(|m| m.items.as_ref())
            .is_none()
            .then_some("远端条目列表"),
        CommandName::GetConnection
        | CommandName::SetConnection
        | CommandName::Login
        | CommandName::Logout => management
            .and_then(|m| m.connection.as_ref())
            .is_none()
            .then_some("连接事实"),
        CommandName::ExportRecovery => management
            .and_then(|m| m.exported_files.as_ref())
            .is_none()
            .then_some("导出文件计数"),
        _ => management.is_none().then_some("管理结果"),
    };
    match missing {
        Some(what) => Err(Failure::protocol(format!(
            "agent 成功应答缺少{what}，不能当作成功或空结果"
        ))),
        None => Ok(()),
    }
}

fn render_success(json: bool, name: CommandName, result: &GetStatusResult) {
    if json {
        let value = serde_json::json!({
            "ok": true,
            "command": command_label(name),
            "result": result,
        });
        println!("{value}");
        return;
    }
    let label = command_label(name);
    if matches!(name, CommandName::GetStatus | CommandName::SubscribeStatus) {
        println!(
            "agent {}，本机 IPC {}.{}，快照 {}",
            result.agent_version,
            result.ipc_protocol.major,
            result.ipc_protocol.minor,
            result.snapshot_version.as_deref().unwrap_or("(未提供)")
        );
    }
    let Some(management) = result.management.as_ref() else {
        println!("ok {label}");
        return;
    };
    if let Some(files) = &management.files {
        for row in files {
            println!(
                "{}：{}，驻留={}，未确认={}，待办={}；{}",
                row.path, row.status, row.residency, row.dirty, row.pending, row.release_reason
            );
        }
    }
    if let Some(connection) = management.connection.as_ref() {
        println!(
            "连接：server={} url={} 登录={}",
            connection.server_id,
            connection.server_url,
            if connection.authenticated {
                "是"
            } else {
                "否"
            }
        );
    }
    if let Some(bindings) = management.bindings.as_ref() {
        if bindings.is_empty() {
            println!("Binding：(无)");
        }
        for binding in bindings {
            println!(
                "Binding {} root={} path={} cache={} mode={}{}{}",
                binding.binding_id,
                binding.root_id,
                binding.local_path,
                binding.cache_path,
                binding.mode,
                if binding.paused { " paused" } else { "" },
                if binding.detached { " detached" } else { "" }
            );
        }
    }
    if let Some(items) = management.items.as_ref() {
        if items.is_empty() {
            println!("条目：(无)");
        }
        for item in items {
            println!(
                "条目 {} name={}{}{}{}",
                item.id,
                item.name,
                item.entry_id
                    .as_ref()
                    .map(|v| format!(" entry={v}"))
                    .unwrap_or_default(),
                item.version_id
                    .as_ref()
                    .map(|v| format!(" version={v}"))
                    .unwrap_or_default(),
                item.revision
                    .as_ref()
                    .map(|v| format!(" revision={v}"))
                    .unwrap_or_default()
            );
        }
        if let Some(cursor) = management.next_cursor.as_ref() {
            println!("下一页游标：{cursor}");
        }
    }
    if let Some(problems) = management.problems.as_ref() {
        if problems.is_empty() {
            println!("问题：(无)");
        }
        for problem in problems {
            println!(
                "问题 {} code={} retryable={} reason={} next={} local={}",
                problem.problem_id,
                problem.code,
                problem.retryable,
                problem.reason,
                problem.next_step,
                problem.local_data
            );
        }
    }
    if let Some(files) = management.exported_files.as_ref() {
        println!("恢复包已导出文件：{files}");
    }
    if let Some(operation) = management.operation_id.as_ref() {
        println!("operation_id={operation}");
    }
    if let Some(remaining) = management.mode_remaining.as_ref() {
        println!("mode_remaining={remaining}");
    }
    if let Some(unconfirmed) = management.unconfirmed.as_ref() {
        println!("保留的未确认条目：{}", unconfirmed.len());
    }
    if let Some(incomplete) = management.incomplete.as_ref() {
        println!("不保证离线可读条目：{}", incomplete.len());
    }
    if matches!(name, CommandName::GetStatus | CommandName::SubscribeStatus) {
        if let Some(bindings) = result.bindings.as_ref() {
            if bindings.is_empty() {
                println!("状态 Binding：(无)");
            }
            for binding in bindings {
                println!(
                    "状态 {} dirty={} pending={} failed={} transfers={} intents={} recovery={} mode_pending={} paused={} detached={} synchronized={}",
                    binding.binding_id,
                    binding.dirty,
                    binding.pending,
                    binding.failed,
                    binding.transfers,
                    binding.intents,
                    binding.recovery,
                    binding.mode_pending,
                    binding.paused,
                    binding.detached,
                    binding.synchronized
                );
            }
        }
    }
    println!("ok {label}");
}

fn render_failure(json: bool, label: Option<&str>, failure: &Failure) {
    if json {
        let value = serde_json::json!({
            "ok": false,
            "command": label,
            "error": {
                "code": failure.code,
                "message": failure.message,
                "retryable": false,
            },
            "exit_code": failure.exit,
        });
        println!("{value}");
    } else {
        eprintln!("错误（{}）：{}", failure.code, failure.message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: &[&str]) -> Vec<String> {
        value.iter().map(|s| (*s).to_string()).collect()
    }

    fn ipc(value: &[&str]) -> (CommandName, Option<CommandParameters>, Option<String>) {
        match parse(&args(value)) {
            Ok(Action::Ipc {
                name,
                parameters,
                login_username,
            }) => (name, *parameters, login_username),
            Ok(Action::Help) => panic!("expected command"),
            Err(_) => panic!("expected command"),
        }
    }

    #[test]
    fn parses_all_management_commands() {
        assert_eq!(ipc(&["status"]).0, CommandName::GetStatus);
        assert_eq!(
            ipc(&["subscribe-status", "--after-version", "p:1"]).0,
            CommandName::SubscribeStatus
        );
        assert_eq!(
            ipc(&[
                "create-binding",
                "--binding-id",
                "b",
                "--root-id",
                "r",
                "--local-path",
                "C:/a",
                "--cache-path",
                "C:/b",
                "--mode",
                "full"
            ])
            .1
            .unwrap()
            .mode
            .as_deref(),
            Some("full")
        );
        assert_eq!(
            ipc(&["pause", "--binding-id", "b"]).0,
            CommandName::PauseBinding
        );
        assert_eq!(
            ipc(&["resume", "--binding-id", "b"]).0,
            CommandName::ResumeBinding
        );
        assert_eq!(
            ipc(&["detach", "--binding-id", "b"]).0,
            CommandName::DetachBinding
        );
        assert_eq!(
            ipc(&["mode", "--binding-id", "b", "--mode", "on_demand"]).0,
            CommandName::SetMode
        );
        assert_eq!(
            ipc(&["download", "--binding-id", "b", "--entry-id", "e"]).0,
            CommandName::DownloadEntry
        );
        assert_eq!(
            ipc(&["release", "--binding-id", "b", "--entry-id", "e"]).0,
            CommandName::ReleaseEntry
        );
        assert_eq!(ipc(&["problems"]).0, CommandName::ListProblems);
        assert_eq!(
            ipc(&["retry", "--binding-id", "b", "--operation-id", "o"]).0,
            CommandName::RetryProblem
        );
        assert_eq!(
            ipc(&["history", "--binding-id", "b", "--entry-id", "e"]).0,
            CommandName::ListHistory
        );
        assert_eq!(
            ipc(&["trash", "--binding-id", "b"]).0,
            CommandName::ListTrash
        );
        assert_eq!(
            ipc(&[
                "restore",
                "--binding-id",
                "b",
                "--entry-id",
                "e",
                "--operation-id",
                "o",
                "--base-revision",
                "2"
            ])
            .0,
            CommandName::RestoreEntry
        );
        assert_eq!(
            ipc(&[
                "export-recovery",
                "--binding-id",
                "b",
                "--destination",
                "C:/out"
            ])
            .0,
            CommandName::ExportRecovery
        );
        assert_eq!(ipc(&["connection"]).0, CommandName::GetConnection);
        assert_eq!(
            ipc(&[
                "set-connection",
                "--server-id",
                "s",
                "--server-url",
                "https://x"
            ])
            .0,
            CommandName::SetConnection
        );
        assert_eq!(ipc(&["logout"]).0, CommandName::Logout);
        assert_eq!(ipc(&["roots"]).0, CommandName::ListRoots);
        assert_eq!(ipc(&["bindings"]).0, CommandName::ListBindings);
        let login = ipc(&["login", "--username", "owner"]);
        assert_eq!(login.0, CommandName::Login);
        assert_eq!(login.2.as_deref(), Some("owner"));
        assert!(login.1.is_none());
    }

    #[test]
    fn generated_operation_id_is_canonical() {
        let (_, parameters, _) = ipc(&["download", "--binding-id", "b", "--entry-id", "e"]);
        let id = parameters.unwrap().operation_id.unwrap();
        assert_eq!(id.len(), 32);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    #[test]
    fn rejects_usage_errors() {
        for value in [
            &["unknown"][..],
            &[][..],
            &["status", "--json"][..],
            &["pause"][..],
            &["pause", "--binding-id"][..],
            &["pause", "--binding-id", "a", "--binding-id", "b"][..],
            &["pause", "--binding-id", "a", "extra"][..],
            &["login", "--username", "u", "--password", "secret"][..],
            &["mode", "--binding-id", "b", "--mode", "invalid"][..],
            &["help", "extra"][..],
        ] {
            let failure = match parse(&args(value)) {
                Ok(_) => panic!("must reject {value:?}"),
                Err(failure) => failure,
            };
            assert_eq!(failure.exit, EXIT_USAGE, "{value:?}");
            assert_eq!(failure.code, "usage");
        }
    }

    #[test]
    fn agent_codes_have_stable_exit_codes() {
        for (code, exit) in [
            (LocalIpcErrorCode::UnsupportedVersion, EXIT_PROTOCOL),
            (LocalIpcErrorCode::Unauthenticated, EXIT_UNAUTHENTICATED),
            (LocalIpcErrorCode::InvalidArgument, EXIT_REJECTED),
            (LocalIpcErrorCode::NotFound, EXIT_REJECTED),
            (LocalIpcErrorCode::Blocked, EXIT_REJECTED),
            (LocalIpcErrorCode::Unavailable, EXIT_REJECTED),
            (LocalIpcErrorCode::Conflict, EXIT_REJECTED),
            (LocalIpcErrorCode::Internal, EXIT_REJECTED),
        ] {
            let failure = agent_failure(ErrorBody {
                code,
                message: "x".into(),
                retryable: false,
            });
            assert_eq!(failure.exit, exit);
        }
    }

    #[test]
    fn result_shape_is_not_silently_accepted() {
        let empty = GetStatusResult {
            agent_version: "0.1.0".into(),
            bindings: None,
            ipc_protocol: vaultsyncd_local_ipc::version::supported(),
            snapshot_version: None,
            management: None,
        };
        assert_eq!(
            validate_result(CommandName::ListBindings, &empty)
                .unwrap_err()
                .exit,
            EXIT_PROTOCOL
        );
        assert_eq!(
            validate_result(CommandName::GetStatus, &empty)
                .unwrap_err()
                .exit,
            EXIT_PROTOCOL
        );
    }
}
