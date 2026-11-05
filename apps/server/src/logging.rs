//! 结构化日志初始化（design §4.1「日志 | tracing」、§18.3）。
//!
//! 输出到 stderr，由 systemd/journald 收集；正常日志不记录文件内容与凭据，配置文件只记录路径，
//! 请求级字段（request_id 等）由各处理器用 `tracing` 的 span/字段补上。

use std::io::IsTerminal;

use crate::config::{LogConfig, LogFormat};

/// 日志初始化失败（全局 subscriber 已被设置过，或其他订阅者注册失败）。
#[derive(Debug)]
pub struct LogInitError(String);

impl std::fmt::Display for LogInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "日志初始化失败：{}", self.0)
    }
}

impl std::error::Error for LogInitError {}

/// 按配置安装全局日志 subscriber。
///
/// # Errors
///
/// 全局 subscriber 已经安装过（例如在同一进程里初始化两次）时返回 [`LogInitError`]。
pub fn init(config: LogConfig) -> Result<(), LogInitError> {
    // 管道/文件（systemd journal、验收脚本、容器日志）不写 ANSI 转义：日志被采集或断言时
    // 必须是可解析的纯文本；只有交互终端才上色。
    let builder = tracing_subscriber::fmt()
        .with_max_level(config.level)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr);
    let result = match config.format {
        LogFormat::Text => builder.try_init(),
        LogFormat::Json => builder.json().try_init(),
    };
    result.map_err(|error| LogInitError(error.to_string()))
}
