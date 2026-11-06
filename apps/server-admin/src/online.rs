//! 在线命令只连接受限 socket；不打开 SQLite、不读取密码或创建长期会话。

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use vaultsyncd_platform_linux_server::ipc;

pub const USAGE: &str =
    "在线用法：vaultsyncd-admin <命令> --socket <状态目录/admin/socket> [--json]
  status                              服务生命周期与 Root 数量
  roots [页号]                        Root 身份与健康清单
  roots register <绝对路径>           复用服务端真实 Root 注册准入
  problems [页号]                     Root 健康、持久冲突与启动恢复问题
  history <RootId> <EntryId> [页号]    已提交版本（包含已删除条目）
  trash <RootId> [页号]               回收站清单、到期时间与版本身份
分页命令每页最多 100 条，页号默认 0；next_page 为 null 时结束。
只允许服务进程同一有效 UID（可通过 sudo -u <服务账号> 运行），不接受凭据参数。
远程管理使用 SSH 进入服务主机运行本命令，或使用已认证 HTTPS API，不支持明文远程 socket。";

pub async fn run(args: Vec<OsString>) -> u8 {
    match parse(args) {
        Ok((socket, command, json)) => match exchange(socket, command).await {
            Ok(value) => {
                if json {
                    println!("{value}");
                } else {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&value).expect("JSON Value 可序列化")
                    );
                }
                if value["status"].as_u64().is_some_and(|status| status < 400) {
                    0
                } else {
                    1
                }
            }
            Err(error) => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"status":503,"error":{"code":"ADMIN_UNAVAILABLE","message":error.to_string()}})
                    );
                } else {
                    eprintln!("在线管理失败：{error}");
                }
                1
            }
        },
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            2
        }
    }
}

fn parse(args: Vec<OsString>) -> Result<(PathBuf, Vec<String>, bool), String> {
    let mut args = args.into_iter();
    let mut socket = None;
    let mut command = vec!["v1".to_owned()];
    let mut json = false;
    while let Some(arg) = args.next() {
        let text = arg.into_string().map_err(|_| "参数不是 UTF-8")?;
        match text.as_str() {
            "--socket" if socket.is_none() => {
                socket = Some(PathBuf::from(args.next().ok_or("--socket 缺少路径")?))
            }
            "--json" if !json => json = true,
            option if option.starts_with('-') => return Err(format!("未知或重复参数：{option}")),
            _ => command.push(text),
        }
    }
    let socket = socket.ok_or("在线管理需要 --socket")?;
    if !socket.is_absolute() {
        return Err("--socket 必须是绝对路径".to_owned());
    }
    let valid = match command.get(1).map(String::as_str) {
        Some("status") => command.len() == 2,
        Some("roots") => {
            (2..=3).contains(&command.len()) || (command.len() == 4 && command[2] == "register")
        }
        Some("problems") => (2..=3).contains(&command.len()),
        Some("trash") => (3..=4).contains(&command.len()),
        Some("history") => (4..=5).contains(&command.len()),
        _ => false,
    };
    if !valid {
        return Err("命令或参数数量不匹配".to_owned());
    }
    Ok((socket, command, json))
}

async fn exchange(socket: PathBuf, command: Vec<String>) -> std::io::Result<Value> {
    tokio::time::timeout(Duration::from_secs(15), async move {
        let mut stream = ipc::connect(&socket).await?;
        ipc::write_frame(
            &mut stream,
            &serde_json::to_vec(&command).map_err(std::io::Error::other)?,
        )
        .await?;
        let bytes = ipc::read_frame(&mut stream).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
        if !value["status"]
            .as_u64()
            .is_some_and(|status| (100..=599).contains(&status))
            || (value.get("result").is_none() == value.get("error").is_none())
            || (value["status"].as_u64().is_some_and(|status| status < 400)
                != value.get("result").is_some())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "非法管理响应",
            ));
        }
        Ok(value)
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "管理请求超时；修改结果未知，请查询 roots 核对，勿直接重放",
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }
    #[test]
    fn online_parser_is_strict_and_never_accepts_credentials() {
        assert!(
            parse(args(&[
                "roots",
                "register",
                "/data/root",
                "--socket",
                "/run/admin/socket",
                "--json"
            ]))
            .is_ok()
        );
        for input in [
            vec!["status"],
            vec!["status", "--socket", "relative"],
            vec!["status", "--socket", "/x", "--password", "secret"],
            vec!["history", "--socket", "/x"],
            vec!["roots", "delete", "/x", "--socket", "/s"],
        ] {
            assert!(parse(args(&input)).is_err());
        }
    }
}
