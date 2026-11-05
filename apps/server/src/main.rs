//! `vaultsyncd` 服务端进程入口（design §5.3：入口只做启动、信号与配置加载）。
//!
//! 这里不做业务判断：配置校验在 [`vaultsyncd_server::config`]，依赖组装与启动顺序在
//! [`vaultsyncd_server::bootstrap`]，恢复结论在 [`vaultsyncd_server::recovery`]。
//!
//! 退出码：
//!
//! | 码 | 含义 |
//! | --- | --- |
//! | 0 | 正常停止（收到 SIGTERM/SIGINT，完成优雅停止） |
//! | 1 | 启动或运行期失败（数据库、恢复读不出结论、监听绑定） |
//! | 2 | 用法或配置错误（参数不对、配置读不出或不合法） |

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info};

use vaultsyncd_server::config::{CONFIG_ENV, Config, DEFAULT_CONFIG_PATH};
use vaultsyncd_server::{EXIT_CONFIG, EXIT_FAILURE, EXIT_OK, bootstrap, logging};

const USAGE: &str = "\
用法：vaultsyncd-server [--config <路径>]

  --config <路径>   TOML 配置文件的路径（默认取环境变量 VAULTSYNCD_CONFIG，
                    再退到 /etc/vaultsyncd/vaultsyncd.toml）
  -h, --help        打印本说明

服务端只在 Linux 上运行；启动阶段先打开数据库并核对启动恢复，恢复没有完成时进程仍提供健康探测，
但不开放写入。收到 SIGTERM 或 SIGINT 后进入 draining 并优雅停止。";

/// 命令行解析结果。
enum Command {
    Run(PathBuf),
    Help,
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
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
    let command = match parse_args(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let config_path = match command {
        Command::Help => {
            println!("{USAGE}");
            return ExitCode::from(EXIT_OK);
        }
        Command::Run(path) => path,
    };

    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("配置不可用（{}）：{error}", error.code());
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    if let Err(error) = logging::init(config.log) {
        eprintln!("{error}");
        return ExitCode::from(EXIT_CONFIG);
    }
    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %config_path.display(),
        listen = %config.listen,
        state_dir = %config.state_dir.display(),
        "vaultsyncd-server 启动"
    );

    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(source) => {
            error!(error = %source, "无法注册 SIGTERM 处理，拒绝在没有优雅停止能力的情况下启动");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(stream) => stream,
        Err(source) => {
            error!(error = %source, "无法注册 SIGINT 处理，拒绝在没有优雅停止能力的情况下启动");
            return ExitCode::from(EXIT_FAILURE);
        }
    };

    let service = match bootstrap::prepare(config).await {
        Ok(service) => service,
        Err(error) => {
            error!(error = %error, "启动失败");
            return ExitCode::from(EXIT_FAILURE);
        }
    };

    let shutdown = async move {
        tokio::select! {
            _ = terminate.recv() => info!(signal = "SIGTERM", "收到停止信号"),
            _ = interrupt.recv() => info!(signal = "SIGINT", "收到停止信号"),
        }
    };

    match service.serve(shutdown).await {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(error) => {
            error!(error = %error, "运行失败");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

/// 解析命令行：只认 `--config` 与 `--help`，其余一律按用法错误处理（不猜选项）。
fn parse_args(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut config: Option<PathBuf> = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        match text.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--config 需要给出配置文件路径".to_owned())?;
                if config.is_some() {
                    return Err("--config 只能给出一次".to_owned());
                }
                config = Some(PathBuf::from(value));
            }
            other => {
                if let Some(value) = other.strip_prefix("--config=") {
                    if value.is_empty() {
                        return Err("--config= 后面需要配置文件路径".to_owned());
                    }
                    if config.is_some() {
                        return Err("--config 只能给出一次".to_owned());
                    }
                    config = Some(PathBuf::from(value));
                } else {
                    return Err(format!("未知参数：{other}"));
                }
            }
        }
    }

    let path = config
        .or_else(|| std::env::var_os(CONFIG_ENV).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    Ok(Command::Run(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> impl Iterator<Item = OsString> {
        let owned: Vec<OsString> = items.iter().map(OsString::from).collect();
        owned.into_iter()
    }

    #[test]
    fn explicit_config_path_wins() {
        match parse_args(args(&["--config", "/tmp/a.toml"])) {
            Ok(Command::Run(path)) => assert_eq!(path, PathBuf::from("/tmp/a.toml")),
            _ => panic!("应解析出 --config 路径"),
        }
        match parse_args(args(&["--config=/tmp/b.toml"])) {
            Ok(Command::Run(path)) => assert_eq!(path, PathBuf::from("/tmp/b.toml")),
            _ => panic!("应解析出 --config= 路径"),
        }
    }

    #[test]
    fn help_does_not_need_config() {
        assert!(matches!(parse_args(args(&["--help"])), Ok(Command::Help)));
        assert!(matches!(parse_args(args(&["-h"])), Ok(Command::Help)));
    }

    #[test]
    fn unknown_and_incomplete_options_are_usage_errors() {
        assert!(parse_args(args(&["--verbose"])).is_err());
        assert!(parse_args(args(&["--config"])).is_err());
        assert!(parse_args(args(&["--config="])).is_err());
        assert!(parse_args(args(&["--config", "a", "--config", "b"])).is_err());
    }
}
