//! 服务端配置：TOML 文件的加载与严格校验（design §5.10、§17.2）。
//!
//! 配置是运维输入，不是可信输入：未知键、未知日志级别、不能解析的监听地址、不存在或不在
//! 绝对路径上的状态目录一律拒绝启动，不做“用一个默认值兜过去”的处理。默认值只有一处：
//! 没有 `[server]` 段时监听 `127.0.0.1:8721`——回环地址加独立端口，既能本机开发，也不会
//! 因为忘记配置就把服务暴露到网卡上（design §17.2 的“任何仅供开发的例外必须显式限制在
//! 测试环境”由此落实为默认值而不是文档约定）。

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 开发默认监听地址：只监听回环地址，端口与常见开发服务分开。
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8721";

/// 状态目录里的数据库文件名（design §5.10 的 `server.sqlite`）。
pub const DATABASE_FILE_NAME: &str = "server.sqlite";

/// 配置环境变量：设置后与 `--config` 等价，命令行参数优先。
pub const CONFIG_ENV: &str = "VAULTSYNCD_CONFIG";

/// 系统级配置的默认位置（design §5.10：示例为 `/etc/vaultsyncd/vaultsyncd.toml`）。
pub const DEFAULT_CONFIG_PATH: &str = "/etc/vaultsyncd/vaultsyncd.toml";

/// 日志格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    /// 单行文本，便于人读与 `journalctl`。
    Text,
    /// 每行一个 JSON 对象，便于采集（design §18.3 的结构化字段）。
    Json,
}

impl LogFormat {
    /// 稳定短名（配置值与日志共用）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            LogFormat::Text => "text",
            LogFormat::Json => "json",
        }
    }
}

/// 校验后的日志配置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogConfig {
    /// 最低输出级别。
    pub level: tracing::Level,
    /// 输出格式。
    pub format: LogFormat,
}

/// 校验后的服务端配置。启动路径只使用这个类型，不再回头读 TOML 文本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// HTTP 监听地址。
    pub listen: SocketAddr,
    /// 内部状态目录（绝对路径，启动时必须已存在）。
    pub state_dir: PathBuf,
    /// 服务端数据库文件的绝对路径（`state_dir/server.sqlite`）。
    pub database: PathBuf,
    /// 日志配置。
    pub log: LogConfig,
}

/// 配置加载失败。
#[derive(Debug)]
pub enum ConfigError {
    /// 配置文件读不出来（缺失、权限、非 UTF-8）。
    Io {
        /// 尝试读取的路径。
        path: PathBuf,
        /// 原始 I/O 错误。
        source: std::io::Error,
    },
    /// TOML 语法错误或出现未知键。
    Parse {
        /// 配置文件路径。
        path: PathBuf,
        /// 解析器给出的原因（含未知键名）。
        detail: String,
    },
    /// 字段值不合法。
    InvalidValue {
        /// 字段名（配置里的写法，例如 `server.listen`）。
        field: &'static str,
        /// 具体原因。
        detail: String,
    },
}

