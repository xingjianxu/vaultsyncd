//! `vaultsyncd-admin` 命令入口（design §5.3：只做参数解析、调用用例与输出结果）。
//!
//! `init` 在服务端**离线**时初始化首位 owner；在线管理只通过受限 Unix socket（S075）。
//! 它不经过 HTTP、不写日志文件、不接受命令行密码：
//!
//! - 密码只从 stdin 读；stdin 是终端时要求输两遍且不回显（`rpassword`）。
//! - 输出只有登录名、账号身份、数据库路径与“Argon2id”这一事实，**永不打印密码或散列**；散列即使
//!   不是明文，也会成为可以离线爆破的材料。
//! - 初始化前后都不修改 `sessions`（S043 之后才有），也不写任何“默认账号”。
//!
//! 退出码：
//!
//! | 码 | 含义 |
//! | --- | --- |
//! | 0 | 初始化完成 |
//! | 1 | 执行失败（状态目录不可用、数据库打不开、散列或写入失败） |
//! | 2 | 用法错误或输入不合规（参数不对、登录名/密码不合规、两次密码不一致） |
//! | 3 | 状态目录被占用：服务端正在运行，或另一个 init 正在进行 |
//! | 4 | 已经初始化过：拒绝覆盖现有账号 |

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

mod accounts;
mod init;
mod online;

/// 初始化成功的退出码。
pub const EXIT_OK: u8 = 0;
/// 执行失败的退出码。
pub const EXIT_FAILURE: u8 = 1;
/// 用法或输入错误的退出码。
pub const EXIT_USAGE: u8 = 2;
/// 状态目录被占用的退出码。
pub const EXIT_LOCKED: u8 = 3;
/// 已经初始化过的退出码。
pub const EXIT_ALREADY_INITIALIZED: u8 = 4;

/// 未给出 `--login` 时使用的登录名（唯一账号的默认称呼，不是默认密码）。
pub const DEFAULT_LOGIN: &str = "owner";

const USAGE: &str = "\
用法：vaultsyncd-admin init --state-dir <目录> [--login <登录名>]

  init                在服务端离线时初始化首位 owner 账号（只能成功一次）
  --state-dir <目录>   服务端的状态目录（绝对路径，与 vaultsyncd.toml 的 state_dir 相同）
  --login <登录名>     登录名，默认 owner；不做大小写折叠
  -h, --help           打印本说明

密码从标准输入读取：终端下提示输入两遍且不回显；从管道读取时只读一行。密码不接受命令行参数。";

/// 解析结果。
enum Command {
    Init(InitArgs),
    Help,
}

/// `init` 的参数。
struct InitArgs {
    state_dir: PathBuf,
    login: String,
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(source) => {
            eprintln!("无法创建 Tokio 运行时：{source}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    runtime.block_on(run())
}

async fn run() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| {
        ["roots", "status", "problems", "history", "trash"]
            .iter()
            .any(|command| arg == command)
    }) {
        return ExitCode::from(online::run(args).await);
    }
    let command = match parse_args(args.into_iter()) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let args = match command {
        Command::Help => {
            println!("{USAGE}\n\n{}", online::USAGE);
            return ExitCode::from(EXIT_OK);
        }
        Command::Init(args) => args,
    };

    let password = match init::read_password() {
        Ok(password) => password,
        Err(failure) => {
            eprintln!("{}", failure.message());
            return ExitCode::from(failure.exit_code());
        }
    };

    match init::run(&args, &password).await {
        Ok(outcome) => {
            println!(
                "首次 owner 已初始化：login={} id={} 密码散列=Argon2id（密码与散列均未输出）",
                outcome.login,
                outcome.id.to_hex()
            );
            println!("数据库：{}", outcome.database.display());
            println!("服务端启动后即可用该登录名与密码登录（会话与刷新凭据属 S043）。");
            ExitCode::from(EXIT_OK)
        }
        Err(failure) => {
            eprintln!("{}", failure.message());
            ExitCode::from(failure.exit_code())
        }
    }
}

/// 解析命令行：只认 `init`、`--state-dir`、`--login` 与 `--help`，其余一律用法错误（不猜选项）。
///
/// 特别注意没有 `--password`：密码出现在 argv 里会留在进程列表与 shell 历史中，所以这里把它当作
/// 未知参数拒绝，而不是“读一下也无妨”。
fn parse_args(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.peekable();
    let mut subcommand: Option<String> = None;
    let mut state_dir: Option<PathBuf> = None;
    let mut login: Option<String> = None;
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        match text.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--state-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--state-dir 需要给出目录路径".to_owned())?;
                if state_dir.is_some() {
                    return Err("--state-dir 只能给出一次".to_owned());
                }
                state_dir = Some(PathBuf::from(value));
            }
            "--login" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--login 需要给出登录名".to_owned())?;
                if login.is_some() {
                    return Err("--login 只能给出一次".to_owned());
                }
                login = Some(value.to_string_lossy().into_owned());
            }
            other if other.starts_with('-') => {
                return Err(format!("未知参数：{other}"));
            }
            other => {
                if subcommand.is_some() {
                    return Err(format!("多余的参数：{other}"));
                }
                if other != "init" {
                    return Err(format!("未知子命令：{other}"));
                }
                subcommand = Some(other.to_owned());
            }
        }
    }

    match subcommand.as_deref() {
        Some("init") => {
            let state_dir = state_dir.ok_or_else(|| "init 需要 --state-dir".to_owned())?;
            Ok(Command::Init(InitArgs {
                state_dir,
                login: login.unwrap_or_else(|| DEFAULT_LOGIN.to_owned()),
            }))
        }
        _ => Err("缺少子命令".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> impl Iterator<Item = OsString> {
        let owned: Vec<OsString> = items.iter().map(OsString::from).collect();
        owned.into_iter()
    }

    #[test]
    fn init_takes_state_dir_and_defaults_the_login() {
        match parse_args(args(&["init", "--state-dir", "/srv/vaultsyncd"])) {
            Ok(Command::Init(args)) => {
                assert_eq!(args.state_dir, PathBuf::from("/srv/vaultsyncd"));
                assert_eq!(args.login, DEFAULT_LOGIN);
            }
            _ => panic!("应解析出 init 参数"),
        }
    }

    #[test]
    fn explicit_login_is_kept_verbatim() {
        match parse_args(args(&["init", "--state-dir", "/srv/x", "--login", "Owner"])) {
            Ok(Command::Init(args)) => assert_eq!(args.login, "Owner"),
            _ => panic!("应解析出显式登录名"),
        }
    }

    #[test]
    fn missing_state_dir_and_unknown_arguments_are_usage_errors() {
        assert!(parse_args(args(&["init"])).is_err());
        assert!(parse_args(args(&[])).is_err());
        assert!(parse_args(args(&["reinit", "--state-dir", "/x"])).is_err());
        assert!(parse_args(args(&["init", "--state-dir"])).is_err());
        assert!(
            parse_args(args(&[
                "init",
                "--state-dir",
                "/x",
                "--password",
                "hunter2"
            ]))
            .is_err(),
            "密码不接受命令行参数"
        );
    }

    #[test]
    fn help_needs_no_arguments() {
        assert!(matches!(parse_args(args(&["--help"])), Ok(Command::Help)));
        assert!(matches!(parse_args(args(&["-h"])), Ok(Command::Help)));
    }
}
