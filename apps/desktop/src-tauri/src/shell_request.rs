//! Fixed Explorer verbs. Selected names are data, never an executable or script.
use std::{ffi::OsString, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Action {
    History,
    Download,
    Release,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShellRequest {
    pub action: Action,
    pub path: PathBuf,
}

pub(crate) fn parse(args: &[OsString]) -> Result<ShellRequest, &'static str> {
    if args.len() != 4 || args[0] != "--shell-action" || args[2] != "--" {
        return Err("右键操作参数无效。");
    }
    let action = match args[1].to_str() {
        Some("history") => Action::History,
        Some("download") => Action::Download,
        Some("release") => Action::Release,
        _ => return Err("不支持此右键操作。"),
    };
    let path = args[3].to_str().ok_or("文件路径不是有效 Unicode。")?;
    if path.is_empty() || path.contains(['\0', '\r', '\n', '"']) {
        return Err("文件路径无效。");
    }
    Ok(ShellRequest {
        action,
        path: PathBuf::from(path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }
    #[test]
    fn selected_names_are_one_literal_argument() {
        let path = r"C:\同步目录\空 格 & %PATH% '中文.txt";
        let request = parse(&args(&["--shell-action", "history", "--", path])).unwrap();
        assert_eq!(request.action, Action::History);
        assert_eq!(request.path, PathBuf::from(path));
        for action in ["download", "release"] {
            assert!(parse(&args(&["--shell-action", action, "--", path])).is_ok());
        }
    }
    #[test]
    fn unknown_verbs_and_extra_arguments_never_execute() {
        for values in [
            vec![],
            vec!["--shell-action", "exec", "--", "file"],
            vec!["--shell-action", "release", "file"],
            vec!["--shell-action", "release", "--", "file", "other"],
            vec!["--shell-action", "history", "--", "bad\npath"],
            vec!["--shell-action", "history", "--", ""],
        ] {
            assert!(parse(&args(&values)).is_err());
        }
    }
}