impl ConfigError {
    /// 稳定错误码（日志与验收脚本共用）。
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            ConfigError::Io { .. } => "config_unreadable",
            ConfigError::Parse { .. } => "config_invalid",
            ConfigError::InvalidValue { .. } => "config_invalid_value",
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, source } => {
                write!(f, "读不出配置文件 {}：{source}", path.display())
            }
            ConfigError::Parse { path, detail } => {
                write!(f, "配置文件 {} 不合法：{detail}", path.display())
            }
            ConfigError::InvalidValue { field, detail } => {
                write!(f, "配置项 {field} 不合法：{detail}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// TOML 的原始形状：只在这里出现，校验后转成 [`Config`]。
///
/// `deny_unknown_fields` 是有意的：拼错的键（例如把 `listen` 写成 `listne`）必须报错，
/// 否则服务会带着默认回环地址启动，运维以为改过配置却没有任何效果。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    state: RawState,
    #[serde(default)]
    log: RawLog,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    #[serde(default = "default_listen")]
    listen: String,
}

impl Default for RawServer {
    fn default() -> Self {
        // `#[serde(default)]` 用 `Default` 补齐整个 `[server]` 段，因此这里必须给出同一份
        // 回环默认值，不能让派生出来的空字符串在“没有 `[server]` 段”时变成非法监听地址。
        RawServer {
            listen: default_listen(),
        }
    }
}

fn default_listen() -> String {
    DEFAULT_LISTEN.to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawState {
    dir: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawLog {
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    format: Option<String>,
}

impl Config {
    /// 从文件加载配置。
    ///
    /// # Errors
    ///
    /// 见 [`ConfigError`]：文件读不出、TOML 语法或未知键、字段值不合法。
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, path)
    }

    /// 解析配置文本。`path` 只用于错误信息，不读文件（测试与 `--check` 复用同一段校验）。
    ///
    /// # Errors
    ///
    /// 见 [`ConfigError`]。
    pub fn parse(text: &str, path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|error| ConfigError::Parse {
            path: path.as_ref().to_path_buf(),
            detail: error.to_string(),
        })?;

        let listen =
            raw.server
                .listen
                .parse::<SocketAddr>()
                .map_err(|error| ConfigError::InvalidValue {
                    field: "server.listen",
                    detail: format!("{} 不是 IP:端口形式：{error}", raw.server.listen),
                })?;

        let state_dir = PathBuf::from(&raw.state.dir);
        if !state_dir.is_absolute() {
            return Err(ConfigError::InvalidValue {
                field: "state.dir",
                detail: format!("{} 不是绝对路径", state_dir.display()),
            });
        }
        // 状态目录必须已经存在：服务端不替运维决定内部目录落在哪个文件系统上，也不在
        // 未经准入检查的路径上创建数据库（design §11.4 的内部目录准入属后续存储卡）。
        let metadata =
            std::fs::metadata(&state_dir).map_err(|source| ConfigError::InvalidValue {
                field: "state.dir",
                detail: format!("{} 不可用（需先创建目录）：{source}", state_dir.display()),
            })?;
        if !metadata.is_dir() {
            return Err(ConfigError::InvalidValue {
                field: "state.dir",
                detail: format!("{} 不是目录", state_dir.display()),
            });
        }
        let database = state_dir.join(DATABASE_FILE_NAME);

        let level = match raw.log.level.as_deref() {
            None => tracing::Level::INFO,
            Some(text) => {
                text.parse::<tracing::Level>()
                    .map_err(|_| ConfigError::InvalidValue {
                        field: "log.level",
                        detail: format!("{text} 不是 trace/debug/info/warn/error"),
                    })?
            }
        };
        let format = match raw.log.format.as_deref() {
            None | Some("text") => LogFormat::Text,
            Some("json") => LogFormat::Json,
            Some(other) => {
                return Err(ConfigError::InvalidValue {
                    field: "log.format",
                    detail: format!("{other} 不是 text 或 json"),
                });
            }
        };

        Ok(Config {
            listen,
            state_dir,
            database,
            log: LogConfig { level, format },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vaultsyncd-s041-config-{}-{label}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("创建临时目录");
        path
    }

    fn valid_text(dir: &Path) -> String {
        format!(
            "[server]\nlisten = \"127.0.0.1:8721\"\n\n[state]\ndir = \"{}\"\n\n[log]\nlevel = \"debug\"\nformat = \"json\"\n",
            dir.display()
        )
    }

    #[test]
    fn valid_config_yields_typed_values_and_derived_database_path() {
        let dir = temp_dir("valid");
        let config = Config::parse(&valid_text(&dir), "test.toml").expect("解析配置");
        assert_eq!(config.listen, "127.0.0.1:8721".parse().unwrap());
        assert_eq!(config.state_dir, dir);
        assert_eq!(config.database, dir.join(DATABASE_FILE_NAME));
        assert_eq!(config.log.level, tracing::Level::DEBUG);
        assert_eq!(config.log.format, LogFormat::Json);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_server_and_log_sections_fall_back_to_loopback_only() {
        let dir = temp_dir("defaults");
        let text = format!("[state]\ndir = \"{}\"\n", dir.display());
        let config = Config::parse(&text, "test.toml").expect("解析配置");
        assert_eq!(config.listen, DEFAULT_LISTEN.parse().unwrap());
        assert!(config.listen.ip().is_loopback(), "默认必须只监听回环地址");
        assert_ne!(config.listen.port(), 80);
        assert_eq!(config.log.level, tracing::Level::INFO);
        assert_eq!(config.log.format, LogFormat::Text);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_key_is_rejected_instead_of_silently_ignored() {
        let dir = temp_dir("unknown");
        let text = format!(
            "[server]\nlistne = \"127.0.0.1:8721\"\n\n[state]\ndir = \"{}\"\n",
            dir.display()
        );
        let error = Config::parse(&text, "test.toml").expect_err("未知键必须报错");
        assert_eq!(error.code(), "config_invalid");
        assert!(error.to_string().contains("listne"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_toml_is_rejected() {
        let error = Config::parse("[state\n", "test.toml").expect_err("语法错误必须报错");
        assert_eq!(error.code(), "config_invalid");
    }

    #[test]
    fn state_dir_must_be_absolute_and_existing() {
        let relative =
            Config::parse("[state]\ndir = \"state\"\n", "test.toml").expect_err("相对路径必须报错");
        assert_eq!(relative.code(), "config_invalid_value");
        assert!(relative.to_string().contains("绝对路径"), "{relative}");

        let missing = Config::parse("[state]\ndir = \"/nonexistent/s041-state\"\n", "test.toml")
            .expect_err("不存在的目录必须报错");
        assert!(missing.to_string().contains("不可用"), "{missing}");

        let file =
            std::env::temp_dir().join(format!("vaultsyncd-s041-notdir-{}", std::process::id()));
        std::fs::write(&file, b"x").expect("写临时文件");
        let not_dir = Config::parse(
            &format!("[state]\ndir = \"{}\"\n", file.display()),
            "test.toml",
        )
        .expect_err("普通文件不能当状态目录");
        assert!(not_dir.to_string().contains("不是目录"), "{not_dir}");
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn listen_must_be_ip_and_port() {
        let dir = temp_dir("listen");
        for value in ["example.com:80", "127.0.0.1", "0.0.0.0:70000"] {
            let text = format!(
                "[server]\nlisten = \"{value}\"\n\n[state]\ndir = \"{}\"\n",
                dir.display()
            );
            let error = Config::parse(&text, "test.toml").expect_err("非法监听地址必须被拒绝");
            assert_eq!(error.code(), "config_invalid_value");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn log_level_and_format_are_closed_sets() {
        let dir = temp_dir("log");
        for (section, needle) in [
            ("level = \"loud\"", "log.level"),
            ("format = \"xml\"", "log.format"),
        ] {
            let text = format!("[state]\ndir = \"{}\"\n\n[log]\n{section}\n", dir.display());
            let error = Config::parse(&text, "test.toml").expect_err("非法日志配置必须报错");
            assert!(
                error.to_string().contains(needle),
                "错误信息应指出 {needle}：{error}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_reports_the_path() {
        let error = Config::load("/nonexistent/s041/vaultsyncd.toml").expect_err("文件不存在");
        assert_eq!(error.code(), "config_unreadable");
        assert!(error.to_string().contains("/nonexistent/s041"), "{error}");
    }
}
